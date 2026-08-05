use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Gauge, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::config::Config;
use crate::plan::{human_bytes, ArchivePlan, MediaInfo, Payload};
use crate::runner::{self, Ack, BurnParams, BurnRequest, RunReport, RunnerCtx, Stage, StageEvent};
use crate::tools::Tools;

pub(crate) const ACCENT: Color = Color::Cyan;
pub(crate) const OK: Color = Color::Green;
pub(crate) const WARN: Color = Color::Yellow;
pub(crate) const ERR: Color = Color::Red;
pub(crate) const DIM: Color = Color::DarkGray;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOG_CAP: usize = 200;
/// A burn confirm must be a deliberate keypress on a prompt the user has
/// seen: Proceed is ignored until the prompt has been on screen this long
/// (type-ahead Enter — double-tap, held key — must never answer it).
const ACK_ARM_DELAY: Duration = Duration::from_millis(500);

const STAGE_ORDER: [Stage; 9] = [
    Stage::Preflight,
    Stage::Parity,
    Stage::Checksums,
    Stage::Format,
    Stage::Master,
    Stage::Burn,
    Stage::VerifyImage,
    Stage::VerifyFiles,
    Stage::CheckMedia,
];

/// Ratatui wizard: probe + plan screen (Enter = burn, q = quit), live pipeline
/// screen (stage gauges + event log), report screen. Runs the pipeline on a
/// worker thread; renders StageEvents; answers NeedAck prompts through the ack
/// channel.
pub fn run(cfg: Config, tools: Tools, req: BurnRequest) -> Result<()> {
    let req = BurnRequest { amend: true, ..req };
    let payloads = payload_rows(&req.payloads);
    let params = BurnParams::resolve(&cfg, &req);
    let (tx, rx) = mpsc::channel::<StageEvent>();
    let (ack_tx, ack_rx) = mpsc::channel::<Ack>();
    let ctx = RunnerCtx {
        cfg: cfg.clone(),
        tools,
        tx,
        ack_rx,
    };
    let worker = std::thread::spawn(move || runner::run_burn(&ctx, &req));
    // SIGTERM/SIGHUP (terminal closed, `systemctl stop`) arrive as signals, not
    // key events; the loop polls this so they trigger the same clean shutdown
    // as Ctrl-C. (Ctrl-C itself reaches the loop as a key in raw mode.)
    let stop = crate::shutdown::install();

    let mut terminal = ratatui::init();
    // keys queued before this screen existed (shell autorepeat, the picker's
    // confirm Enter) must not leak into the wizard
    flush_input();
    let mut app = App::new(cfg, payloads, params, ack_tx);
    let loop_result = app.event_loop(&mut terminal, &rx, &stop);
    ratatui::restore();

    // A UI-loop error (e.g. a draw failure) while the burn is still live must
    // not orphan the tool: shut it down before surfacing the error.
    if loop_result.is_err() {
        if !worker.is_finished() {
            crate::shutdown::escalate(|| worker.is_finished(), &app.ack_tx);
            let _ = worker.join();
        }
        return loop_result;
    }

    if worker.is_finished() || app.disconnected {
        return match worker.join() {
            Ok(res) => res,
            Err(_) => anyhow::bail!("pipeline thread panicked"),
        };
    }
    if let Some(msg) = app.failure {
        anyhow::bail!("{msg}");
    }
    if app.force_quit {
        crate::shutdown::escalate(|| worker.is_finished(), &app.ack_tx);
        let _ = worker.is_finished().then(|| worker.join());
        anyhow::bail!(
            "interrupted; running tool terminated - the disc in the drive may be \
             partially written and must not be trusted without a verify run"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Plan,
    Run,
    Report,
}

/// Plan-screen rows, in display order; indices drive `App::selected`.
const PARAM_ROWS: usize = 6;
const ROW_LABEL: usize = 0;
const ROW_SPEED: usize = 1;
const ROW_REDUNDANCY: usize = 2;
const ROW_PARITY: usize = 3;
const ROW_DEFECT_MGMT: usize = 4;
const ROW_STAGING: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditField {
    Label,
    Speed,
    Staging,
}

#[derive(Debug, Clone)]
enum StageState {
    Pending,
    Running { pct: Option<f32> },
    Done,
    Failed,
}

struct PayloadRow {
    name: String,
    size_text: String,
    container: bool,
}

struct EventLog {
    lines: VecDeque<(bool, String)>,
}

impl EventLog {
    fn new() -> Self {
        Self {
            lines: VecDeque::new(),
        }
    }

    fn push(&mut self, warn: bool, text: String) {
        if self.lines.len() == LOG_CAP {
            self.lines.pop_front();
        }
        self.lines.push_back((warn, text));
    }

    fn tail(&self, n: usize) -> impl Iterator<Item = &(bool, String)> {
        self.lines.iter().skip(self.lines.len().saturating_sub(n))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lines.len()
    }
}

struct App {
    cfg: Config,
    payloads: Vec<PayloadRow>,
    ack_tx: Sender<Ack>,
    screen: Screen,
    /// Drive the runner actually selected (may differ from cfg.device when
    /// auto-detection picked another drive).
    device: Option<String>,
    media: Option<MediaInfo>,
    plan: Option<ArchivePlan>,
    /// Runner-canonical params: seeded from BurnParams::resolve, overwritten
    /// by every Plan event — always exactly what the runner holds.
    params: BurnParams,
    /// Local pending edits shown until the runner echoes them back.
    edit: Option<BurnParams>,
    /// `edit` has changes not yet sent (no ack slot was free).
    edit_dirty: bool,
    /// An Amend is in flight; awaiting the fresh Plan + NeedAck.
    replanning: bool,
    selected: usize,
    editing: Option<EditField>,
    input: String,
    stages: Vec<(Stage, StageState)>,
    current: Option<(Stage, String)>,
    log: EventLog,
    pending_ack: Option<String>,
    /// First render instant of the first prompt; Proceed stays dead before
    /// it and for ACK_ARM_DELAY after it. Never reset: later prompts belong
    /// to a user already at the screen.
    ack_shown: Option<Instant>,
    report: Option<RunReport>,
    report_scroll: u16,
    failure: Option<String>,
    aborted: bool,
    disconnected: bool,
    quit: bool,
    force_quit: bool,
    run_started: Option<Instant>,
    tick: u64,
}

impl App {
    fn new(
        cfg: Config,
        payloads: Vec<PayloadRow>,
        params: BurnParams,
        ack_tx: Sender<Ack>,
    ) -> Self {
        Self {
            cfg,
            payloads,
            ack_tx,
            screen: Screen::Plan,
            device: None,
            media: None,
            plan: None,
            params,
            edit: None,
            edit_dirty: false,
            replanning: false,
            selected: 0,
            editing: None,
            input: String::new(),
            stages: STAGE_ORDER
                .iter()
                .map(|s| (*s, StageState::Pending))
                .collect(),
            current: None,
            log: EventLog::new(),
            pending_ack: None,
            ack_shown: None,
            report: None,
            report_scroll: 0,
            failure: None,
            aborted: false,
            disconnected: false,
            quit: false,
            force_quit: false,
            run_started: None,
            tick: 0,
        }
    }

    fn shown_params(&self) -> &BurnParams {
        self.edit.as_ref().unwrap_or(&self.params)
    }

    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        rx: &Receiver<StageEvent>,
        stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        while !self.quit {
            if crate::shutdown::stopping(stop) {
                self.force_quit = true;
                return Ok(());
            }
            self.drain(rx);
            terminal.draw(|frame| self.render(frame))?;
            if self.pending_ack.is_some() && self.ack_shown.is_none() {
                self.ack_shown = Some(Instant::now());
                flush_input();
            }
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.on_key(key);
                    }
                }
            }
            self.tick = self.tick.wrapping_add(1);
        }
        Ok(())
    }

    fn drain(&mut self, rx: &Receiver<StageEvent>) {
        if self.disconnected {
            return;
        }
        loop {
            match rx.try_recv() {
                Ok(ev) => self.apply(ev),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    if self.aborted {
                        self.quit = true;
                    } else if self.failure.is_none() && self.report.is_none() {
                        self.failure = Some("pipeline thread exited unexpectedly".into());
                        if self.screen == Screen::Plan && self.plan.is_some() {
                            self.screen = Screen::Run;
                        }
                    }
                    break;
                }
            }
        }
    }

    fn apply(&mut self, ev: StageEvent) {
        match ev {
            StageEvent::Plan {
                device,
                media,
                plan,
                params,
            } => {
                self.device = Some(device);
                self.media = Some(media);
                self.plan = Some(plan);
                self.params = params;
                // canonical (sanitized/clamped) wins after a clean round-trip
                if !self.edit_dirty {
                    self.edit = None;
                }
            }
            StageEvent::StageStart(stage) => {
                self.set_stage(stage, StageState::Running { pct: None });
                if stage != Stage::Preflight {
                    self.enter_run();
                }
            }
            StageEvent::Progress { stage, pct, detail } => {
                self.set_stage(stage, StageState::Running { pct });
                self.current = Some((stage, detail));
                if stage != Stage::Preflight {
                    self.enter_run();
                }
            }
            StageEvent::StageDone { stage, summary } => {
                self.set_stage(stage, StageState::Done);
                if self.current.as_ref().is_some_and(|(s, _)| *s == stage) {
                    self.current = None;
                }
                self.log
                    .push(false, format!("{}: {summary}", stage.label()));
            }
            StageEvent::Info(text) => self.log.push(false, text),
            StageEvent::Out(text) => self.log.push(false, text),
            StageEvent::Warn(text) => self.log.push(true, text),
            StageEvent::NeedAck { prompt } => {
                // the runner grants one ack slot per NeedAck: spend it on queued
                // edits first; a mid-run NeedAck must never get a stale Amend
                if self.screen == Screen::Plan && !self.aborted && self.edit_dirty {
                    self.edit_dirty = false;
                    self.replanning = true;
                    let p = self.edit.clone().unwrap_or_else(|| self.params.clone());
                    let _ = self.ack_tx.send(Ack::Amend(p));
                } else {
                    self.pending_ack = Some(prompt);
                    self.replanning = false;
                }
            }
            StageEvent::Finished { report } => {
                self.report = Some(report);
                self.screen = Screen::Report;
            }
            StageEvent::Failed { stage, error } => {
                self.set_stage(stage, StageState::Failed);
                self.failure = Some(format!("{} failed: {error}", stage.label()));
                // a failure before any plan exists (no disc, probe error) is
                // not a pipeline event: the Run screen would read as "it
                // started burning" — show it where the user is looking
                if self.screen == Screen::Plan && self.plan.is_some() {
                    self.screen = Screen::Run;
                }
            }
        }
    }

    fn set_stage(&mut self, stage: Stage, state: StageState) {
        if let Some(slot) = self.stages.iter_mut().find(|(s, _)| *s == stage) {
            slot.1 = state;
        }
    }

    fn enter_run(&mut self) {
        if self.screen == Screen::Plan {
            self.screen = Screen::Run;
        }
        if self.run_started.is_none() {
            self.run_started = Some(Instant::now());
        }
    }

    fn visible_stages(&self) -> Vec<(Stage, StageState)> {
        self.stages
            .iter()
            .filter(|(s, st)| *s != Stage::Format || !matches!(st, StageState::Pending))
            .cloned()
            .collect()
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.force_quit = true;
            self.quit = true;
            return;
        }
        if self.screen == Screen::Plan && !self.aborted && self.plan_key(key) {
            return;
        }
        if self.pending_ack.is_some() {
            match key.code {
                KeyCode::Enter if self.ack_ready() => self.answer(Ack::Proceed),
                // aborting is the safe direction: never delayed
                KeyCode::Char('q') | KeyCode::Esc => self.answer(Ack::Abort),
                _ => {}
            }
            return;
        }
        match self.screen {
            Screen::Plan => {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                    self.force_quit = true;
                    self.quit = true;
                }
            }
            Screen::Run => {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) && self.failure.is_some() {
                    self.quit = true;
                }
            }
            Screen::Report => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.report_scroll = self.report_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.report_scroll = self.report_scroll.saturating_add(1);
                }
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                _ => {}
            },
        }
    }

    /// Plan-screen param editing; true = key consumed. Unconsumed keys fall
    /// through to the ack/quit handling in on_key.
    fn plan_key(&mut self, key: KeyEvent) -> bool {
        if let Some(field) = self.editing {
            match key.code {
                KeyCode::Enter => self.commit_edit(field),
                KeyCode::Esc => {
                    self.editing = None;
                    self.input.clear();
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(c) => match field {
                    EditField::Label => {
                        if self.input.len() < 32 {
                            self.input.push(c);
                        }
                    }
                    EditField::Speed => {
                        if c.is_ascii_digit() && self.input.len() < 3 {
                            self.input.push(c);
                        }
                    }
                    EditField::Staging => {
                        if self.input.len() < 256 {
                            self.input.push(c);
                        }
                    }
                },
                _ => {}
            }
            return true;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(PARAM_ROWS - 1);
                true
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.adjust(-1);
                true
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.adjust(1);
                true
            }
            KeyCode::Char(' ') => {
                if matches!(self.selected, ROW_PARITY | ROW_DEFECT_MGMT) {
                    self.adjust(1);
                }
                true
            }
            KeyCode::Char('e') => {
                self.begin_edit();
                true
            }
            // a non-fitting plan can only be amended or aborted, never confirmed
            KeyCode::Enter if self.plan.as_ref().is_some_and(|p| !p.fits) => true,
            _ => false,
        }
    }

    fn adjust(&mut self, delta: i64) {
        match self.selected {
            ROW_SPEED => {
                let options = self.speed_options();
                let current = self.shown_params().speed;
                let idx = options.iter().position(|o| *o == current).unwrap_or(0) as i64;
                let next = (idx + delta).rem_euclid(options.len() as i64) as usize;
                let value = options[next];
                self.amend_with(|p| p.speed = value);
            }
            ROW_REDUNDANCY => {
                let next = (self.shown_params().redundancy_pct as i64 + delta).clamp(1, 100) as u32;
                self.amend_with(|p| p.redundancy_pct = next);
            }
            ROW_PARITY => self.amend_with(|p| p.parity = !p.parity),
            ROW_DEFECT_MGMT => self.amend_with(|p| p.defect_management = !p.defect_management),
            _ => {}
        }
    }

    /// Drive default first, then probed speeds; the shown value is kept in the
    /// cycle so an unprobed (typed) speed still steps sanely.
    fn speed_options(&self) -> Vec<Option<u32>> {
        let mut opts = vec![None];
        if let Some(media) = &self.media {
            for s in &media.speeds {
                let v = Some(s.round() as u32);
                if !opts.contains(&v) {
                    opts.push(v);
                }
            }
        }
        let current = self.shown_params().speed;
        if !opts.contains(&current) {
            opts.push(current);
        }
        opts
    }

    fn begin_edit(&mut self) {
        let field = match self.selected {
            ROW_LABEL => EditField::Label,
            ROW_SPEED => EditField::Speed,
            ROW_STAGING => EditField::Staging,
            _ => return,
        };
        self.input = match field {
            EditField::Label => self.shown_params().label.clone(),
            EditField::Speed => self
                .shown_params()
                .speed
                .map(|s| s.to_string())
                .unwrap_or_default(),
            EditField::Staging => self.shown_params().staging.display().to_string(),
        };
        self.editing = Some(field);
    }

    fn commit_edit(&mut self, field: EditField) {
        let input = std::mem::take(&mut self.input);
        self.editing = None;
        match field {
            EditField::Label => self.amend_with(|p| p.label = input),
            EditField::Speed => {
                let speed = input.parse::<u32>().ok().filter(|s| *s > 0);
                self.amend_with(|p| p.speed = speed);
            }
            EditField::Staging => {
                let trimmed = input.trim();
                let staging = if trimmed.is_empty() {
                    self.cfg.staging.clone()
                } else {
                    PathBuf::from(trimmed)
                };
                self.amend_with(|p| p.staging = staging);
            }
        }
    }

    /// One Amend per granted ack slot: send now if a NeedAck is pending,
    /// otherwise queue locally; queued edits coalesce and flush on the next
    /// NeedAck (see apply). Invariant: pending_ack.is_some() => !edit_dirty.
    fn amend_with(&mut self, f: impl FnOnce(&mut BurnParams)) {
        let mut p = self.edit.clone().unwrap_or_else(|| self.params.clone());
        f(&mut p);
        if p == *self.shown_params() {
            return;
        }
        self.edit = Some(p.clone());
        if self.pending_ack.take().is_some() {
            self.edit_dirty = false;
            self.replanning = true;
            let _ = self.ack_tx.send(Ack::Amend(p));
        } else {
            self.edit_dirty = true;
        }
    }

    fn ack_ready(&self) -> bool {
        self.ack_shown
            .is_some_and(|shown| shown.elapsed() >= ACK_ARM_DELAY)
    }

    fn answer(&mut self, ack: Ack) {
        self.pending_ack = None;
        match &ack {
            Ack::Proceed => {
                if self.screen == Screen::Plan && self.plan.is_some() {
                    self.enter_run();
                }
            }
            Ack::Abort => self.aborted = true,
            Ack::Amend(_) => {}
        }
        let _ = self.ack_tx.send(ack);
    }

    // -- rendering ---------------------------------------------------------

    fn render(&self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        frame.render_widget(Paragraph::new(app_title()), pad(header));

        match self.screen {
            Screen::Plan => self.plan_screen(frame, body),
            Screen::Run => self.run_screen(frame, body),
            Screen::Report => self.report_screen(frame, body),
        }

        frame.render_widget(
            Paragraph::new(Span::styled(self.footer_hint(), Style::new().fg(DIM))),
            pad(footer),
        );

        if self.screen != Screen::Plan {
            if let Some(prompt) = &self.pending_ack {
                ack_modal(frame, body, prompt);
            }
        }
    }

    fn footer_hint(&self) -> String {
        if self.aborted {
            return "aborting…".into();
        }
        if self.screen == Screen::Plan && self.editing.is_some() {
            return "type value · Enter commit · Esc cancel".into();
        }
        if self.pending_ack.is_some() {
            return match self.screen {
                Screen::Plan => {
                    "↑↓ select · ←→ adjust · Space toggle · e edit · Enter burn · q abort".into()
                }
                _ => "Enter proceed · q/Esc abort".into(),
            };
        }
        match self.screen {
            Screen::Plan if self.failure.is_some() => "q quit".into(),
            Screen::Plan if self.replanning || self.edit_dirty => "re-planning…".into(),
            Screen::Plan => "probing… · q quit".into(),
            Screen::Run => {
                let elapsed = self
                    .run_started
                    .map(|t| format_elapsed(t.elapsed()))
                    .unwrap_or_else(|| "00:00:00".into());
                if self.failure.is_some() {
                    format!("elapsed {elapsed} · q quit")
                } else {
                    format!("elapsed {elapsed} · Ctrl-C force-quit")
                }
            }
            Screen::Report => "↑↓ scroll · q quit".into(),
        }
    }

    fn plan_screen(&self, frame: &mut Frame, area: Rect) {
        let block = body_block("Plan");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let (Some(media), Some(plan)) = (&self.media, &self.plan) else {
            self.probing_view(frame, inner);
            return;
        };

        let mut lines = vec![Line::from("")];
        let mut media_text = format!(
            "{} — {} free",
            media.kind.label(),
            human_bytes(media.free_bytes)
        );
        if let Some(id) = &media.media_id {
            media_text.push_str(&format!("  ({id})"));
        }
        lines.push(kv("Media  ", &media_text));
        lines.push(kv(
            "Device ",
            self.device.as_deref().unwrap_or(&self.cfg.device),
        ));
        lines.push(Line::from(""));
        lines.push(heading("Payload"));
        for row in &self.payloads {
            let tag = if row.container { "  container" } else { "" };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {:<40}", row.name),
                    Style::new().fg(Color::White),
                ),
                Span::styled(
                    format!("{:>12}", row.size_text),
                    Style::new().fg(Color::Gray),
                ),
                Span::styled(tag, Style::new().fg(WARN)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(heading("Parameters"));
        for line in self.param_lines(media, plan) {
            lines.push(line);
        }
        lines.push(Line::from(""));
        lines.push(kv(
            "Total  ",
            &format!(
                "{} of {} budget ({}% headroom off {} capacity)",
                human_bytes(plan.total_bytes_est),
                human_bytes(plan.budget),
                self.cfg.headroom_pct,
                human_bytes(plan.capacity)
            ),
        ));
        if !plan.fits {
            lines.push(Line::from(Span::styled(
                "  DOES NOT FIT — lower redundancy, disable parity, or use larger media",
                Style::new().fg(ERR).add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(""));

        let [info_area, gauge_area, warn_area, prompt_area] = Layout::vertical([
            Constraint::Length(lines.len() as u16),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(2),
        ])
        .areas(inner);

        frame.render_widget(Paragraph::new(lines), info_area);

        let color = if plan.fits { OK } else { ERR };
        let ratio = fit_ratio(plan.total_bytes_est, plan.budget);
        frame.render_widget(
            Gauge::default()
                .ratio(ratio)
                .gauge_style(Style::new().fg(color).bg(Color::Black))
                .label(Span::styled(
                    format!("{:.0}% of budget", ratio * 100.0),
                    Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                )),
            pad(gauge_area),
        );

        let mut warn_lines = vec![Line::from("")];
        for w in &plan.warnings {
            warn_lines.push(Line::from(Span::styled(
                format!("  ! {w}"),
                Style::new().fg(WARN),
            )));
        }
        frame.render_widget(
            Paragraph::new(warn_lines).wrap(Wrap { trim: false }),
            warn_area,
        );

        let prompt_line = if self.replanning || self.edit_dirty {
            Line::from(Span::styled("  re-planning…", Style::new().fg(DIM)))
        } else if let Some(prompt) = &self.pending_ack {
            if plan.fits {
                Line::from(vec![
                    Span::styled(
                        format!("  {prompt}  "),
                        Style::new().fg(OK).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("Enter burn · q abort", Style::new().fg(DIM)),
                ])
            } else {
                Line::from(vec![
                    Span::styled(
                        format!("  {prompt}  "),
                        Style::new().fg(ERR).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("←/→ adjust · q abort", Style::new().fg(DIM)),
                ])
            }
        } else if self.aborted {
            Line::from(Span::styled("  aborting…", Style::new().fg(DIM)))
        } else {
            Line::from(Span::styled("  waiting for runner…", Style::new().fg(DIM)))
        };
        frame.render_widget(Paragraph::new(prompt_line), prompt_area);
    }

    fn param_lines(&self, media: &MediaInfo, plan: &ArchivePlan) -> Vec<Line<'static>> {
        let params = self.shown_params();
        let label_text = if self.editing == Some(EditField::Label) {
            format!("{}▏", self.input)
        } else {
            params.label.clone()
        };
        let speed_text = if self.editing == Some(EditField::Speed) {
            format!("{}▏", self.input)
        } else {
            let mut t = match params.speed {
                Some(s) => format!("{s}x"),
                None => "drive default".into(),
            };
            if !media.speeds.is_empty() {
                let probed: Vec<String> = media
                    .speeds
                    .iter()
                    .map(|s| format!("{}x", s.round() as u32))
                    .collect();
                t.push_str(&format!("  (probed: {})", probed.join(", ")));
            }
            t
        };
        let rows: [(&str, String, &str); PARAM_ROWS] = [
            ("Label      ", label_text, "e edit"),
            ("Speed      ", speed_text, "←/→ cycle · e type"),
            (
                "Redundancy ",
                format!(
                    "{}%  → parity ~{}",
                    params.redundancy_pct,
                    human_bytes(plan.parity_bytes_est)
                ),
                "←/→ adjust",
            ),
            (
                "Parity     ",
                (if params.parity { "on" } else { "off" }).into(),
                "Space toggle",
            ),
            (
                "Defect mgmt",
                (if params.defect_management {
                    "on (formatted, capacity shrinks)"
                } else {
                    "off (stream recording)"
                })
                .into(),
                "Space toggle",
            ),
            (
                "Staging    ",
                if self.editing == Some(EditField::Staging) {
                    format!("{}▏", self.input)
                } else {
                    params.staging.display().to_string()
                },
                "e edit · empty resets to default",
            ),
        ];
        rows.iter()
            .enumerate()
            .map(|(i, (name, value, hint))| {
                let sel = i == self.selected;
                let marker = if sel { "▸ " } else { "  " };
                let name_style = if sel {
                    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(DIM)
                };
                let mut spans = vec![
                    Span::styled(format!("  {marker}{name} "), name_style),
                    Span::styled(format!("{value:<36}"), Style::new().fg(Color::White)),
                ];
                if sel {
                    spans.push(Span::styled(hint.to_string(), Style::new().fg(DIM)));
                }
                Line::from(spans)
            })
            .collect()
    }

    fn probing_view(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from("")];
        match &self.failure {
            Some(msg) => {
                for row in head_wrap("  ✗ ", "    ", msg, area.width as usize) {
                    lines.push(Line::from(Span::styled(
                        row,
                        Style::new().fg(ERR).add_modifier(Modifier::BOLD),
                    )));
                }
            }
            None => {
                let spin = SPINNER[((self.tick / 2) % SPINNER.len() as u64) as usize];
                lines.push(Line::from(Span::styled(
                    format!("  {spin}  probing drive and media, inspecting payload…"),
                    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
                )));
            }
        }
        lines.push(Line::from(""));
        if let Some(prompt) = &self.pending_ack {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {prompt}  "),
                    Style::new().fg(WARN).add_modifier(Modifier::BOLD),
                ),
                Span::styled("Enter proceed · q abort", Style::new().fg(DIM)),
            ]));
            lines.push(Line::from(""));
        }
        let room = area.height.saturating_sub(lines.len() as u16) as usize;
        for (warn, text) in self.log.tail(room) {
            lines.push(log_line(*warn, text));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn run_screen(&self, frame: &mut Frame, area: Rect) {
        let block = body_block("Run");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let stages = self.visible_stages();
        let banner_rows = self
            .failure
            .as_deref()
            .map(|msg| head_wrap("  ✗ ", "    ", msg, inner.width as usize))
            .unwrap_or_default();
        // cap 5: a huge error chain must not evict the log; main reprints the
        // full error after the terminal is restored
        let banner_h = banner_rows.len().clamp(1, 5) as u16;
        let mut constraints: Vec<Constraint> =
            stages.iter().map(|_| Constraint::Length(1)).collect();
        constraints.push(Constraint::Length(1)); // current detail
        constraints.push(Constraint::Length(banner_h)); // failure banner / spacer
        constraints.push(Constraint::Fill(1)); // log
        let rows = Layout::vertical(constraints).split(inner);

        for (i, (stage, state)) in stages.iter().enumerate() {
            self.stage_row(frame, rows[i], *stage, state);
        }

        let detail_area = rows[stages.len()];
        if let Some((stage, detail)) = &self.current {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!("  ▸ {}: ", stage.label()),
                        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(detail.clone(), Style::new().fg(Color::White)),
                ])),
                detail_area,
            );
        }

        let banner_area = rows[stages.len() + 1];
        if !banner_rows.is_empty() {
            let lines: Vec<Line> = banner_rows
                .into_iter()
                .take(banner_area.height as usize)
                .map(|row| {
                    Line::from(Span::styled(
                        row,
                        Style::new().fg(ERR).add_modifier(Modifier::BOLD),
                    ))
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), banner_area);
        }

        let log_area = rows[stages.len() + 2];
        let room = log_area.height as usize;
        let lines: Vec<Line> = layout_log(self.log.tail(room), log_area.width as usize, room)
            .into_iter()
            .map(|(warn, row)| log_row(warn, row))
            .collect();
        frame.render_widget(Paragraph::new(lines), log_area);
    }

    fn stage_row(&self, frame: &mut Frame, area: Rect, stage: Stage, state: &StageState) {
        let [name_area, gauge_area] =
            Layout::horizontal([Constraint::Length(16), Constraint::Fill(1)]).areas(area);

        let (name_style, ratio, gauge_color, label) = match state {
            StageState::Pending => (Style::new().fg(DIM), 0.0, DIM, "pending".to_string()),
            StageState::Running { pct } => {
                let label = match pct {
                    Some(p) => format!("{:.0}%", p.clamp(0.0, 100.0)),
                    None => SPINNER[((self.tick / 2) % SPINNER.len() as u64) as usize].to_string(),
                };
                (
                    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
                    pct_ratio(*pct),
                    ACCENT,
                    label,
                )
            }
            StageState::Done => (Style::new().fg(OK), 1.0, OK, "done".to_string()),
            StageState::Failed => (
                Style::new().fg(ERR).add_modifier(Modifier::BOLD),
                1.0,
                ERR,
                "failed".to_string(),
            ),
        };

        frame.render_widget(
            Paragraph::new(Span::styled(format!("  {}", stage.label()), name_style)),
            name_area,
        );
        frame.render_widget(
            Gauge::default()
                .ratio(ratio)
                .gauge_style(Style::new().fg(gauge_color).bg(Color::Black))
                .label(Span::styled(label, Style::new().fg(Color::White))),
            pad(gauge_area),
        );
    }

    fn report_screen(&self, frame: &mut Frame, area: Rect) {
        let block = body_block("Report");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(report) = &self.report else {
            return;
        };

        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  ✓ archive complete",
                Style::new().fg(OK).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        for (stage, summary) in &report.stages {
            lines.push(Line::from(vec![
                Span::styled(format!("  ✓ {:<14}", stage.label()), Style::new().fg(OK)),
                Span::styled(summary.clone(), Style::new().fg(Color::Gray)),
            ]));
        }
        lines.push(Line::from(""));
        if let Some(iso) = &report.iso_path {
            lines.push(kv("ISO    ", &iso.display().to_string()));
        }
        if let Some(sha) = &report.iso_sha256 {
            lines.push(kv("sha256 ", sha));
        }
        if report.iso_bytes > 0 {
            lines.push(kv("size   ", &human_bytes(report.iso_bytes)));
        }
        if !report.written_files.is_empty() {
            lines.push(Line::from(""));
            lines.push(heading("Files written"));
            for f in &report.written_files {
                lines.push(Line::from(Span::styled(
                    format!("    {}", f.display()),
                    Style::new().fg(Color::Gray),
                )));
            }
        }
        if !report.degradations.is_empty() {
            lines.push(Line::from(""));
            lines.push(heading("Caveats"));
            for c in &report.degradations {
                lines.push(Line::from(Span::styled(
                    format!("  ! {c}"),
                    Style::new().fg(WARN),
                )));
            }
        }
        lines.push(Line::from(""));
        lines.push(heading("Reminders"));
        for r in &report.reminders {
            lines.push(Line::from(Span::styled(
                format!("  • {r}"),
                Style::new().fg(WARN),
            )));
        }
        // logical-line clamp: wrapped lines under-scroll slightly, accepted
        // over the feature-gated Paragraph::line_count
        let max = lines.len().saturating_sub(inner.height as usize) as u16;
        let offset = self.report_scroll.min(max);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            inner,
        );
    }
}

