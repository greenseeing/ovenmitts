use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use crate::config::Config;
use crate::plan::{build_plan, container_heuristic, human_bytes, MediaInfo, Payload, PlanInput};
use crate::tools::Tools;
use crate::tui::{app_title, body_block, flush_input, head_wrap, pad, ACCENT, DIM, ERR, OK, WARN};

/// Interactive payload picker for bare `ovenmitts`: browse from `start`,
/// checkbox-select files/directories, fuzzy-filter within the current
/// directory, with a live fit estimate against the probed disc.
/// Ok(None) = user cancelled.
pub fn pick_payloads(cfg: &Config, tools: &Tools, start: PathBuf) -> Result<Option<Vec<PathBuf>>> {
    let mut picker = Picker::new(cfg.clone(), start)?;
    let (probe, media_rx) = probe_in_background(cfg, tools);
    let mut terminal = ratatui::init();
    flush_input();
    let loop_result = event_loop(&mut picker, &mut terminal, &media_rx);
    ratatui::restore();
    // the probe holds the drive exclusively; returning before it finishes
    // would race the runner's own preflight probe into a busy error
    let _ = probe.join();
    loop_result?;
    Ok(match picker.outcome {
        Some(Outcome::Confirmed(paths)) => Some(paths),
        _ => None,
    })
}

fn probe_in_background(
    cfg: &Config,
    tools: &Tools,
) -> (std::thread::JoinHandle<()>, Receiver<Option<MediaInfo>>) {
    let (tx, rx) = mpsc::channel();
    let cfg = cfg.clone();
    let tools = tools.clone();
    let handle = std::thread::spawn(move || {
        let media = crate::runner::detect_media(&cfg, &tools)
            .ok()
            .map(|(_, m, _)| m);
        let _ = tx.send(media);
    });
    (handle, rx)
}

fn event_loop(
    picker: &mut Picker,
    terminal: &mut DefaultTerminal,
    media_rx: &Receiver<Option<MediaInfo>>,
) -> Result<()> {
    while picker.outcome.is_none() {
        if let Ok(media) = media_rx.try_recv() {
            picker.set_media(media);
        }
        terminal.draw(|frame| picker.render(frame))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    picker.on_key(key);
                }
            }
        }
    }
    Ok(())
}

enum Outcome {
    Confirmed(Vec<PathBuf>),
    Cancelled,
}

enum Probe {
    Pending,
    /// No drive gave a medium: fit math assumes a blank BD-R 25, like `plan`.
    Assumed(MediaInfo),
    Found(MediaInfo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryKind {
    Dir,
    File {
        size: u64,
        container: bool,
    },
    /// Broken symlink, socket, fifo, ...: visible but inert.
    Inert,
}

struct Entry {
    name: String,
    path: PathBuf,
    kind: EntryKind,
}

/// A confirmed selection, keyed in `Picker::selected` by the inspected
/// (canonical) root so symlink aliases cannot smuggle in duplicates.
struct Sel {
    /// The listing path the user toggled; differs from the key for symlinks.
    entry_path: PathBuf,
    payload: Payload,
}

struct Picker {
    cfg: Config,
    dir: PathBuf,
    entries: Vec<Entry>,
    /// Indices into `entries` after the hidden filter and fuzzy query.
    visible: Vec<usize>,
    cursor: usize,
    selected: BTreeMap<PathBuf, Sel>,
    filter: String,
    filtering: bool,
    show_hidden: bool,
    status: Option<(bool, String)>,
    probe: Probe,
    outcome: Option<Outcome>,
}

impl Picker {
    fn new(cfg: Config, start: PathBuf) -> Result<Self> {
        let dir = std::fs::canonicalize(&start)
            .map_err(|e| anyhow::anyhow!("cannot read directory {}: {e}", start.display()))?;
        let entries = read_entries(&dir)?;
        let mut picker = Self {
            cfg,
            dir,
            entries,
            visible: Vec::new(),
            cursor: 0,
            selected: BTreeMap::new(),
            filter: String::new(),
            filtering: false,
            show_hidden: false,
            status: None,
            probe: Probe::Pending,
            outcome: None,
        };
        picker.refilter();
        Ok(picker)
    }

    fn set_media(&mut self, media: Option<MediaInfo>) {
        self.probe = match media {
            Some(m) => Probe::Found(m),
            None => Probe::Assumed(
                crate::media::synthetic("bd25").expect("bd25 is a built-in media hint"),
            ),
        };
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.outcome = Some(Outcome::Cancelled);
            return;
        }
        if self.filtering {
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.refilter();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.refilter();
                }
                KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
                KeyCode::Down => self.cursor_down(),
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.refilter();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor_down(),
            KeyCode::Char(' ') => self.toggle_current(),
            KeyCode::Right | KeyCode::Char('l') => self.descend(),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => self.ascend(),
            KeyCode::Char('/') => {
                self.filtering = true;
                self.status = None;
            }
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                self.refilter();
            }
            KeyCode::Enter => self.confirm(),
            KeyCode::Char('q') | KeyCode::Esc => self.outcome = Some(Outcome::Cancelled),
            _ => {}
        }
    }

    fn cursor_down(&mut self) {
        if !self.visible.is_empty() {
            self.cursor = (self.cursor + 1).min(self.visible.len() - 1);
        }
    }

    fn current(&self) -> Option<&Entry> {
        self.visible.get(self.cursor).map(|&i| &self.entries[i])
    }

    /// Inspect on toggle: selection errors (unreadable, empty dir, bad name)
    /// surface here instead of failing preflight later.
    fn toggle_current(&mut self) {
        let Some(entry) = self.current() else { return };
        let (path, name, kind) = (entry.path.clone(), entry.name.clone(), entry.kind.clone());
        if kind == EntryKind::Inert {
            self.status = Some((true, format!("{name}: not a regular file or directory")));
            return;
        }
        if let Some(key) = self.sel_key_for(&path) {
            self.selected.remove(&key);
            self.status = None;
            return;
        }
        match Payload::inspect(path.clone()) {
            Err(e) => self.status = Some((true, format!("{e:#}"))),
            Ok((payload, _)) => {
                let root = payload.root.clone();
                // a symlink alias of a selected root is the same selection
                if self.selected.remove(&root).is_some() {
                    self.status = None;
                    return;
                }
                if let Some(anc) = self
                    .selected
                    .values()
                    .find(|s| root.starts_with(&s.payload.root) && s.payload.root != root)
                {
                    self.status =
                        Some((true, format!("already included via {}", anc.payload.name)));
                    return;
                }
                let nested: Vec<PathBuf> = self
                    .selected
                    .keys()
                    .filter(|k| k.starts_with(&root))
                    .cloned()
                    .collect();
                for k in &nested {
                    self.selected.remove(k);
                }
                self.status = match nested.len() {
                    0 => None,
                    n => Some((
                        false,
                        format!("{} now covers {n} nested selection(s)", payload.name),
                    )),
                };
                self.selected.insert(
                    root,
                    Sel {
                        entry_path: path,
                        payload,
                    },
                );
            }
        }
    }

    fn descend(&mut self) {
        let Some(entry) = self.current() else { return };
        if entry.kind != EntryKind::Dir {
            return;
        }
        let (path, name) = (entry.path.clone(), entry.name.clone());
        // canonicalize so listings under a symlinked dir stay canonical paths
        let target = match std::fs::canonicalize(&path) {
            Ok(t) => t,
            Err(e) => {
                self.status = Some((true, format!("cannot open {name}: {e}")));
                return;
            }
        };
        match read_entries(&target) {
            Ok(entries) => {
                self.dir = target;
                self.entries = entries;
                self.reset_view();
                self.cursor = 0;
            }
            Err(e) => self.status = Some((true, format!("{e:#}"))),
        }
    }

    fn ascend(&mut self) {
        let Some(parent) = self.dir.parent().map(Path::to_path_buf) else {
            return;
        };
        match read_entries(&parent) {
            Ok(entries) => {
                let from = std::mem::replace(&mut self.dir, parent);
                self.entries = entries;
                self.reset_view();
                self.cursor = self
                    .visible
                    .iter()
                    .position(|&i| self.entries[i].path == from)
                    .unwrap_or(0);
            }
            Err(e) => self.status = Some((true, format!("{e:#}"))),
        }
    }

    fn reset_view(&mut self) {
        self.filter.clear();
        self.filtering = false;
        self.status = None;
        self.refilter();
    }

    /// Enter with nothing selected picks the cursor entry first, so
    /// "navigate to vault.hc, Enter" is a complete flow.
    fn confirm(&mut self) {
        if self.selected.is_empty() {
            self.toggle_current();
        }
        if !self.selected.is_empty() {
            self.outcome = Some(Outcome::Confirmed(self.selected.keys().cloned().collect()));
        }
    }

    fn refilter(&mut self) {
        let show_hidden = self.show_hidden;
        let shown = |e: &Entry| show_hidden || !e.name.starts_with('.');
        let query = self.filter.trim().to_string();
        if query.is_empty() {
            self.visible = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| shown(e))
                .map(|(i, _)| i)
                .collect();
        } else {
            let mut scored: Vec<(u32, usize)> = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| shown(e))
                .filter_map(|(i, e)| fuzzy_match(&query, &e.name).map(|s| (s, i)))
                .collect();
            scored.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| self.entries[a.1].name.cmp(&self.entries[b.1].name))
            });
            self.visible = scored.into_iter().map(|(_, i)| i).collect();
        }
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
    }

    fn sel_for(&self, path: &Path) -> Option<&Sel> {
        self.selected
            .get(path)
            .or_else(|| self.selected.values().find(|s| s.entry_path == *path))
    }

    fn sel_key_for(&self, path: &Path) -> Option<PathBuf> {
        if self.selected.contains_key(path) {
            return Some(path.to_path_buf());
        }
        self.selected
            .iter()
            .find(|(_, s)| s.entry_path == *path)
            .map(|(k, _)| k.clone())
    }

    /// The runner's exact capacity math over the current selection; advisory
    /// here, authoritative again on the plan screen.
    fn fit_plan(&self) -> Option<crate::plan::ArchivePlan> {
        let media = match &self.probe {
            Probe::Pending => return None,
            Probe::Assumed(m) | Probe::Found(m) => m,
        };
        let input = PlanInput {
            payloads: self.selected.values().map(|s| s.payload.clone()).collect(),
            parity: true,
            redundancy_pct: self.cfg.redundancy_pct,
            headroom_pct: self.cfg.headroom_pct,
            defect_management: self.cfg.defect_management,
        };
        Some(build_plan(&input, media))
    }

    // -- rendering ---------------------------------------------------------

    fn render(&self, frame: &mut Frame) {
        let width = frame.area().width as usize;
        let hints = wrapped(
            self.footer_hint(),
            "",
            "",
            Style::new().fg(DIM),
            width.saturating_sub(2),
        );
        let table = self.selection_rows(width.saturating_sub(4));
        // the browser keeps at least 8 rows; the table yields on tiny windows
        let table_h = match table.len() {
            0 => 0,
            n => ((n + 2) as u16).min(
                frame
                    .area()
                    .height
                    .saturating_sub(2 + 8 + hints.len() as u16),
            ),
        };
        let [header, body, sel, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Fill(1),
            Constraint::Length(table_h),
            Constraint::Length(hints.len() as u16),
        ])
        .areas(frame.area());
        frame.render_widget(Paragraph::new(app_title()), pad(header));

        let title = format!("Pick payloads — {}", display_dir(&self.dir));
        let block = body_block(&title);
        let inner = block.inner(pad(body));
        frame.render_widget(block, pad(body));

        let w = inner.width as usize;
        let mut lines: Vec<Line> = Vec::new();
        lines.extend(self.media_lines(w));
        lines.extend(self.fit_lines(w));
        lines.push(Line::from(""));
        let status_lines = match &self.status {
            None => Vec::new(),
            Some((warn, msg)) => {
                let (head, color) = if *warn {
                    ("  ! ", WARN)
                } else {
                    ("  · ", Color::Gray)
                };
                wrapped(msg, head, "    ", Style::new().fg(color), w)
            }
        };
        let fixed = lines.len()
            + usize::from(self.filtering || !self.filter.is_empty())
            + status_lines.len();
        let room = (inner.height as usize).saturating_sub(fixed).max(1);
        let start = (self.cursor + 1).saturating_sub(room);
        if self.visible.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no entries)",
                Style::new().fg(DIM),
            )));
        }
        for (row, &idx) in self.visible.iter().enumerate().skip(start).take(room) {
            lines.push(self.entry_line(row == self.cursor, &self.entries[idx], w));
        }
        if self.filtering || !self.filter.is_empty() {
            let style = if self.filtering {
                Style::new().fg(Color::White)
            } else {
                Style::new().fg(DIM)
            };
            let cursor = if self.filtering { "▏" } else { "" };
            lines.push(Line::from(Span::styled(
                format!("  /{}{cursor}", self.filter),
                style,
            )));
        }
        lines.extend(status_lines);
        frame.render_widget(Paragraph::new(lines), inner);

        if table_h > 0 {
            let sel_block = body_block("Selected");
            let sel_inner = sel_block.inner(pad(sel));
            frame.render_widget(sel_block, pad(sel));
            frame.render_widget(Paragraph::new(table), sel_inner);
        }

        frame.render_widget(Paragraph::new(hints), pad(footer));
    }

    fn media_lines(&self, width: usize) -> Vec<Line<'static>> {
        let (text, style) = match &self.probe {
            Probe::Pending => ("probing drive…".to_string(), Style::new().fg(DIM)),
            Probe::Assumed(_) => (
                "no disc detected · assuming a blank BD-R 25".to_string(),
                Style::new().fg(DIM),
            ),
            Probe::Found(m) => (
                format!("{} · {} free", m.kind.label(), human_bytes(m.free_bytes)),
                Style::new().fg(Color::White),
            ),
        };
        wrapped(&text, "  ", "    ", style, width)
    }

    fn fit_lines(&self, width: usize) -> Vec<Line<'static>> {
        if self.selected.is_empty() {
            return wrapped(
                "nothing selected",
                "  ",
                "    ",
                Style::new().fg(DIM),
                width,
            );
        }
        let n = self.selected.len();
        let (text, style) = match self.fit_plan() {
            None => {
                let total: u64 = self.selected.values().map(|s| s.payload.total_size).sum();
                (
                    format!("{n} selected · {}", human_bytes(total)),
                    Style::new().fg(Color::White),
                )
            }
            Some(plan) => {
                let (verdict, color) = if plan.fits {
                    ("fits", OK)
                } else {
                    ("over budget", ERR)
                };
                (
                    format!(
                        "{n} selected · {} · est {} of {} — {verdict}",
                        human_bytes(plan.payload_bytes),
                        human_bytes(plan.total_bytes_est),
                        human_bytes(plan.budget),
                    ),
                    Style::new().fg(color),
                )
            }
        };
        wrapped(&text, "  ", "    ", style, width)
    }

    /// Bottom table of the selection: full path, payload bytes, share of the
    /// disc budget. One row per payload, oldest-path order (BTreeMap).
    fn selection_rows(&self, width: usize) -> Vec<Line<'static>> {
        if self.selected.is_empty() {
            return Vec::new();
        }
        const MAX_ROWS: usize = 8;
        let budget = self.fit_plan().map(|p| p.budget).filter(|b| *b > 0);
        let meta_w = "  0000.00 GiB  00.0%".chars().count();
        let path_w = width.saturating_sub(meta_w + 2).max(8);
        let mut lines = vec![Line::from(Span::styled(
            format!("  {:<path_w$}  {:>10}  {:>5}", "path", "size", "used"),
            Style::new().fg(DIM),
        ))];
        for s in self.selected.values().take(MAX_ROWS) {
            let size = human_bytes(s.payload.total_size);
            let pct = match budget {
                Some(b) => format!("{:.1}%", 100.0 * s.payload.total_size as f64 / b as f64),
                None => "—".into(),
            };
            let path = tail_fit(&display_dir(&s.payload.root), path_w);
            lines.push(Line::from(vec![
                Span::styled(format!("  {path:<path_w$}"), Style::new().fg(Color::White)),
                Span::styled(format!("  {size:>10}  {pct:>5}"), Style::new().fg(DIM)),
            ]));
        }
        let extra = self.selected.len().saturating_sub(MAX_ROWS);
        if extra > 0 {
            lines.push(Line::from(Span::styled(
                format!("  … and {extra} more"),
                Style::new().fg(DIM),
            )));
        }
        lines
    }

    fn entry_line(&self, current: bool, e: &Entry, width: usize) -> Line<'static> {
        let checked = self.sel_for(&e.path).is_some();
        let mark = match e.kind {
            EntryKind::Inert => "    ".to_string(),
            _ => format!("[{}] ", if checked { "x" } else { " " }),
        };
        let prefix = format!(" {} {mark}", if current { "▸" } else { " " });
        let name = match e.kind {
            EntryKind::Dir => format!("{}/", e.name),
            _ => e.name.clone(),
        };
        let (size_text, container) = match &e.kind {
            EntryKind::File { size, container } => (human_bytes(*size), *container),
            EntryKind::Dir => match self.sel_for(&e.path) {
                Some(s) => (
                    format!(
                        "{} ({} files)",
                        human_bytes(s.payload.total_size),
                        s.payload.files.len()
                    ),
                    false,
                ),
                None => (String::new(), false),
            },
            EntryKind::Inert => (String::new(), false),
        };
        let tag = if container { "  container" } else { "" };
        let meta = format!("{size_text}{tag}");
        let name_w = width
            .saturating_sub(prefix.chars().count() + meta.chars().count() + 2)
            .max(8);
        let name = if name.chars().count() > name_w {
            let mut cut: String = name.chars().take(name_w.saturating_sub(1)).collect();
            cut.push('…');
            cut
        } else {
            format!("{name:<name_w$}")
        };
        let base = if e.kind == EntryKind::Inert {
            Style::new().fg(DIM)
        } else if checked {
            Style::new().fg(OK)
        } else {
            Style::new().fg(Color::White)
        };
        let base = if current {
            base.add_modifier(Modifier::BOLD).fg(ACCENT)
        } else {
            base
        };
        let mut spans = vec![
            Span::styled(format!("{prefix}{name}"), base),
            Span::styled(size_text, Style::new().fg(DIM)),
        ];
        if !tag.is_empty() {
            spans.push(Span::styled(tag.to_string(), Style::new().fg(WARN)));
        }
        Line::from(spans)
    }

    fn footer_hint(&self) -> &'static str {
        if self.filtering {
            "type to filter · ↑↓ move · Enter keep · Esc clear"
        } else {
            "↑↓ move · Space select · →/← open/up · / filter · . hidden · Enter confirm · q cancel"
        }
    }
}