// -- helpers ---------------------------------------------------------------

fn payload_rows(paths: &[PathBuf]) -> Vec<PayloadRow> {
    paths
        .iter()
        .map(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string());
            match Payload::inspect(p.clone()) {
                Ok((f, _)) => PayloadRow {
                    name: if f.is_dir {
                        format!("{}/", f.name)
                    } else {
                        f.name.clone()
                    },
                    size_text: if f.is_dir {
                        format!("{} ({} files)", human_bytes(f.total_size), f.files.len())
                    } else {
                        human_bytes(f.total_size)
                    },
                    container: f.looks_like_container(),
                },
                Err(e) => PayloadRow {
                    name,
                    size_text: format!("({e})"),
                    container: false,
                },
            }
        })
        .collect()
}

fn fit_ratio(total: u64, budget: u64) -> f64 {
    if budget == 0 {
        return 1.0;
    }
    (total as f64 / budget as f64).clamp(0.0, 1.0)
}

fn pct_ratio(pct: Option<f32>) -> f64 {
    match pct {
        Some(p) => (p as f64 / 100.0).clamp(0.0, 1.0),
        None => 0.0,
    }
}

fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

pub(crate) fn app_title() -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "ovenmitts",
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  —  archival burn to BD-R / M-DISC", Style::new().fg(DIM)),
    ])
}

pub(crate) fn body_block(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {text}"),
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
    ))
}

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {key} "), Style::new().fg(DIM)),
        Span::styled(value.to_string(), Style::new().fg(Color::White)),
    ])
}

fn log_line(warn: bool, text: &str) -> Line<'static> {
    if warn {
        Line::from(Span::styled(format!("  ! {text}"), Style::new().fg(WARN)))
    } else {
        Line::from(Span::styled(
            format!("  · {text}"),
            Style::new().fg(Color::Gray),
        ))
    }
}

/// Greedy word wrap by char count: `head` starts row one, `cont` starts every
/// continuation row; no row exceeds `width` chars. Words longer than a row's
/// content width hard-split; empty text still yields the head row.
pub(crate) fn head_wrap(head: &str, cont: &str, text: &str, width: usize) -> Vec<String> {
    // content width clamps to 1 so a pathologically narrow area cannot stall
    let content = |prefix: &str| width.saturating_sub(prefix.chars().count()).max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut line = String::from(head);
    let mut left = content(head);
    let mut bare = true;
    for word in text.split_whitespace() {
        let mut chars = word.chars().peekable();
        while chars.peek().is_some() {
            let len = chars.clone().count();
            let need = if bare { len } else { len + 1 };
            if need <= left {
                if !bare {
                    line.push(' ');
                    left -= 1;
                }
                line.extend(chars.by_ref());
                left -= len;
                bare = false;
            } else if bare {
                for _ in 0..left {
                    line.push(chars.next().unwrap());
                }
                rows.push(std::mem::replace(&mut line, String::from(cont)));
                left = content(cont);
            } else {
                rows.push(std::mem::replace(&mut line, String::from(cont)));
                left = content(cont);
                bare = true;
            }
        }
    }
    rows.push(line);
    rows
}