fn read_entries(dir: &Path) -> Result<Vec<Entry>> {
    let listing = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("cannot read directory {}: {e}", dir.display()))?;
    let mut entries = Vec::new();
    for entry in listing {
        let entry =
            entry.map_err(|e| anyhow::anyhow!("cannot read directory {}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let kind = match entry.file_type() {
            Err(_) => EntryKind::Inert,
            // follow symlinks: a link the user points at is explicit intent,
            // same as Payload::inspect canonicalizing an argv path
            Ok(ft) if ft.is_symlink() => match std::fs::metadata(&path) {
                Ok(m) if m.is_dir() => EntryKind::Dir,
                Ok(m) if m.is_file() => EntryKind::File {
                    size: m.len(),
                    container: container_heuristic(&path, m.len()),
                },
                _ => EntryKind::Inert,
            },
            Ok(ft) if ft.is_dir() => EntryKind::Dir,
            Ok(ft) if ft.is_file() => {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                EntryKind::File {
                    size,
                    container: container_heuristic(&path, size),
                }
            }
            Ok(_) => EntryKind::Inert,
        };
        entries.push(Entry { name, path, kind });
    }
    entries.sort_by(|a, b| {
        let da = a.kind == EntryKind::Dir;
        let db = b.kind == EntryKind::Dir;
        db.cmp(&da).then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

/// Case-insensitive subsequence match; lower score wins. Gaps dominate,
/// then first-hit position, then name length. None = no match.
fn fuzzy_match(query: &str, name: &str) -> Option<u32> {
    let hay: Vec<char> = name.to_lowercase().chars().collect();
    let mut pos = 0usize;
    let mut first = 0usize;
    let mut gaps = 0usize;
    let mut prev: Option<usize> = None;
    for qc in query.to_lowercase().chars() {
        let at = pos + hay[pos..].iter().position(|c| *c == qc)?;
        match prev {
            None => first = at,
            Some(p) => gaps += at - p - 1,
        }
        prev = Some(at);
        pos = at + 1;
    }
    Some((gaps * 16 + first * 2 + hay.len() / 8) as u32)
}

fn wrapped(text: &str, head: &str, cont: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    head_wrap(head, cont, text, width)
        .into_iter()
        .map(|row| Line::from(Span::styled(row, style)))
        .collect()
}

/// Keep the tail of a long path: the leaf name matters more than the root.
fn tail_fit(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n <= width {
        return text.to_string();
    }
    let tail: String = text.chars().skip(n + 1 - width).collect();
    format!("…{tail}")
}

fn display_dir(dir: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rest) = dir.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    dir.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FileConfig;
    use crate::plan::MediaKind;

    fn write(path: &Path, len: usize) {
        std::fs::write(path, vec![0u8; len]).unwrap();
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("vault.hc"), 4096);
        write(&root.join("notes.md"), 10);
        write(&root.join(".hidden"), 1);
        std::fs::create_dir(root.join("photos")).unwrap();
        write(&root.join("photos").join("a.jpg"), 100);
        write(&root.join("photos").join("b.jpg"), 200);
        std::fs::create_dir(root.join("photos").join("raw")).unwrap();
        write(&root.join("photos").join("raw").join("c.raw"), 300);
        dir
    }

    fn picker(dir: &Path) -> Picker {
        Picker::new(
            crate::config::Config::resolve(FileConfig::default()).unwrap(),
            dir.to_path_buf(),
        )
        .unwrap()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn bd25() -> MediaInfo {
        MediaInfo {
            kind: MediaKind::BdR25,
            profile: "BD-R".into(),
            blank: true,
            formatted: false,
            free_bytes: crate::plan::BD_R_25,
            formatted_capacity: None,
            speeds: vec![],
            media_id: None,
        }
    }

    fn visible_names(p: &Picker) -> Vec<String> {
        p.visible
            .iter()
            .map(|&i| p.entries[i].name.clone())
            .collect()
    }

    fn cursor_to(p: &mut Picker, name: &str) {
        let pos = p
            .visible
            .iter()
            .position(|&i| p.entries[i].name == name)
            .unwrap_or_else(|| panic!("{name} not visible: {:?}", visible_names(p)));
        p.cursor = pos;
    }

    #[test]
    fn fuzzy_match_is_case_insensitive_subsequence() {
        assert!(fuzzy_match("VLT", "vault.hc").is_some());
        assert!(fuzzy_match("vlt", "VAULT.HC").is_some());
        assert!(fuzzy_match("xyz", "vault.hc").is_none());
    }

    #[test]
    fn fuzzy_match_prefers_contiguous_and_early_hits() {
        assert!(fuzzy_match("au", "vault").unwrap() < fuzzy_match("au", "abcu").unwrap());
        assert!(fuzzy_match("a", "apple").unwrap() < fuzzy_match("a", "banana").unwrap());
    }

    #[test]
    fn read_entries_sorts_dirs_first_and_flags_containers() {
        let dir = tree();
        let entries = read_entries(dir.path()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["photos", ".hidden", "notes.md", "vault.hc"]);
        assert!(entries.iter().any(|e| e.name == "vault.hc"
            && matches!(
                e.kind,
                EntryKind::File {
                    container: true,
                    ..
                }
            )));
    }

    #[test]
    fn hidden_entries_are_off_by_default_and_toggle_with_dot() {
        let dir = tree();
        let mut p = picker(dir.path());
        assert!(!visible_names(&p).contains(&".hidden".to_string()));
        p.on_key(key(KeyCode::Char('.')));
        assert!(visible_names(&p).contains(&".hidden".to_string()));
    }

    #[test]
    fn cursor_clamps_at_both_ends() {
        let dir = tree();
        let mut p = picker(dir.path());
        p.on_key(key(KeyCode::Up));
        assert_eq!(p.cursor, 0);
        for _ in 0..20 {
            p.on_key(key(KeyCode::Char('j')));
        }
        assert_eq!(p.cursor, p.visible.len() - 1);
    }

    #[test]
    fn space_toggles_selection_on_and_off() {
        let dir = tree();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "vault.hc");
        p.on_key(key(KeyCode::Char(' ')));
        assert_eq!(p.selected.len(), 1);
        p.on_key(key(KeyCode::Char(' ')));
        assert!(p.selected.is_empty());
    }

    #[test]
    fn selection_survives_navigation_and_filtering() {
        let dir = tree();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "vault.hc");
        p.on_key(key(KeyCode::Char(' ')));
        cursor_to(&mut p, "photos");
        p.on_key(key(KeyCode::Right));
        assert!(visible_names(&p).contains(&"a.jpg".to_string()));
        p.on_key(key(KeyCode::Left));
        assert_eq!(p.selected.len(), 1);
        p.on_key(key(KeyCode::Char('/')));
        p.on_key(key(KeyCode::Char('n')));
        assert_eq!(visible_names(&p), vec!["notes.md"]);
        assert_eq!(p.selected.len(), 1);
    }

    #[test]
    fn ascend_places_cursor_on_the_dir_we_left() {
        let dir = tree();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "photos");
        p.on_key(key(KeyCode::Right));
        p.on_key(key(KeyCode::Left));
        assert_eq!(p.current().unwrap().name, "photos");
    }

    #[test]
    fn descend_on_file_is_a_noop() {
        let dir = tree();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "vault.hc");
        let before = p.dir.clone();
        p.on_key(key(KeyCode::Right));
        assert_eq!(p.dir, before);
    }

    #[test]
    fn selecting_a_dir_absorbs_nested_selections() {
        let dir = tree();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "photos");
        p.on_key(key(KeyCode::Right));
        cursor_to(&mut p, "a.jpg");
        p.on_key(key(KeyCode::Char(' ')));
        p.on_key(key(KeyCode::Left));
        cursor_to(&mut p, "photos");
        p.on_key(key(KeyCode::Char(' ')));
        assert_eq!(p.selected.len(), 1);
        assert!(p.selected.keys().next().unwrap().ends_with("photos"));
    }

    #[test]
    fn selecting_under_a_selected_ancestor_refuses() {
        let dir = tree();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "photos");
        p.on_key(key(KeyCode::Char(' ')));
        p.on_key(key(KeyCode::Right));
        cursor_to(&mut p, "a.jpg");
        p.on_key(key(KeyCode::Char(' ')));
        assert_eq!(p.selected.len(), 1);
        let (warn, msg) = p.status.clone().unwrap();
        assert!(warn);
        assert!(msg.contains("already included via photos"), "{msg}");
    }

    #[test]
    fn toggling_a_symlink_alias_of_a_selected_dir_deselects_it() {
        let dir = tree();
        std::os::unix::fs::symlink(dir.path().join("photos"), dir.path().join("pics")).unwrap();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "photos");
        p.on_key(key(KeyCode::Char(' ')));
        assert_eq!(p.selected.len(), 1);
        cursor_to(&mut p, "pics");
        p.on_key(key(KeyCode::Char(' ')));
        assert!(p.selected.is_empty());
    }

    #[test]
    fn toggling_an_empty_dir_reports_the_inspect_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "empty");
        p.on_key(key(KeyCode::Char(' ')));
        assert!(p.selected.is_empty());
        let (warn, msg) = p.status.clone().unwrap();
        assert!(warn);
        assert!(msg.contains("no files"), "{msg}");
    }

    #[test]
    fn filter_mode_takes_q_literally_and_esc_clears_without_cancelling() {
        let dir = tree();
        let mut p = picker(dir.path());
        p.on_key(key(KeyCode::Char('/')));
        p.on_key(key(KeyCode::Char('q')));
        assert!(p.outcome.is_none());
        assert_eq!(p.filter, "q");
        p.on_key(key(KeyCode::Esc));
        assert!(p.outcome.is_none());
        assert!(p.filter.is_empty());
        assert!(!p.filtering);
        assert!(visible_names(&p).len() > 1);
    }

    #[test]
    fn filter_enter_commits_and_normal_keys_return() {
        let dir = tree();
        let mut p = picker(dir.path());
        p.on_key(key(KeyCode::Char('/')));
        p.on_key(key(KeyCode::Char('v')));
        p.on_key(key(KeyCode::Enter));
        assert!(!p.filtering);
        assert_eq!(p.filter, "v");
        assert_eq!(visible_names(&p), vec!["vault.hc"]);
    }

    #[test]
    fn enter_with_empty_selection_picks_the_cursor_entry() {
        let dir = tree();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "vault.hc");
        p.on_key(key(KeyCode::Enter));
        match p.outcome {
            Some(Outcome::Confirmed(ref paths)) => {
                assert_eq!(paths.len(), 1);
                assert!(paths[0].ends_with("vault.hc"));
            }
            _ => panic!("expected confirm"),
        }
    }

    #[test]
    fn enter_confirms_the_selected_set_sorted() {
        let dir = tree();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "vault.hc");
        p.on_key(key(KeyCode::Char(' ')));
        cursor_to(&mut p, "notes.md");
        p.on_key(key(KeyCode::Char(' ')));
        p.on_key(key(KeyCode::Enter));
        match p.outcome {
            Some(Outcome::Confirmed(ref paths)) => {
                assert_eq!(paths.len(), 2);
                assert!(paths[0].ends_with("notes.md"));
                assert!(paths[1].ends_with("vault.hc"));
            }
            _ => panic!("expected confirm"),
        }
    }

    #[test]
    fn q_and_esc_cancel_in_normal_mode() {
        let dir = tree();
        let mut p = picker(dir.path());
        p.on_key(key(KeyCode::Char('q')));
        assert!(matches!(p.outcome, Some(Outcome::Cancelled)));
    }

    #[test]
    fn unreadable_dir_descend_keeps_the_listing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tree();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "locked");
        let before = p.dir.clone();
        p.on_key(key(KeyCode::Right));
        assert_eq!(p.dir, before);
        assert!(p.status.as_ref().is_some_and(|(warn, _)| *warn));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn fit_plan_uses_probed_media_and_flags_over_budget() {
        let dir = tree();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "vault.hc");
        p.on_key(key(KeyCode::Char(' ')));
        assert!(p.fit_plan().is_none(), "no fit verdict while probing");
        p.set_media(Some(bd25()));
        assert!(p.fit_plan().unwrap().fits);
        let mut tiny = bd25();
        tiny.free_bytes = 1024;
        p.set_media(Some(tiny));
        assert!(!p.fit_plan().unwrap().fits);
    }

    #[test]
    fn no_disc_falls_back_to_assumed_bd25() {
        let dir = tree();
        let mut p = picker(dir.path());
        p.set_media(None);
        cursor_to(&mut p, "vault.hc");
        p.on_key(key(KeyCode::Char(' ')));
        assert!(p.fit_plan().unwrap().fits);
    }

    fn buffer_text(p: &Picker, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| p.render(f)).unwrap();
        let mut text = String::new();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn tail_fit_keeps_the_leaf_end_of_long_paths() {
        assert_eq!(tail_fit("short", 10), "short");
        let cut = tail_fit("/very/long/path/to/vault.hc", 12);
        assert_eq!(cut.chars().count(), 12);
        assert!(cut.starts_with('…'));
        assert!(cut.ends_with("vault.hc"));
    }

    #[test]
    fn wrapped_splits_long_text_into_continuation_rows() {
        let rows = wrapped(
            "payload directory has no files and this message is long",
            "  ! ",
            "    ",
            Style::new(),
            24,
        );
        assert!(rows.len() > 1);
        let first = format!("{}", rows[0]);
        assert!(first.starts_with("  ! "), "{first}");
    }

    #[test]
    fn selection_table_lists_path_size_and_budget_share() {
        let dir = tree();
        let mut p = picker(dir.path());
        p.set_media(Some(bd25()));
        cursor_to(&mut p, "vault.hc");
        p.on_key(key(KeyCode::Char(' ')));
        cursor_to(&mut p, "photos");
        p.on_key(key(KeyCode::Char(' ')));
        let text = buffer_text(&p, 100, 30);
        assert!(text.contains("Selected"), "{text}");
        assert!(text.contains("used"), "{text}");
        assert!(text.contains("vault.hc"), "{text}");
        assert!(text.contains("photos"), "{text}");
        assert!(text.contains("4.00 KiB"), "{text}");
        assert!(text.contains("0.0%"), "{text}");
    }

    #[test]
    fn selection_table_share_is_dash_while_probing() {
        let dir = tree();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "vault.hc");
        p.on_key(key(KeyCode::Char(' ')));
        let rows = p.selection_rows(80);
        let row = format!("{}", rows[1]);
        assert!(row.contains("vault.hc"), "{row}");
        assert!(row.contains('—'), "{row}");
    }

    #[test]
    fn narrow_window_wraps_hints_and_status_instead_of_clipping() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let mut p = picker(dir.path());
        cursor_to(&mut p, "empty");
        p.on_key(key(KeyCode::Char(' ')));
        let text = buffer_text(&p, 44, 24);
        let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("q cancel"), "{text}");
        assert!(flat.contains("no files"), "{text}");
    }

    #[test]
    fn render_shows_checkboxes_selection_and_filter() {
        let dir = tree();
        let mut p = picker(dir.path());
        p.set_media(Some(bd25()));
        cursor_to(&mut p, "vault.hc");
        p.on_key(key(KeyCode::Char(' ')));
        p.on_key(key(KeyCode::Char('/')));
        p.on_key(key(KeyCode::Char('v')));
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| p.render(f)).unwrap();
        let mut text = String::new();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("[x]"), "{text}");
        assert!(text.contains("vault.hc"), "{text}");
        assert!(text.contains("container"), "{text}");
        assert!(text.contains("1 selected"), "{text}");
        assert!(text.contains("— fits"), "{text}");
        assert!(text.contains("/v▏"), "{text}");
        assert!(text.contains("BD-R 25 GB"), "{text}");
    }
}