/// Pre-wrapped visual rows for the run-screen log, trimmed to the newest
/// `room` rows so wrapping can never push the latest entries off-screen.
fn layout_log<'a>(
    entries: impl Iterator<Item = &'a (bool, String)>,
    width: usize,
    room: usize,
) -> Vec<(bool, String)> {
    let mut rows: Vec<(bool, String)> = Vec::new();
    for (warn, text) in entries {
        let head = if *warn { "  ! " } else { "  · " };
        rows.extend(
            head_wrap(head, "    ", text, width)
                .into_iter()
                .map(|r| (*warn, r)),
        );
    }
    rows.split_off(rows.len().saturating_sub(room))
}

fn log_row(warn: bool, row: String) -> Line<'static> {
    let style = if warn {
        Style::new().fg(WARN)
    } else {
        Style::new().fg(Color::Gray)
    };
    Line::from(Span::styled(row, style))
}

/// Discard queued terminal input. Bounded so a pathological stream cannot
/// stall the UI; errors are moot (no input to flush is the goal state).
pub(crate) fn flush_input() {
    for _ in 0..1024 {
        match event::poll(Duration::ZERO) {
            Ok(true) => {
                let _ = event::read();
            }
            _ => break,
        }
    }
}

pub(crate) fn pad(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    }
}

fn ack_modal(frame: &mut Frame, area: Rect, prompt: &str) {
    let width = area.width.saturating_sub(8).clamp(20, 64);
    let text_rows = (prompt.chars().count() as u16 / width.saturating_sub(4).max(1)) + 1;
    let height = (text_rows + 4).min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width),
        height,
    };
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(WARN))
        .title(Span::styled(
            " Action needed ",
            Style::new().fg(WARN).add_modifier(Modifier::BOLD),
        ));
    let lines = vec![
        Line::from(Span::styled(
            prompt.to_string(),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Enter proceed · q abort",
            Style::new().fg(DIM),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FileConfig;
    use crate::plan::{MediaKind, PlanInput};

    fn test_app() -> (App, Receiver<Ack>) {
        let (ack_tx, ack_rx) = mpsc::channel();
        (
            App::new(
                Config::resolve(FileConfig::default()).unwrap(),
                vec![],
                sample_params(),
                ack_tx,
            ),
            ack_rx,
        )
    }

    /// Prompt applied AND rendered long enough ago that Proceed is live —
    /// the state every pre-existing ack test assumes.
    fn need_ack(app: &mut App) {
        app.apply(StageEvent::NeedAck {
            prompt: "burn?".into(),
        });
        app.ack_shown = Some(Instant::now() - ACK_ARM_DELAY);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn sample_params() -> BurnParams {
        BurnParams {
            label: "T1".into(),
            speed: None,
            redundancy_pct: 15,
            parity: true,
            defect_management: false,
            staging: "/staging".into(),
        }
    }

    fn sample_plan() -> (MediaInfo, ArchivePlan) {
        let media = MediaInfo {
            kind: MediaKind::BdR25,
            profile: "BD-R".into(),
            blank: true,
            formatted: false,
            free_bytes: crate::plan::BD_R_25,
            formatted_capacity: None,
            speeds: vec![],
            media_id: None,
        };
        let input = PlanInput {
            payloads: vec![],
            parity: true,
            redundancy_pct: 15,
            headroom_pct: 5,
            defect_management: false,
        };
        let plan = crate::plan::build_plan(&input, &media);
        (media, plan)
    }

    fn plan_event() -> StageEvent {
        let (media, plan) = sample_plan();
        StageEvent::Plan {
            device: "/dev/sr0".into(),
            media,
            plan,
            params: sample_params(),
        }
    }

    #[test]
    fn ack_enter_ignored_before_prompt_is_rendered() {
        let (mut app, ack_rx) = test_app();
        app.apply(plan_event());
        app.apply(StageEvent::NeedAck {
            prompt: "burn?".into(),
        });
        // type-ahead: no render has armed the prompt yet
        app.on_key(key(KeyCode::Enter));
        assert!(ack_rx.try_recv().is_err(), "stale Enter must not proceed");
        assert!(app.pending_ack.is_some());
        assert_eq!(app.screen, Screen::Plan);
    }

    #[test]
    fn ack_enter_ignored_within_arm_delay_then_accepted() {
        let (mut app, ack_rx) = test_app();
        app.apply(plan_event());
        app.apply(StageEvent::NeedAck {
            prompt: "burn?".into(),
        });
        app.ack_shown = Some(Instant::now());
        app.on_key(key(KeyCode::Enter));
        assert!(
            ack_rx.try_recv().is_err(),
            "Enter inside the arm delay must not proceed"
        );
        app.ack_shown = Some(Instant::now() - ACK_ARM_DELAY);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(ack_rx.try_recv().unwrap(), Ack::Proceed);
        assert_eq!(app.screen, Screen::Run);
    }

    #[test]
    fn ack_abort_works_even_before_arming() {
        let (mut app, ack_rx) = test_app();
        app.apply(plan_event());
        app.apply(StageEvent::NeedAck {
            prompt: "burn?".into(),
        });
        app.on_key(key(KeyCode::Esc));
        assert_eq!(ack_rx.try_recv().unwrap(), Ack::Abort);
    }

    #[test]
    fn log_ring_caps_at_200_and_keeps_newest() {
        let mut log = EventLog::new();
        for i in 0..250 {
            log.push(false, format!("line {i}"));
        }
        assert_eq!(log.len(), 200);
        let first = log.tail(200).next().unwrap();
        assert_eq!(first.1, "line 50");
        let last = log.tail(1).next().unwrap();
        assert_eq!(last.1, "line 249");
    }

    #[test]
    fn log_tail_returns_last_n_in_order() {
        let mut log = EventLog::new();
        for i in 0..5 {
            log.push(i % 2 == 0, format!("{i}"));
        }
        let tail: Vec<&str> = log.tail(3).map(|(_, t)| t.as_str()).collect();
        assert_eq!(tail, ["2", "3", "4"]);
    }

    #[test]
    fn fit_ratio_clamps_and_survives_zero_budget() {
        assert_eq!(fit_ratio(50, 100), 0.5);
        assert_eq!(fit_ratio(200, 100), 1.0);
        assert_eq!(fit_ratio(1, 0), 1.0);
    }

    #[test]
    fn pct_ratio_treats_pct_as_0_to_100() {
        assert_eq!(pct_ratio(None), 0.0);
        assert_eq!(pct_ratio(Some(50.0)), 0.5);
        assert_eq!(pct_ratio(Some(150.0)), 1.0);
        assert_eq!(pct_ratio(Some(-5.0)), 0.0);
    }

    #[test]
    fn elapsed_formats_hh_mm_ss() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "00:00:00");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "00:00:59");
        assert_eq!(format_elapsed(Duration::from_secs(3661)), "01:01:01");
    }

    #[test]
    fn format_stage_hidden_until_it_appears() {
        let (mut app, _rx) = test_app();
        assert!(app
            .visible_stages()
            .iter()
            .all(|(s, _)| *s != Stage::Format));
        app.apply(StageEvent::StageStart(Stage::Format));
        assert!(app
            .visible_stages()
            .iter()
            .any(|(s, _)| *s == Stage::Format));
    }

    #[test]
    fn confirm_ack_sends_proceed_and_enters_run() {
        let (mut app, ack_rx) = test_app();
        app.apply(plan_event());
        app.apply(StageEvent::NeedAck {
            prompt: "burn now?".into(),
        });
        app.ack_shown = Some(Instant::now() - ACK_ARM_DELAY);
        assert_eq!(app.screen, Screen::Plan);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(ack_rx.try_recv().unwrap(), Ack::Proceed);
        assert_eq!(app.screen, Screen::Run);
        assert!(app.pending_ack.is_none());
    }

    #[test]
    fn ack_before_plan_stays_on_plan_screen() {
        let (mut app, ack_rx) = test_app();
        app.apply(StageEvent::NeedAck {
            prompt: "insert a disc".into(),
        });
        app.ack_shown = Some(Instant::now() - ACK_ARM_DELAY);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(ack_rx.try_recv().unwrap(), Ack::Proceed);
        assert_eq!(app.screen, Screen::Plan);
    }

    #[test]
    fn q_on_ack_sends_abort() {
        let (mut app, ack_rx) = test_app();
        app.apply(StageEvent::NeedAck {
            prompt: "burn now?".into(),
        });
        app.on_key(key(KeyCode::Char('q')));
        assert_eq!(ack_rx.try_recv().unwrap(), Ack::Abort);
        assert!(app.aborted);
    }

    #[test]
    fn failed_event_marks_stage_and_shows_banner() {
        let (mut app, _rx) = test_app();
        app.apply(plan_event());
        app.apply(StageEvent::Failed {
            stage: Stage::Burn,
            error: "no disc".into(),
        });
        assert_eq!(app.screen, Screen::Run);
        assert!(app.failure.as_deref().unwrap().contains("burn failed"));
        assert!(!app.quit);
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn preflight_failure_before_plan_stays_on_plan_screen() {
        let (mut app, _rx) = test_app();
        app.apply(StageEvent::Failed {
            stage: Stage::Preflight,
            error: "probing /dev/sr0: no medium present".into(),
        });
        assert_eq!(
            app.screen,
            Screen::Plan,
            "no plan yet: Run would read as burning"
        );
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("✗ preflight failed"), "{text}");
        assert!(text.contains("no medium present"), "{text}");
        assert!(text.contains("q quit"), "{text}");
        assert!(!text.contains("probing drive and media"), "{text}");
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn q_mid_run_is_ignored_without_failure() {
        let (mut app, _rx) = test_app();
        app.apply(StageEvent::StageStart(Stage::Burn));
        assert_eq!(app.screen, Screen::Run);
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.quit);
    }

    #[test]
    fn finished_switches_to_report_and_q_quits() {
        let (mut app, _rx) = test_app();
        app.apply(StageEvent::Finished {
            report: RunReport::default(),
        });
        assert_eq!(app.screen, Screen::Report);
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn plan_event_updates_device_row_source() {
        let (mut app, _rx) = test_app();
        assert_eq!(app.device, None);
        app.apply(plan_event());
        assert_eq!(app.device.as_deref(), Some("/dev/sr0"));
    }

    #[test]
    fn plan_keys_navigate_param_rows() {
        let (mut app, _rx) = test_app();
        app.apply(plan_event());
        assert_eq!(app.selected, ROW_LABEL);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected, ROW_REDUNDANCY);
        for _ in 0..9 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(
            app.selected, ROW_STAGING,
            "selection clamps to the last row"
        );
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.selected, ROW_DEFECT_MGMT);
        for _ in 0..9 {
            app.on_key(key(KeyCode::Char('k')));
        }
        assert_eq!(app.selected, ROW_LABEL);
    }

    #[test]
    fn toggle_sends_amend_when_ack_pending() {
        let (mut app, ack_rx) = test_app();
        app.apply(plan_event());
        need_ack(&mut app);
        app.selected = ROW_PARITY;
        app.on_key(key(KeyCode::Char(' ')));
        let Ok(Ack::Amend(p)) = ack_rx.try_recv() else {
            panic!("Space on the parity row must send Amend");
        };
        assert!(!p.parity);
        assert!(app.pending_ack.is_none(), "amend consumes the ack slot");
        assert!(app.replanning);
    }

    #[test]
    fn edits_queue_until_needack_then_flush() {
        let (mut app, ack_rx) = test_app();
        app.apply(plan_event()); // no NeedAck yet
        app.selected = ROW_DEFECT_MGMT;
        app.on_key(key(KeyCode::Char(' ')));
        assert!(matches!(ack_rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(app.edit_dirty);
        need_ack(&mut app);
        let Ok(Ack::Amend(p)) = ack_rx.try_recv() else {
            panic!("queued edit must flush on NeedAck");
        };
        assert!(p.defect_management);
        assert!(matches!(ack_rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(app.pending_ack.is_none(), "flush consumes the ack slot");
    }

    #[test]
    fn rapid_edits_coalesce_into_single_amend() {
        let (mut app, ack_rx) = test_app();
        app.apply(plan_event());
        need_ack(&mut app);
        app.selected = ROW_REDUNDANCY; // base 15
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        let Ok(Ack::Amend(p)) = ack_rx.try_recv() else {
            panic!("first edit must send immediately");
        };
        assert_eq!(p.redundancy_pct, 16);
        assert!(matches!(ack_rx.try_recv(), Err(TryRecvError::Empty)));
        // runner echoes the 16% plan and re-asks; queued edits flush as ONE amend
        let (media, plan) = sample_plan();
        let mut p16 = sample_params();
        p16.redundancy_pct = 16;
        app.apply(StageEvent::Plan {
            device: "/dev/sr0".into(),
            media,
            plan,
            params: p16,
        });
        need_ack(&mut app);
        let Ok(Ack::Amend(p)) = ack_rx.try_recv() else {
            panic!("queued edits must flush on NeedAck");
        };
        assert_eq!(p.redundancy_pct, 18);
        assert!(matches!(ack_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn label_edit_mode_commits_on_enter_and_esc_cancels() {
        let (mut app, ack_rx) = test_app();
        app.apply(plan_event());
        need_ack(&mut app);
        app.selected = ROW_LABEL;
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(app.editing, Some(EditField::Label));
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.aborted, "q while editing is a character, not abort");
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.editing, None);
        assert!(
            !app.aborted,
            "Esc while editing cancels the edit, not the run"
        );
        assert!(matches!(ack_rx.try_recv(), Err(TryRecvError::Empty)));

        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Backspace)); // "T1" -> "T"
        app.on_key(key(KeyCode::Char('9')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.editing, None);
        let Ok(Ack::Amend(p)) = ack_rx.try_recv() else {
            panic!("Enter must commit the label edit");
        };
        assert_eq!(p.label, "T9");
        assert_eq!(
            app.screen,
            Screen::Plan,
            "Enter in edit mode must not proceed"
        );
    }

    #[test]
    fn speed_cycles_through_probed_speeds() {
        let (mut app, ack_rx) = test_app();
        let (mut media, plan) = sample_plan();
        media.speeds = vec![2.0, 4.0];
        app.apply(StageEvent::Plan {
            device: "/dev/sr0".into(),
            media,
            plan,
            params: sample_params(),
        });
        need_ack(&mut app);
        app.selected = ROW_SPEED;
        app.on_key(key(KeyCode::Right)); // drive default -> 2x
        let Ok(Ack::Amend(p)) = ack_rx.try_recv() else {
            panic!("expected Amend");
        };
        assert_eq!(p.speed, Some(2));
        app.on_key(key(KeyCode::Right)); // queued: 2x -> 4x
        need_ack(&mut app);
        let Ok(Ack::Amend(p)) = ack_rx.try_recv() else {
            panic!("expected flushed Amend");
        };
        assert_eq!(p.speed, Some(4));
        app.on_key(key(KeyCode::Right)); // queued: 4x wraps to drive default
        need_ack(&mut app);
        let Ok(Ack::Amend(p)) = ack_rx.try_recv() else {
            panic!("expected flushed Amend");
        };
        assert_eq!(p.speed, None);
    }

    #[test]
    fn canonical_params_replace_local_edit_after_replan() {
        let (mut app, ack_rx) = test_app();
        app.apply(plan_event());
        need_ack(&mut app);
        app.selected = ROW_PARITY;
        app.on_key(key(KeyCode::Char(' ')));
        assert!(matches!(ack_rx.try_recv(), Ok(Ack::Amend(_))));
        let mut canonical = sample_params();
        canonical.parity = false;
        let (media, plan) = sample_plan();
        app.apply(StageEvent::Plan {
            device: "/dev/sr0".into(),
            media,
            plan,
            params: canonical.clone(),
        });
        assert!(app.edit.is_none(), "clean round-trip adopts runner params");
        assert_eq!(app.shown_params(), &canonical);
        assert!(app.replanning);
        need_ack(&mut app);
        assert!(!app.replanning);
        assert!(app.pending_ack.is_some());
    }

    #[test]
    fn enter_ignored_when_plan_does_not_fit() {
        let (mut app, ack_rx) = test_app();
        let (media, mut plan) = sample_plan();
        plan.fits = false;
        app.apply(StageEvent::Plan {
            device: "/dev/sr0".into(),
            media,
            plan,
            params: sample_params(),
        });
        need_ack(&mut app);
        app.on_key(key(KeyCode::Enter));
        assert!(
            matches!(ack_rx.try_recv(), Err(TryRecvError::Empty)),
            "a non-fitting plan must not be confirmable"
        );
        assert_eq!(app.screen, Screen::Plan);
        app.on_key(key(KeyCode::Char('q')));
        assert_eq!(ack_rx.try_recv().unwrap(), Ack::Abort);
    }

    #[test]
    fn edits_locked_after_run_starts() {
        let (mut app, ack_rx) = test_app();
        app.apply(plan_event());
        need_ack(&mut app);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(ack_rx.try_recv().unwrap(), Ack::Proceed);
        assert_eq!(app.screen, Screen::Run);
        app.selected = ROW_PARITY;
        app.on_key(key(KeyCode::Char(' ')));
        app.on_key(key(KeyCode::Right));
        assert!(
            matches!(ack_rx.try_recv(), Err(TryRecvError::Empty)),
            "param keys must be inert once the run starts"
        );
    }

    #[test]
    fn report_screen_lists_each_reminder_once() {
        let (mut app, _ack_rx) = test_app();
        app.apply(StageEvent::Finished {
            report: RunReport {
                iso_path: Some(PathBuf::from("/staging/ARCHIVE/ARCHIVE.iso")),
                iso_sha256: Some("ab".repeat(32)),
                iso_bytes: 4096,
                stages: vec![],
                reminders: vec![
                    "second copy: insert a fresh disc and run `ovenmitts burn-iso /staging/ARCHIVE/ARCHIVE.iso`".into(),
                ],
                written_files: vec![],
                degradations: vec![],
            },
        });
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buf = terminal.backend().buffer();
        let hits = (0..30)
            .map(|y| {
                (0..120)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .filter(|row| row.contains("second copy"))
            .count();
        assert_eq!(hits, 1, "the second-copy reminder must render exactly once");
    }

    #[test]
    fn staging_edit_commits_on_enter_and_empty_resets_to_default() {
        let (mut app, ack_rx) = test_app();
        app.apply(plan_event());
        need_ack(&mut app);
        app.selected = ROW_STAGING;
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(app.editing, Some(EditField::Staging));
        assert_eq!(app.input, "/staging", "edit seeds with the current path");
        for _ in 0.."/staging".len() {
            app.on_key(key(KeyCode::Backspace));
        }
        for c in "/alt/stage ".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.editing, None);
        let Ok(Ack::Amend(p)) = ack_rx.try_recv() else {
            panic!("Enter must commit the staging edit");
        };
        assert_eq!(
            p.staging,
            PathBuf::from("/alt/stage"),
            "commit trims whitespace"
        );

        app.on_key(key(KeyCode::Char('e')));
        for _ in 0.."/alt/stage".len() {
            app.on_key(key(KeyCode::Backspace));
        }
        app.on_key(key(KeyCode::Enter));
        need_ack(&mut app);
        let Ok(Ack::Amend(p)) = ack_rx.try_recv() else {
            panic!("queued empty-reset must flush on NeedAck");
        };
        assert_eq!(
            p.staging, app.cfg.staging,
            "empty input resets to the session default"
        );
    }

    #[test]
    fn report_screen_lists_written_files() {
        let (mut app, _ack_rx) = test_app();
        app.apply(StageEvent::Finished {
            report: RunReport {
                written_files: vec![
                    PathBuf::from("/staging/PIPE/parity/vault.hc.par2"),
                    PathBuf::from("/staging/PIPE/checksums.sha256"),
                ],
                ..RunReport::default()
            },
        });
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..30)
            .map(|y| {
                (0..120)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect()
            })
            .collect();
        assert!(rows.iter().any(|r| r.contains("Files written")));
        assert!(rows
            .iter()
            .any(|r| r.contains("/staging/PIPE/parity/vault.hc.par2")));
        assert!(rows
            .iter()
            .any(|r| r.contains("/staging/PIPE/checksums.sha256")));
    }

    #[test]
    fn report_screen_scrolls_long_file_list() {
        let (mut app, _ack_rx) = test_app();
        app.apply(StageEvent::Finished {
            report: RunReport {
                written_files: (0..40)
                    .map(|i| PathBuf::from(format!("/staging/X/file{i:02}")))
                    .collect(),
                ..RunReport::default()
            },
        });
        let backend = ratatui::backend::TestBackend::new(80, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let screen_text = |t: &ratatui::Terminal<ratatui::backend::TestBackend>| -> String {
            let buf = t.backend().buffer();
            (0..12)
                .map(|y| {
                    (0..80)
                        .map(|x| buf.cell((x, y)).unwrap().symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        terminal.draw(|f| app.render(f)).unwrap();
        let top = screen_text(&terminal);
        assert!(top.contains("file00"), "unscrolled report shows the head");
        assert!(!top.contains("file39"));
        for _ in 0..100 {
            app.on_key(key(KeyCode::Down));
        }
        terminal.draw(|f| app.render(f)).unwrap();
        let bottom = screen_text(&terminal);
        assert!(
            bottom.contains("file39"),
            "scrolling must reach the tail (clamped)"
        );
        assert!(!bottom.contains("file00"));
        assert!(!app.quit, "scroll keys must not quit the report screen");
    }

    #[test]
    fn head_wrap_wraps_at_word_boundaries() {
        let rows = head_wrap("  ! ", "    ", "organic dye lasts five years", 20);
        assert_eq!(rows, vec!["  ! organic dye", "    lasts five years"]);
        assert!(rows.iter().all(|r| r.chars().count() <= 20));
    }

    #[test]
    fn head_wrap_hard_splits_overlong_word() {
        let rows = head_wrap("  ! ", "    ", "abcdefghijklmnop", 10);
        assert_eq!(rows, vec!["  ! abcdef", "    ghijkl", "    mnop"]);
    }

    #[test]
    fn head_wrap_empty_text_yields_head_row() {
        assert_eq!(head_wrap("  · ", "    ", "", 40), vec!["  · "]);
    }

    #[test]
    fn head_wrap_survives_width_smaller_than_head() {
        let rows = head_wrap("  ! ", "    ", "ab cd", 2);
        assert!(!rows.is_empty());
        assert!(rows.iter().map(|r| r.chars().count()).sum::<usize>() >= 4 + 4);
    }

    #[test]
    fn layout_log_keeps_newest_rows_when_wrapping_overflows() {
        let entries = [
            (false, "one two three four five six seven".to_string()),
            (true, "eight nine ten eleven twelve".to_string()),
            (false, "final entry".to_string()),
        ];
        let rows = layout_log(entries.iter(), 16, 4);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows.last().unwrap().1, "  · final entry");
    }

    #[test]
    fn layout_log_indents_continuations_under_prefix() {
        let entries = [(
            true,
            "a warning long enough to wrap onto more rows".to_string(),
        )];
        let rows = layout_log(entries.iter(), 20, 10);
        assert!(rows.len() > 1);
        assert!(rows[0].1.starts_with("  ! "));
        assert!(rows[1..].iter().all(|(_, r)| r.starts_with("    ")));
    }
}
