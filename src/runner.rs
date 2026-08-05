use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};

use crate::config::Config;
use crate::plan::{self, human_bytes, ArchivePlan, MediaInfo, Payload, PlanInput};
use crate::tools::Tools;
use crate::{burn, ecc, hashing, master, media, parity, span, verify};

const READY_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    Preflight,
    Split,
    Parity,
    Checksums,
    Format,
    Master,
    Burn,
    VerifyImage,
    VerifyFiles,
    CheckMedia,
}

impl Stage {
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Preflight => "preflight",
            Stage::Split => "split",
            Stage::Parity => "parity",
            Stage::Checksums => "checksums",
            Stage::Format => "format",
            Stage::Master => "master",
            Stage::Burn => "burn",
            Stage::VerifyImage => "verify image",
            Stage::VerifyFiles => "verify files",
            Stage::CheckMedia => "check media",
        }
    }
}

#[derive(Debug, Clone)]
pub enum StageEvent {
    Plan {
        device: String,
        media: MediaInfo,
        plan: ArchivePlan,
        params: BurnParams,
    },
    StageStart(Stage),
    Progress {
        stage: Stage,
        pct: Option<f32>,
        detail: String,
    },
    StageDone {
        stage: Stage,
        summary: String,
    },
    Info(String),
    Warn(String),
    /// Primary command output (info listings): rendered bare, severity
    /// prefixes never apply.
    Out(String),
    /// Runner blocks until an Ack arrives (reinsert disc, confirm burn, ...)
    NeedAck {
        prompt: String,
    },
    /// A multi-disc set moves to its next disc; per-disc stages start over.
    DiscStart {
        index: u32,
        total: u32,
        label: String,
        parity: bool,
    },
    Finished {
        report: RunReport,
    },
    Failed {
        stage: Stage,
        error: String,
    },
}

/// UI -> runner replies for NeedAck prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ack {
    Proceed,
    Abort,
    /// Re-plan with these params and ask again (plan-screen editing).
    Amend(BurnParams),
}

#[derive(Debug, Clone, Default)]
pub struct RunReport {
    pub iso_path: Option<PathBuf>,
    pub iso_sha256: Option<String>,
    pub iso_bytes: u64,
    pub stages: Vec<(Stage, String)>,
    pub reminders: Vec<String>,
    /// Every file the run left on disk, in write order.
    pub written_files: Vec<PathBuf>,
    /// Ways the run's guarantees were weakened but not broken (e.g. buffered
    /// read-back after a physical reload). Rendered as caveats, never silent.
    pub degradations: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BurnRequest {
    pub payloads: Vec<PathBuf>,
    pub label: Option<String>,
    pub parity: bool,
    pub dry_run: bool,
    pub assume_yes: bool,
    /// UI may answer the confirm prompt with Ack::Amend (TUI plan editing).
    /// Line mode leaves this false and keeps the bail-before-prompt behavior.
    pub amend: bool,
    pub discard_iso: bool,
}

/// Per-run tunables resolved once from Config + BurnRequest. The confirm loop
/// owns them; the TUI may amend them until Proceed. Never written to config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BurnParams {
    pub label: String,
    pub speed: Option<u32>,
    pub redundancy_pct: u32,
    pub parity: bool,
    pub defect_management: bool,
    pub staging: PathBuf,
}

impl BurnParams {
    pub fn resolve(cfg: &Config, req: &BurnRequest) -> Self {
        Self {
            label: sanitize_label(req.label.as_deref().unwrap_or(&default_label())),
            speed: cfg.speed,
            redundancy_pct: cfg.redundancy_pct,
            parity: req.parity,
            defect_management: cfg.defect_management,
            staging: cfg.staging.clone(),
        }
    }

    fn canonicalize(mut self) -> (Self, Vec<String>) {
        let mut warns = Vec::new();
        let clean = sanitize_label(&self.label);
        if clean != self.label {
            warns.push(format!("label sanitized to {clean}"));
            self.label = clean;
        }
        if !(1..=100).contains(&self.redundancy_pct) {
            let clamped = self.redundancy_pct.clamp(1, 100);
            warns.push(format!(
                "redundancy {}% out of range, using {clamped}%",
                self.redundancy_pct
            ));
            self.redundancy_pct = clamped;
        }
        if self.speed == Some(0) {
            warns.push("speed 0 treated as drive default".into());
            self.speed = None;
        }
        let expanded = crate::config::expand_tilde(&self.staging);
        if expanded != self.staging {
            warns.push(format!("staging expanded to {}", expanded.display()));
            self.staging = expanded;
        }
        (self, warns)
    }

    fn plan_input(&self, payloads: &[Payload], headroom_pct: u32) -> PlanInput {
        PlanInput {
            payloads: payloads.to_vec(),
            parity: self.parity,
            redundancy_pct: self.redundancy_pct,
            headroom_pct,
            defect_management: self.defect_management,
        }
    }
}

pub struct RunnerCtx {
    pub cfg: Config,
    pub tools: Tools,
    pub tx: Sender<StageEvent>,
    pub ack_rx: Receiver<Ack>,
    /// When set, every event is also appended to a run.log on disk, so a
    /// crash or power loss mid-burn leaves a forensic record.
    tee: std::sync::Mutex<Option<RunLog>>,
}

impl RunnerCtx {
    pub fn new(cfg: Config, tools: Tools, tx: Sender<StageEvent>, ack_rx: Receiver<Ack>) -> Self {
        Self {
            cfg,
            tools,
            tx,
            ack_rx,
            tee: std::sync::Mutex::new(None),
        }
    }

    /// Start teeing events to `path` (append, dated header). Best effort: a
    /// run log that cannot be opened warns but never fails the run.
    fn tee_events_to(&self, path: &Path) {
        match RunLog::open(path) {
            Ok(log) => {
                if let Ok(mut tee) = self.tee.lock() {
                    *tee = Some(log);
                }
            }
            Err(e) => self.warn(format!("no run log at {}: {e:#}", path.display())),
        }
    }

    pub fn send(&self, ev: StageEvent) {
        if let Ok(mut tee) = self.tee.lock() {
            if let Some(log) = tee.as_mut() {
                log.record(&ev);
            }
        }
        let _ = self.tx.send(ev);
    }

    /// Emit NeedAck and block for the raw reply (confirm loop only).
    fn ask_raw(&self, prompt: &str) -> Result<Ack> {
        self.send(StageEvent::NeedAck {
            prompt: prompt.to_string(),
        });
        match self.ack_rx.recv() {
            Ok(ack) => Ok(ack),
            Err(_) => anyhow::bail!("ui channel closed"),
        }
    }

    /// Emit NeedAck and block for the reply; Err on Abort or closed channel.
    pub fn ask(&self, prompt: &str) -> Result<()> {
        match self.ask_raw(prompt)? {
            Ack::Proceed => Ok(()),
            Ack::Abort => anyhow::bail!("aborted by user"),
            Ack::Amend(_) => anyhow::bail!("unexpected parameter amendment"),
        }
    }

    fn start(&self, stage: Stage) {
        self.send(StageEvent::StageStart(stage));
    }

    fn done(&self, stages: &mut Vec<(Stage, String)>, stage: Stage, summary: String) {
        self.send(StageEvent::StageDone {
            stage,
            summary: summary.clone(),
        });
        stages.push((stage, summary));
    }

    fn progress(&self, stage: Stage, pct: Option<f32>, detail: String) {
        self.send(StageEvent::Progress { stage, pct, detail });
    }

    fn info(&self, text: String) {
        self.send(StageEvent::Info(text));
    }

    fn warn(&self, text: String) {
        self.send(StageEvent::Warn(text));
    }

    fn out(&self, text: String) {
        self.send(StageEvent::Out(text));
    }
}

/// Append-mode event log: one dated header per run, then timestamped lines
/// for stage transitions, info/warnings, failures, and decile progress steps
/// (full progress would be megabytes of \r spam).
struct RunLog {
    file: std::fs::File,
    last_decile: Option<(Stage, u32)>,
}

impl RunLog {
    fn open(path: &Path) -> std::io::Result<Self> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)?;
        writeln!(
            file,
            "=== run {} (ovenmitts {}) ===",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
            env!("CARGO_PKG_VERSION")
        )?;
        Ok(Self {
            file,
            last_decile: None,
        })
    }

    fn record(&mut self, ev: &StageEvent) {
        use std::io::Write as _;
        let line = match ev {
            StageEvent::StageStart(stage) => Some(format!("[{}] start", stage.label())),
            StageEvent::StageDone { stage, summary } => {
                Some(format!("[{}] done - {summary}", stage.label()))
            }
            StageEvent::Progress {
                stage,
                pct: Some(pct),
                detail,
            } => {
                let decile = (*pct / 10.0) as u32;
                if self.last_decile == Some((*stage, decile)) {
                    None
                } else {
                    self.last_decile = Some((*stage, decile));
                    Some(format!("[{}] {pct:5.1}% {detail}", stage.label()))
                }
            }
            // percent-less lines are the interesting ones during a stall
            // ("(no tool output for Ns)", raw tool notes) - keep them all
            StageEvent::Progress {
                stage,
                pct: None,
                detail,
            } => Some(format!("[{}] {detail}", stage.label())),
            StageEvent::Info(t) => Some(format!("info: {t}")),
            StageEvent::Warn(t) => Some(format!("warning: {t}")),
            StageEvent::DiscStart {
                index,
                total,
                label,
                parity,
            } => Some(format!(
                "=== disc {index} of {total} - {label}{} ===",
                if *parity { " (parity)" } else { "" }
            )),
            StageEvent::Failed { stage, error } => {
                Some(format!("error: [{}] {error}", stage.label()))
            }
            _ => None,
        };
        if let Some(line) = line {
            let _ = writeln!(
                self.file,
                "{} {line}",
                chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
            );
        }
    }
}

// Guarantee the tx sees Failed on any error (no silent death).
fn with_failure(ctx: &RunnerCtx, f: impl FnOnce(&mut Stage) -> Result<()>) -> Result<()> {
    let mut stage = Stage::Preflight;
    let res = f(&mut stage);
    if let Err(e) = &res {
        ctx.send(StageEvent::Failed {
            stage,
            error: format!("{e:#}"),
        });
    }
    res
}

/// Full pipeline. Emits Plan first; unless assume_yes, asks for confirmation
/// after the plan. On success emits Finished with the report.
pub fn run_burn(ctx: &RunnerCtx, req: &BurnRequest) -> Result<()> {
    with_failure(ctx, |stage| burn_pipeline(ctx, req, stage))
}

// The sequencer: each stage is a BurnRun method (or a shared *_stage helper
// also used by verify/burn-iso); this function only orders them and keeps the
// failure-attribution pointer current.
fn burn_pipeline(ctx: &RunnerCtx, req: &BurnRequest, stage: &mut Stage) -> Result<()> {
    *stage = Stage::Preflight;
    ctx.start(Stage::Preflight);
    let (payloads, device, media) = preflight_probe(ctx, &req.payloads)?;
    let mut run = BurnRun::new(ctx);
    let Some((params, plan)) = run.confirm(req, &payloads, &device, &media)? else {
        return Ok(()); // dry run: plan rendered, Finished already sent
    };
    if let Some(span) = plan.span.clone() {
        let cx = SetContext {
            req,
            params: &params,
            plan: &plan,
            span: &span,
            device: &device,
            media: &media,
            payloads: &payloads,
        };
        return run.burn_set(stage, &cx);
    }
    let staging = run.staging(&params)?;

    *stage = Stage::Parity;
    let parity_files = run.parity(&payloads, &params, &staging)?;

    *stage = Stage::Checksums;
    let sums = run.checksums(&payloads, &parity_files, &params, &staging)?;

    *stage = Stage::Master;
    let mastered = run.master(&plan, &params, &staging, &payloads, &parity_files, &sums)?;

    if params.defect_management {
        *stage = Stage::Format;
        run.format(&device, mastered.iso_bytes)?;
    }

    *stage = Stage::Burn;
    run.burn(&device, params.speed, &mastered)?;

    *stage = Stage::VerifyImage;
    ctx.start(Stage::VerifyImage);
    // The burn command carried -eject and readback waits for the reloaded
    // medium, so a physical reload always defeated the cache here.
    verify_image_stage(
        ctx,
        &mut run.stages,
        &device,
        mastered.iso_bytes,
        &mastered.iso_sha,
        true,
        &mut run.degradations,
    )?;

    *stage = Stage::VerifyFiles;
    verify_files_stage(
        ctx,
        &mut run.stages,
        &device,
        EntriesSource::InMemory(&sums.entries),
    )?;

    // req.amend is only ever set by the TUI, so it doubles as "an operator is
    // present to take the disc"; unattended runs must not leave the tray open
    eject_if_configured(ctx, &device, req.amend);
    run.finish(req, &payloads, staging, parity_files, sums, mastered)
}

/// Staging directory for one burn: unique label, created tree, run-log tee.
struct StagingDir {
    label: String,
    dir: PathBuf,
    run_log: PathBuf,
}

/// Checksums stage output: sha256 entries in disc layout plus the manifest
/// rows derived while hashing.
struct ChecksumsOut {
    entries: Vec<(String, String)>,
    manifest_rows: Vec<master::ManifestEntry>,
    path: PathBuf,
}

/// Everything the master stage leaves in staging.
struct Mastered {
    iso: PathBuf,
    iso_bytes: u64,
    iso_sha: String,
    manifest_path: PathBuf,
    recovery_path: PathBuf,
    lba_path: PathBuf,
    iso_sha_path: PathBuf,
}

/// One full-pipeline burn: accumulates the stage summaries and degradations
/// the final report carries.
struct BurnRun<'a> {
    ctx: &'a RunnerCtx,
    stages: Vec<(Stage, String)>,
    degradations: Vec<String>,
}

impl<'a> BurnRun<'a> {
    fn new(ctx: &'a RunnerCtx) -> Self {
        Self {
            ctx,
            stages: Vec::new(),
            degradations: Vec::new(),
        }
    }

    fn done(&mut self, stage: Stage, summary: String) {
        self.ctx.done(&mut self.stages, stage, summary);
    }

    /// Confirm loop: the plan on screen is always one this loop computed for
    /// the params it holds; Amend re-plans (pure — media stays probed once).
    /// Ok(None) = dry run (plan shown, Finished sent).
    fn confirm(
        &mut self,
        req: &BurnRequest,
        payloads: &[Payload],
        device: &str,
        media: &MediaInfo,
    ) -> Result<Option<(BurnParams, ArchivePlan)>> {
        let ctx = self.ctx;
        let mut params = BurnParams::resolve(&ctx.cfg, req);
        let mut prev_warnings: Vec<String> = Vec::new();
        let mut prev_staging_warn: Option<String> = None;
        let mut staging_note_pending = true;
        let (params, plan) = loop {
            let mut plan =
                plan::build_plan(&params.plan_input(payloads, ctx.cfg.headroom_pct), media);
            if !plan.fits {
                // ecc flag only sizes the estimates: the RS02 layer fills
                // every set image to the budget when it will actually run
                let ecc = ctx.cfg.ecc && ctx.tools.dvdisaster.is_some();
                plan.span = span::plan_span(
                    payloads,
                    &params.label,
                    plan.budget,
                    plan.overhead_bytes_est,
                    params.parity,
                    ecc,
                )?
                .map(Box::new);
            }
            if staging_note_pending {
                staging_note_pending = false;
                let needed = plan.parity_bytes_est + plan.total_bytes_est;
                if let Some(note) = stale_staging_note(&params.staging, needed) {
                    ctx.info(note);
                }
            }
            for w in plan.warnings.iter().filter(|w| !prev_warnings.contains(w)) {
                ctx.warn(w.clone());
            }
            prev_warnings.clone_from(&plan.warnings);
            ctx.send(StageEvent::Plan {
                device: device.to_string(),
                media: media.clone(),
                plan: plan.clone(),
                params: params.clone(),
            });

            if req.dry_run {
                // table BEFORE the staging gate: a dry run on a host without
                // the peak staging space must still show what it would take
                if let Some(span) = &plan.span {
                    for line in span_table(span) {
                        ctx.info(line);
                    }
                }
                confirm_gate(ctx, &plan, &params.staging)?;
                ctx.info("dry run - stopping after plan".into());
                ctx.send(StageEvent::Finished {
                    report: RunReport::default(),
                });
                return Ok(None);
            }
            if req.assume_yes {
                confirm_gate(ctx, &plan, &params.staging)?;
                break (params, plan);
            }
            if !req.amend {
                confirm_gate(ctx, &plan, &params.staging)?;
            } else if let Err(e) = check_staging_space(&plan, &params.staging) {
                // surfaced pre-confirm: lowering redundancy shrinks the need
                let msg = format!("{e:#}");
                if prev_staging_warn.as_deref() != Some(msg.as_str()) {
                    ctx.warn(msg.clone());
                    prev_staging_warn = Some(msg);
                }
            }

            let prompt = if let Some(span) = &plan.span {
                let data = span.discs.iter().filter(|d| d.part.is_some()).count();
                format!(
                    "does not fit one disc - spans {} x {} ({} data + {} parity; parity \
                     computation is hours-scale; staging peaks at ~{}); every disc swap \
                     will prompt - burn the set?",
                    span.discs.len(),
                    media.kind.label(),
                    data,
                    span.discs.len() - data,
                    human_bytes(span.staging_peak)
                )
            } else if plan.fits {
                format!(
                    "burn {} to {} ({})?",
                    human_bytes(plan.total_bytes_est),
                    device,
                    media.kind.label()
                )
            } else {
                format!(
                    "does not fit: total {} exceeds budget {} — adjust parameters or abort",
                    human_bytes(plan.total_bytes_est),
                    human_bytes(plan.budget)
                )
            };
            match ctx.ask_raw(&prompt)? {
                Ack::Proceed => {
                    confirm_gate(ctx, &plan, &params.staging)?;
                    break (params, plan);
                }
                Ack::Abort => anyhow::bail!("aborted by user"),
                Ack::Amend(p) => {
                    let (p, warns) = p.canonicalize();
                    for w in warns {
                        ctx.warn(w);
                    }
                    if let Some(s) = p.speed {
                        if !media.speeds.is_empty()
                            && !media.speeds.iter().any(|x| x.round() as u32 == s)
                        {
                            ctx.warn(format!("{s}x is not in the probed write speeds"));
                        }
                    }
                    params = p;
                }
            }
        };

        self.done(
            Stage::Preflight,
            format!(
                "{}: payload {} + parity ~{} fits {} budget",
                media.kind.label(),
                human_bytes(plan.payload_bytes),
                human_bytes(plan.parity_bytes_est),
                human_bytes(plan.budget)
            ),
        );
        Ok(Some((params, plan)))
    }

    fn staging(&mut self, params: &BurnParams) -> Result<StagingDir> {
        let (label, dir) = claim_stage_dir(&params.staging, &params.label)?;
        std::fs::create_dir(dir.join("parity"))
            .with_context(|| format!("create staging dir {}", dir.join("parity").display()))?;
        let run_log = dir.join("run.log");
        self.ctx.tee_events_to(&run_log);
        self.ctx.info(format!("staging into {}", dir.display()));
        Ok(StagingDir {
            label,
            dir,
            run_log,
        })
    }

    fn parity(
        &mut self,
        payloads: &[Payload],
        params: &BurnParams,
        staging: &StagingDir,
    ) -> Result<Vec<PathBuf>> {
        let ctx = self.ctx;
        if !params.parity {
            ctx.warn("parity disabled - a single bad sector can cost the whole payload".into());
            return Ok(Vec::new());
        }
        ctx.start(Stage::Parity);
        let mut parity_files: Vec<PathBuf> = Vec::new();
        let n = payloads.len() as f32;
        for (i, p) in payloads.iter().enumerate() {
            let name = p.name.clone();
            let mut produced = parity::create(
                &ctx.tools,
                p,
                &staging.dir.join("parity"),
                params.redundancy_pct,
                ctx.cfg.stall_timeout(),
                &mut |pct, line| {
                    let overall = pct.map(|v| (i as f32 * 100.0 + v) / n);
                    let detail = if line.is_empty() {
                        name.clone()
                    } else {
                        format!("{name}: {line}")
                    };
                    ctx.progress(Stage::Parity, overall, detail);
                },
            )?;
            parity_files.append(&mut produced);
        }
        let parity_bytes: u64 = parity_files
            .iter()
            .map(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0))
            .sum();
        self.done(
            Stage::Parity,
            format!(
                "{} recovery files, {} ({}% redundancy)",
                parity_files.len(),
                human_bytes(parity_bytes),
                params.redundancy_pct
            ),
        );
        Ok(parity_files)
    }

    fn checksums(
        &mut self,
        payloads: &[Payload],
        parity_files: &[PathBuf],
        params: &BurnParams,
        staging: &StagingDir,
    ) -> Result<ChecksumsOut> {
        let ctx = self.ctx;
        ctx.start(Stage::Checksums);
        let mut entries: Vec<(String, String)> = Vec::new();
        let mut manifest_rows: Vec<master::ManifestEntry> = Vec::new();
        let parity_sizes: Vec<u64> = parity_files
            .iter()
            .map(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0))
            .collect();
        let total: u64 =
            payloads.iter().map(|p| p.total_size).sum::<u64>() + parity_sizes.iter().sum::<u64>();
        let mut base = 0u64;
        let mut th = Throttle::default();
        for p in payloads {
            let mut last_sha = None;
            for m in &p.files {
                let sha = hashing::sha256_file(&m.abs, &mut |done, _| {
                    emit_pct(ctx, Stage::Checksums, &mut th, base + done, total, &m.rel);
                })?;
                base += m.size;
                entries.push((sha.clone(), m.rel.clone()));
                last_sha = Some(sha);
            }
            manifest_rows.push(master::ManifestEntry {
                name: p.name.clone(),
                bytes: p.total_size,
                files: p.files.len(),
                is_dir: p.is_dir,
                sha256: if p.is_dir { None } else { last_sha },
                par2: params.parity.then(|| (p.slice_bytes(), p.parity_blocks())),
            });
        }
        for (f, size) in parity_files.iter().zip(&parity_sizes) {
            let rel = format!("parity/{}", master::file_name_of(f));
            let sha = hashing::sha256_file(f, &mut |done, _| {
                emit_pct(ctx, Stage::Checksums, &mut th, base + done, total, &rel);
            })?;
            base += size;
            entries.push((sha, rel));
        }
        let path = staging.dir.join("checksums.sha256");
        hashing::write_checksums(&entries, &path)?;
        self.done(
            Stage::Checksums,
            format!("{} entries in checksums.sha256", entries.len()),
        );
        Ok(ChecksumsOut {
            entries,
            manifest_rows,
            path,
        })
    }

    fn master(
        &mut self,
        plan: &ArchivePlan,
        params: &BurnParams,
        staging: &StagingDir,
        payloads: &[Payload],
        parity_files: &[PathBuf],
        sums: &ChecksumsOut,
    ) -> Result<Mastered> {
        let ctx = self.ctx;
        ctx.start(Stage::Master);
        // The ISO is the big late allocation: parity above (or a parallel run)
        // may have consumed the space that was free at confirm time.
        check_staging_space_for_iso(plan, &params.staging)?;
        let label = &staging.label;
        // The on-disc docs go INTO the image, so the ECC decision is made on
        // what is knowable now: tool present + enabled ("when free capacity
        // allowed" - RS02 self-identifies, so the wording stays true even
        // when the margin check later declines).
        let ecc_attempted = ctx.cfg.ecc && ctx.tools.dvdisaster.is_some();
        let manifest_path = staging.dir.join("MANIFEST.txt");
        let recovery_path = staging.dir.join("RECOVERY.txt");
        master::write_manifest(
            &manifest_path,
            label,
            &sums.manifest_rows,
            params.parity.then_some(params.redundancy_pct),
            params.defect_management,
            ecc_attempted,
            None,
        )?;
        master::write_recovery(&recovery_path, label, payloads, ecc_attempted, None)?;
        let iso = staging.dir.join(format!("{label}.iso"));
        let input = master::MasterInput {
            label,
            payloads,
            parity_files,
            checksums: &sums.path,
            manifest: &manifest_path,
            recovery: &recovery_path,
            set_txt: None,
            out_iso: &iso,
        };
        let mut iso_bytes = master::build_iso(
            &ctx.tools,
            &input,
            ctx.cfg.stall_timeout(),
            &mut |pct, line| {
                ctx.progress(Stage::Master, pct, line);
            },
        )?;
        // a truncated master (torn write, full disk) must die here, not on a disc
        master::check_iso_truncation(&iso)?;
        iso_bytes = self.ecc_augment(&iso, iso_bytes, plan.budget, &params.staging)?;
        let lba_path = staging.dir.join(format!("{label}.lba.txt"));
        master::report_lba(&ctx.tools, &iso, &lba_path)?;
        let iso_sha = {
            let mut th = Throttle::default();
            hashing::sha256_file(&iso, &mut |done, total| {
                emit_pct(ctx, Stage::Master, &mut th, done, total, "hashing ISO");
            })?
        };
        let iso_sha_path = staging.dir.join(format!("{label}.iso.sha256"));
        hashing::write_checksums(&[(iso_sha.clone(), format!("{label}.iso"))], &iso_sha_path)?;
        self.done(
            Stage::Master,
            format!("{label}.iso {} sha256 {iso_sha}", human_bytes(iso_bytes)),
        );
        Ok(Mastered {
            iso,
            iso_bytes,
            iso_sha,
            manifest_path,
            recovery_path,
            lba_path,
            iso_sha_path,
        })
    }

    fn format(&mut self, device: &str, iso_bytes: u64) -> Result<()> {
        let ctx = self.ctx;
        ctx.start(Stage::Format);
        burn::format_defect_management(
            &ctx.tools,
            device,
            ctx.cfg.stall_timeout(),
            &mut |pct, line| {
                ctx.progress(Stage::Format, pct, line);
            },
        )?;
        let formatted = media::probe(&ctx.tools, device)?;
        let capacity = formatted.formatted_capacity.unwrap_or(formatted.free_bytes);
        ensure!(
            iso_bytes <= capacity,
            "ISO {} ({iso_bytes} bytes) no longer fits: formatted capacity is {} ({capacity} bytes)",
            human_bytes(iso_bytes),
            human_bytes(capacity)
        );
        self.done(
            Stage::Format,
            format!(
                "spare areas formatted, capacity now {}",
                human_bytes(capacity)
            ),
        );
        Ok(())
    }

    fn burn(&mut self, device: &str, speed: Option<u32>, mastered: &Mastered) -> Result<()> {
        let ctx = self.ctx;
        ctx.start(Stage::Burn);
        burn::burn_iso(
            &ctx.tools,
            device,
            &mastered.iso,
            speed,
            ctx.cfg.stall_timeout(),
            &mut |pct, line| {
                ctx.progress(Stage::Burn, pct, line);
            },
        )
        .inspect_err(|_| {
            ctx.info(format!(
                "burn transcript: {}",
                burn::burn_log_path(&mastered.iso).display()
            ));
            ctx.info(format!(
                "staged ISO survives - insert a fresh disc and retry: ovenmitts burn-iso {}",
                mastered.iso.display()
            ));
        })?;
        self.done(
            Stage::Burn,
            format!("{} written, disc ejected", human_bytes(mastered.iso_bytes)),
        );
        Ok(())
    }

    fn finish(
        self,
        req: &BurnRequest,
        payloads: &[Payload],
        staging: StagingDir,
        parity_files: Vec<PathBuf>,
        sums: ChecksumsOut,
        mastered: Mastered,
    ) -> Result<()> {
        let ctx = self.ctx;
        let label = &staging.label;
        let mut reminders = Vec::new();
        let keep_iso = !req.discard_iso;
        if keep_iso {
            reminders.push(format!(
                "second copy: insert a fresh disc and run `ovenmitts burn-iso {}`",
                mastered.iso.display()
            ));
        } else {
            match std::fs::remove_file(&mastered.iso) {
                Ok(()) => ctx.info(format!(
                    "discarded {} after successful verification",
                    mastered.iso.display()
                )),
                // the archive is verified; a leftover ISO must not turn the
                // run into a failure that reads as a bad burn
                Err(e) => ctx.warn(format!(
                    "could not discard {}: {e:#} - remove it manually",
                    mastered.iso.display()
                )),
            }
        }
        reminders.push(format!(
            "keep {} off-disc: parity, {label}.lba.txt and checksums.sha256 are what repair a damaged disc",
            staging.dir.display()
        ));
        if payloads.iter().any(|p| p.looks_like_container()) {
            reminders.push(
                "VeraCrypt: keep an EXTERNAL volume-header backup (Tools > Backup Volume Header); \
                 create a fresh container per archive generation"
                    .into(),
            );
        }
        let mut written_files = Vec::new();
        if staging.run_log.is_file() {
            written_files.push(staging.run_log);
        }
        written_files.extend(parity_files);
        written_files.push(sums.path);
        written_files.push(mastered.manifest_path);
        written_files.push(mastered.recovery_path);
        let burn_log = burn::burn_log_path(&mastered.iso);
        if keep_iso {
            written_files.push(mastered.iso.clone());
        }
        written_files.push(mastered.lba_path);
        written_files.push(mastered.iso_sha_path);
        if burn_log.is_file() {
            written_files.push(burn_log);
        }
        let report_path = staging.dir.join(format!("{label}.report.txt"));
        written_files.push(report_path.clone());
        let report = RunReport {
            iso_path: keep_iso.then_some(mastered.iso),
            iso_sha256: Some(mastered.iso_sha),
            iso_bytes: mastered.iso_bytes,
            stages: self.stages,
            reminders,
            written_files,
            degradations: self.degradations,
        };
        send_finished(ctx, report, &report_path);
        Ok(())
    }

    /// RS02 augmentation decision + run for one mastered image; returns the
    /// (possibly grown) size. Shared by the single-disc master stage and
    /// every disc of a set.
    fn ecc_augment(
        &mut self,
        iso: &Path,
        iso_bytes: u64,
        budget: u64,
        staging: &Path,
    ) -> Result<u64> {
        let ctx = self.ctx;
        let Some(dvdisaster) = ctx.tools.dvdisaster.as_ref().filter(|_| ctx.cfg.ecc) else {
            if ctx.cfg.ecc {
                ctx.info(
                    "dvdisaster not found - no sector-level ECC layer on this disc \
                     (install the speed47 fork; par2 parity remains)"
                        .into(),
                );
            }
            return Ok(iso_bytes);
        };
        match ecc::augment_target(iso_bytes, budget, staging_free_bytes(staging)?) {
            Some(target) => {
                ctx.info(format!(
                    "embedding RS02 sector ECC (dvdisaster): filling the image to \
                     {target} sectors"
                ));
                let after = ecc::augment(
                    dvdisaster,
                    iso,
                    target,
                    ctx.cfg.stall_timeout(),
                    &mut |line| {
                        let l = line.trim();
                        if !l.is_empty() {
                            ctx.progress(Stage::Master, None, l.to_string());
                        }
                    },
                )?;
                // descriptors must still parse after in-place augmentation
                master::check_iso_truncation(iso)?;
                ctx.info(format!(
                    "RS02 ECC embedded - image now {}",
                    human_bytes(after)
                ));
                Ok(after)
            }
            None => {
                ctx.warn(
                    "not enough free disc or staging space for a meaningful RS02 \
                     ECC layer (needs >=5% of the image) - skipped"
                        .into(),
                );
                Ok(iso_bytes)
            }
        }
    }

    /// Multi-disc set: prepare EVERYTHING first (split, the one par2 set,
    /// per-disc docs, every image), then burn disc by disc behind swap
    /// prompts. After Phase A each disc is exactly a burn-iso - that is also
    /// the resume story: any failure mid-set lists one `ovenmitts burn-iso`
    /// line per remaining disc, and no state file exists to go stale.
    fn burn_set(&mut self, stage: &mut Stage, cx: &SetContext) -> Result<()> {
        let ctx = self.ctx;
        for line in span_table(cx.span) {
            ctx.info(line);
        }
        let staging = self.staging(cx.params)?;
        let set_dir = staging.dir.join("set");
        std::fs::create_dir(&set_dir).with_context(|| format!("create {}", set_dir.display()))?;

        // ---- Phase A: nothing burns until every disc's image exists
        *stage = Stage::Split;
        ctx.start(Stage::Split);
        let source = &cx.payloads[0];
        let parts: Vec<span::PartPlan> = cx
            .span
            .discs
            .iter()
            .filter_map(|d| d.part.clone())
            .collect();
        let set = {
            let mut th = Throttle::default();
            span::split(&source.root, &parts, &set_dir, &mut |done, total| {
                emit_pct(ctx, Stage::Split, &mut th, done, total, "splitting source");
            })?
        };
        self.done(
            Stage::Split,
            format!(
                "{} parts of {} ({} per disc)",
                parts.len(),
                human_bytes(cx.span.source_bytes),
                human_bytes(parts[0].bytes)
            ),
        );

        *stage = Stage::Parity;
        let (index_file, volume_files) = if cx.span.recovery_blocks > 0 {
            ctx.start(Stage::Parity);
            let produced = parity::create_set(
                &ctx.tools,
                cx.span,
                &set_dir,
                &staging.dir.join("parity"),
                ctx.cfg.stall_timeout(),
                &mut |pct, line| {
                    let detail = if line.is_empty() {
                        "par2 set".to_string()
                    } else {
                        line
                    };
                    ctx.progress(Stage::Parity, pct, detail);
                },
            )?;
            let index = staging
                .dir
                .join("parity")
                .join(format!("{}.par2", cx.span.source_name));
            ensure!(produced.contains(&index), "par2 produced no index file");
            let volumes: Vec<PathBuf> = produced.into_iter().filter(|p| *p != index).collect();
            ensure!(!volumes.is_empty(), "par2 produced no recovery volumes");
            self.done(
                Stage::Parity,
                format!(
                    "{} recovery blocks in {} volume file(s) - rebuilds any ONE lost disc",
                    cx.span.recovery_blocks,
                    volumes.len()
                ),
            );
            (Some(index), volumes)
        } else {
            ctx.warn("parity disabled - losing any ONE disc loses the payload".into());
            (None, Vec::new())
        };

        *stage = Stage::Checksums;
        ctx.start(Stage::Checksums);
        // hours of parity ran since Split: re-read every part against its
        // split-time hash before a single disc is committed
        {
            let total: u64 = parts.iter().map(|p| p.bytes).sum();
            let mut base = 0u64;
            let mut th = Throttle::default();
            for ((sha, name), part) in set.part_shas.iter().zip(&parts) {
                let path = set_dir.join(name);
                let now = hashing::sha256_file(&path, &mut |done, _| {
                    emit_pct(ctx, Stage::Checksums, &mut th, base + done, total, name);
                })?;
                ensure!(
                    now == *sha,
                    "staging corruption: {} no longer matches its split-time hash - re-run",
                    path.display()
                );
                base += part.bytes;
            }
        }
        let mut parity_shas: Vec<(String, String)> = Vec::new();
        if let Some(index) = &index_file {
            for f in std::iter::once(index).chain(volume_files.iter()) {
                let sha = hashing::sha256_file(f, &mut |_, _| {})?;
                parity_shas.push((sha, format!("parity/{}", master::file_name_of(f))));
            }
        }
        let set_txt = staging.dir.join("SET.txt");
        span::write_set_txt(
            &set_txt,
            cx.span,
            &set,
            &parity_shas,
            cx.media.kind.label(),
            chrono::Utc::now(),
        )?;
        let set_txt_sha = hashing::sha256_file(&set_txt, &mut |_, _| {})?;
        self.done(
            Stage::Checksums,
            format!(
                "{} parts cross-checked; SET.txt catalogs the {}-disc set",
                parts.len(),
                cx.span.discs.len()
            ),
        );

        *stage = Stage::Master;
        ctx.start(Stage::Master);
        let total_discs = cx.span.discs.len() as u32;
        let ecc_on = ctx.cfg.ecc && ctx.tools.dvdisaster.is_some();
        let mut discs: Vec<SetDisc> = Vec::with_capacity(cx.span.discs.len());
        for (i, disc) in cx.span.discs.iter().enumerate() {
            // the remaining images are the big late allocations
            let remaining = (cx.span.discs.len() - i) as u64;
            ensure_staging_free(
                &cx.params.staging,
                cx.span.per_disc_iso_est * remaining,
                &format!("{remaining} remaining set image(s)"),
            )?;
            let disc_dir = staging.dir.join(format!("disc{:02}", disc.index));
            std::fs::create_dir(&disc_dir)
                .with_context(|| format!("create {}", disc_dir.display()))?;
            let note = span::SpanNote {
                disc: disc.index,
                of: total_discs,
                role: disc.role,
                source_name: cx.span.source_name.clone(),
                source_bytes: cx.span.source_bytes,
                source_sha256: set.whole_sha.clone(),
                source_container: source.looks_like_container(),
                recovery_blocks: cx.span.recovery_blocks,
                block: cx.span.block,
            };

            let mut entries: Vec<(String, String)> = Vec::new();
            let mut manifest_rows: Vec<master::ManifestEntry> = Vec::new();
            let mut disc_payloads: Vec<Payload> = Vec::new();
            let mut disc_parity: Vec<PathBuf> = Vec::new();
            if let Some(part) = &disc.part {
                let (sha, _) = set
                    .part_shas
                    .iter()
                    .find(|(_, n)| *n == part.file_name)
                    .expect("split produced every planned part")
                    .clone();
                entries.push((sha.clone(), part.file_name.clone()));
                manifest_rows.push(master::ManifestEntry {
                    name: part.file_name.clone(),
                    bytes: part.bytes,
                    files: 1,
                    is_dir: false,
                    sha256: Some(sha),
                    par2: None,
                });
                disc_payloads.push(part_payload(&set_dir, part));
                if let Some(index) = &index_file {
                    disc_parity.push(index.clone());
                }
            } else if let Some(index) = &index_file {
                disc_parity.push(index.clone());
                disc_parity.extend(volume_files.iter().cloned());
            }
            for f in &disc_parity {
                let rel = format!("parity/{}", master::file_name_of(f));
                if let Some((sha, _)) = parity_shas.iter().find(|(_, r)| *r == rel) {
                    entries.push((sha.clone(), rel));
                }
            }
            entries.push((set_txt_sha.clone(), "SET.txt".to_string()));

            let checksums_path = disc_dir.join("checksums.sha256");
            hashing::write_checksums(&entries, &checksums_path)?;
            let manifest_path = disc_dir.join("MANIFEST.txt");
            let recovery_path = disc_dir.join("RECOVERY.txt");
            master::write_manifest(
                &manifest_path,
                &disc.label,
                &manifest_rows,
                None,
                cx.params.defect_management,
                ecc_on,
                Some(&note),
            )?;
            master::write_recovery(
                &recovery_path,
                &disc.label,
                &disc_payloads,
                ecc_on,
                Some(&note),
            )?;

            let iso = staging.dir.join(format!("{}.iso", disc.label));
            let input = master::MasterInput {
                label: &disc.label,
                payloads: &disc_payloads,
                parity_files: &disc_parity,
                checksums: &checksums_path,
                manifest: &manifest_path,
                recovery: &recovery_path,
                set_txt: Some(&set_txt),
                out_iso: &iso,
            };
            let base_pct = i as f32 * 100.0;
            let n = cx.span.discs.len() as f32;
            let disc_no = disc.index;
            let mut iso_bytes = master::build_iso(
                &ctx.tools,
                &input,
                ctx.cfg.stall_timeout(),
                &mut |pct, line| {
                    let overall = pct.map(|v| (base_pct + v) / n);
                    ctx.progress(Stage::Master, overall, format!("disc {disc_no}: {line}"));
                },
            )?;
            master::check_iso_truncation(&iso)?;
            iso_bytes = self.ecc_augment(&iso, iso_bytes, cx.plan.budget, &cx.params.staging)?;
            let lba_path = staging.dir.join(format!("{}.lba.txt", disc.label));
            master::report_lba(&ctx.tools, &iso, &lba_path)?;
            let iso_sha = hashing::sha256_file(&iso, &mut |_, _| {})?;
            let iso_sha_path = staging.dir.join(format!("{}.iso.sha256", disc.label));
            hashing::write_checksums(
                &[(iso_sha.clone(), format!("{}.iso", disc.label))],
                &iso_sha_path,
            )?;
            discs.push(SetDisc {
                label: disc.label.clone(),
                parity: disc.part.is_none(),
                mastered: Mastered {
                    iso,
                    iso_bytes,
                    iso_sha,
                    manifest_path,
                    recovery_path,
                    lba_path,
                    iso_sha_path,
                },
                entries,
                checksums_path,
            });
        }
        self.done(
            Stage::Master,
            format!("{} images mastered and self-checked", discs.len()),
        );

        // ---- Phase B: burn loop; each disc is now exactly a burn-iso
        let total = discs.len() as u32;
        for (i, d) in discs.iter().enumerate() {
            let k = (i + 1) as u32;
            ctx.send(StageEvent::DiscStart {
                index: k,
                total,
                label: d.label.clone(),
                parity: d.parity,
            });
            if i > 0 {
                self.await_blank(
                    cx,
                    &discs[i - 1].label,
                    &d.label,
                    k,
                    total,
                    d.mastered.iso_bytes,
                )?;
            }
            loop {
                let mark = self.stages.len();
                let res = (|| -> Result<()> {
                    if cx.params.defect_management {
                        *stage = Stage::Format;
                        self.format(cx.device, d.mastered.iso_bytes)?;
                    }
                    *stage = Stage::Burn;
                    self.burn(cx.device, cx.params.speed, &d.mastered)?;
                    *stage = Stage::VerifyImage;
                    self.ctx.start(Stage::VerifyImage);
                    verify_image_stage(
                        self.ctx,
                        &mut self.stages,
                        cx.device,
                        d.mastered.iso_bytes,
                        &d.mastered.iso_sha,
                        true,
                        &mut self.degradations,
                    )?;
                    *stage = Stage::VerifyFiles;
                    verify_files_stage(
                        self.ctx,
                        &mut self.stages,
                        cx.device,
                        EntriesSource::InMemory(&d.entries),
                    )?;
                    Ok(())
                })();
                match res {
                    Ok(()) => {
                        for (_, s) in &mut self.stages[mark..] {
                            *s = format!("disc {k}/{total}: {s}");
                        }
                        break;
                    }
                    Err(e) => {
                        // partial stage rows from the failed attempt would
                        // read as successes in the report
                        self.stages.truncate(mark);
                        ctx.warn(format!("disc {k}/{total} FAILED: {e:#}"));
                        match ctx.ask_raw(&format!(
                            "that disc is likely ruined - mark it BAD, do NOT label it {}. \
                             insert a fresh blank and confirm to retry disc {k}/{total}, \
                             or abort (completed discs stay valid)",
                            d.label
                        ))? {
                            Ack::Proceed => {
                                wait_ready(ctx, cx.device)?;
                                continue;
                            }
                            _ => {
                                let remaining: Vec<String> = discs[i..]
                                    .iter()
                                    .map(|r| {
                                        format!("  ovenmitts burn-iso {}", r.mastered.iso.display())
                                    })
                                    .collect();
                                bail!(
                                    "set aborted at disc {k}/{total}; earlier discs stay \
                                     valid - finish the set with:\n{}",
                                    remaining.join("\n")
                                );
                            }
                        }
                    }
                }
            }
            if (i + 1) < discs.len() {
                match verify::eject(&ctx.tools, cx.device) {
                    Ok(()) => ctx.info(format!(
                        "ejected {} - label this disc {} now",
                        cx.device, d.label
                    )),
                    Err(e) => ctx.warn(format!("could not eject {}: {e:#}", cx.device)),
                }
            } else {
                eject_if_configured(ctx, cx.device, cx.req.amend);
            }
            if cx.req.discard_iso {
                match std::fs::remove_file(&d.mastered.iso) {
                    Ok(()) => ctx.info(format!(
                        "discarded {} after successful verification",
                        d.mastered.iso.display()
                    )),
                    Err(e) => ctx.warn(format!(
                        "could not discard {}: {e:#} - remove it manually",
                        d.mastered.iso.display()
                    )),
                }
            }
        }

        // the parts live on verified discs now; the source file is untouched
        // and SET.txt records every offset needed to re-cut a part
        match std::fs::remove_dir_all(&set_dir) {
            Ok(()) => ctx.info(format!(
                "removed {} - parts are on the discs; SET.txt records their offsets",
                set_dir.display()
            )),
            Err(e) => ctx.warn(format!("could not remove {}: {e:#}", set_dir.display())),
        }
        let keep_iso = !cx.req.discard_iso;
        let mut reminders = Vec::new();
        if keep_iso {
            reminders.push(format!(
                "second copy of the set: insert fresh discs and burn each image again:{}",
                discs
                    .iter()
                    .map(|d| format!("\n  ovenmitts burn-iso {}", d.mastered.iso.display()))
                    .collect::<String>()
            ));
        }
        reminders.push(format!(
            "keep {} off-disc: the recovery volumes, SET.txt, lba maps and checksums \
             are what rebuild a lost disc",
            staging.dir.display()
        ));
        if source.looks_like_container() {
            reminders.push(
                "VeraCrypt: keep an EXTERNAL volume-header backup (Tools > Backup Volume Header); \
                 create a fresh container per archive generation"
                    .into(),
            );
        }
        let mut written_files = Vec::new();
        if staging.run_log.is_file() {
            written_files.push(staging.run_log.clone());
        }
        if let Some(index) = &index_file {
            written_files.push(index.clone());
        }
        written_files.extend(volume_files.iter().cloned());
        written_files.push(set_txt.clone());
        for d in &discs {
            written_files.push(d.checksums_path.clone());
            written_files.push(d.mastered.manifest_path.clone());
            written_files.push(d.mastered.recovery_path.clone());
            if keep_iso {
                written_files.push(d.mastered.iso.clone());
            }
            written_files.push(d.mastered.lba_path.clone());
            written_files.push(d.mastered.iso_sha_path.clone());
            let burn_log = burn::burn_log_path(&d.mastered.iso);
            if burn_log.is_file() {
                written_files.push(burn_log);
            }
        }
        let report_path = staging.dir.join(format!("{}.report.txt", staging.label));
        written_files.push(report_path.clone());
        let report = RunReport {
            iso_path: None,
            iso_sha256: None,
            iso_bytes: 0,
            stages: std::mem::take(&mut self.stages),
            reminders,
            written_files,
            degradations: std::mem::take(&mut self.degradations),
        };
        send_finished(ctx, report, &report_path);
        Ok(())
    }

    /// Swap gate: block until the operator loads a blank of the right kind
    /// with room for the next image. Wrong discs warn and re-prompt.
    fn await_blank(
        &self,
        cx: &SetContext,
        prev_label: &str,
        label: &str,
        k: u32,
        total: u32,
        iso_bytes: u64,
    ) -> Result<()> {
        let ctx = self.ctx;
        loop {
            ctx.ask(&format!(
                "disc {}/{total} verified - LABEL IT NOW: {prev_label}. insert a blank {} \
                 for disc {k}/{total} ({label}), close the tray, then confirm",
                k - 1,
                cx.media.kind.label(),
            ))?;
            wait_ready(ctx, cx.device)?;
            match media::probe(&ctx.tools, cx.device) {
                Ok(m) if m.blank && m.kind == cx.media.kind && m.free_bytes >= iso_bytes => {
                    return Ok(())
                }
                Ok(m) => ctx.warn(format!(
                    "wrong disc: {} ({}, {} free) - need a blank {} with at least {}",
                    m.kind.label(),
                    if m.blank { "blank" } else { "written" },
                    human_bytes(m.free_bytes),
                    cx.media.kind.label(),
                    human_bytes(iso_bytes)
                )),
                Err(e) => ctx.warn(format!("cannot probe {}: {e:#}", cx.device)),
            }
        }
    }
}

/// Everything a set burn needs from the confirm phase.
struct SetContext<'a> {
    req: &'a BurnRequest,
    params: &'a BurnParams,
    plan: &'a ArchivePlan,
    span: &'a span::SpanPlan,
    device: &'a str,
    media: &'a MediaInfo,
    payloads: &'a [Payload],
}

/// One disc of a prepared set: exactly a burn-iso plus its on-disc entries.
struct SetDisc {
    label: String,
    parity: bool,
    mastered: Mastered,
    entries: Vec<(String, String)>,
    checksums_path: PathBuf,
}

fn part_payload(set_dir: &Path, part: &span::PartPlan) -> Payload {
    Payload {
        root: set_dir.join(&part.file_name),
        is_dir: false,
        files: vec![plan::PayloadMember {
            abs: set_dir.join(&part.file_name),
            rel: part.file_name.clone(),
            size: part.bytes,
            container: false,
        }],
        dirs: 0,
        total_size: part.bytes,
        name: part.file_name.clone(),
    }
}

/// One Info line per disc plus a set summary - the plan the operator says
/// yes to, in line mode and the run log alike.
fn span_table(span: &span::SpanPlan) -> Vec<String> {
    let total = span.discs.len();
    let data = span.discs.iter().filter(|d| d.part.is_some()).count();
    let mut rows = Vec::with_capacity(total + 1);
    rows.push(format!(
        "set: {total} discs = {data} data + {} parity; par2 block {} bytes, {} recovery \
         blocks; staging peak ~{}",
        total - data,
        span.block,
        span.recovery_blocks,
        human_bytes(span.staging_peak)
    ));
    for d in &span.discs {
        rows.push(match &d.part {
            Some(p) => format!(
                "  disc {}/{total}  {}  {} ({}, offset {})",
                d.index,
                d.label,
                p.file_name,
                human_bytes(p.bytes),
                p.offset
            ),
            None => format!(
                "  disc {}/{total}  {}  par2 recovery volumes",
                d.index, d.label
            ),
        });
    }
    rows
}

/// Write the report file and emit Finished. The disc is already verified at
/// this point: a failed report write warns and drops the file from the list
/// instead of failing the run — a Failed here would be misattributed to the
/// verify stages and read as a bad burn.
fn send_finished(ctx: &RunnerCtx, mut report: RunReport, report_path: &Path) {
    if let Err(e) = crate::fsutil::write_durable(report_path, report_text(&report, &ctx.tools)) {
        ctx.warn(format!("could not write {}: {e:#}", report_path.display()));
        report.written_files.retain(|p| p != report_path);
    }
    ctx.send(StageEvent::Finished { report });
}

/// Where a VerifyFiles stage gets its checksum entries: the burn pipeline
/// verifies against the in-memory list it just wrote; a standalone verify
/// parses checksums.sha256 off the (untrusted) disc.
enum EntriesSource<'a> {
    InMemory(&'a [(String, String)]),
    FromDisc,
}

/// Shared read-back guard (burn, burn-iso, verify --iso): hash the disc,
/// compare to the expected ISO hash - the mismatch wording is part of the
/// tool's contract - and record the stage. The caller emits StageStart
/// (expected-hash computation may already progress under it).
fn verify_image_stage(
    ctx: &RunnerCtx,
    stages: &mut Vec<(Stage, String)>,
    device: &str,
    iso_bytes: u64,
    expected: &str,
    reloaded: bool,
    degradations: &mut Vec<String>,
) -> Result<()> {
    let (disc_sha, o_direct) = readback_stage(ctx, device, iso_bytes, reloaded, degradations)?;
    ensure!(
        disc_sha == expected,
        "READ-BACK MISMATCH - DO NOT TRUST THIS DISC: disc sha256 {disc_sha} != ISO sha256 {expected}"
    );
    ctx.done(
        stages,
        Stage::VerifyImage,
        readback_summary(iso_bytes, o_direct),
    );
    Ok(())
}

/// Shared mount / hash-every-file / unmount stage (burn pipeline + verify).
fn verify_files_stage(
    ctx: &RunnerCtx,
    stages: &mut Vec<(Stage, String)>,
    device: &str,
    source: EntriesSource,
) -> Result<()> {
    ctx.start(Stage::VerifyFiles);
    let mountpoint = verify::mount_ro(&ctx.tools, device)?;
    let verified = {
        let res = (|| {
            let parsed;
            let entries: &[(String, String)] = match source {
                EntriesSource::InMemory(entries) => entries,
                EntriesSource::FromDisc => {
                    let path = mountpoint.join("checksums.sha256");
                    let text = std::fs::read_to_string(&path)
                        .with_context(|| format!("reading {} from the disc", path.display()))?;
                    parsed = hashing::parse_checksums(&text)?;
                    &parsed
                }
            };
            let mut th = Throttle::default();
            hashing::verify_checksums(&mountpoint, entries, &mut |done, total| {
                emit_pct(
                    ctx,
                    Stage::VerifyFiles,
                    &mut th,
                    done,
                    total,
                    "hashing files on disc",
                );
            })
        })();
        if let Err(e) = verify::unmount(&ctx.tools, device) {
            ctx.warn(format!("could not unmount {device}: {e:#}"));
        }
        res?
    };
    ensure_all_match(&verified)?;
    ctx.done(
        stages,
        Stage::VerifyFiles,
        format!("{} files on disc match checksums.sha256", verified.len()),
    );
    Ok(())
}

/// Burn + verify an existing staged ISO (bit-identical second copy).
pub fn run_burn_iso(ctx: &RunnerCtx, iso: &Path, assume_yes: bool) -> Result<()> {
    with_failure(ctx, |stage| {
        let mut stages: Vec<(Stage, String)> = Vec::new();
        let mut degradations: Vec<String> = Vec::new();

        *stage = Stage::Preflight;
        ctx.start(Stage::Preflight);
        let iso_bytes = std::fs::metadata(iso)
            .with_context(|| format!("stat ISO {}", iso.display()))?
            .len();
        ensure!(iso_bytes > 0, "ISO is empty: {}", iso.display());
        // a staged ISO can rot between burns (torn copy, truncation): the
        // second-copy path gets the same self-check as a fresh master
        master::check_iso_truncation(iso)?;
        let run_log_path = iso.with_extension("run.log");
        ctx.tee_events_to(&run_log_path);
        let (device, media) = resolve_device(ctx)?;
        ctx.info(format!(
            "media: {} — {} free",
            media.kind.label(),
            human_bytes(media.free_bytes)
        ));
        ensure!(
            iso_bytes <= media.free_bytes,
            "ISO {} ({iso_bytes} bytes) exceeds free space {} ({} bytes) on the inserted medium",
            human_bytes(iso_bytes),
            human_bytes(media.free_bytes),
            media.free_bytes
        );
        let budget = media
            .free_bytes
            .saturating_sub(media.free_bytes.saturating_mul(ctx.cfg.headroom_pct as u64) / 100);
        if iso_bytes > budget {
            ctx.warn(format!(
                "ISO fills past the {}% headroom ({} of {} free) - \
                 failures concentrate at the outer region",
                ctx.cfg.headroom_pct,
                human_bytes(iso_bytes),
                human_bytes(media.free_bytes)
            ));
        }
        ctx.done(
            &mut stages,
            Stage::Preflight,
            format!("{} fits {}", human_bytes(iso_bytes), media.kind.label()),
        );

        if !assume_yes {
            ctx.ask(&format!(
                "burn {} ({}) to {device}?",
                iso.display(),
                human_bytes(iso_bytes)
            ))?;
        }

        *stage = Stage::Burn;
        ctx.start(Stage::Burn);
        burn::burn_iso(
            &ctx.tools,
            &device,
            iso,
            ctx.cfg.speed,
            ctx.cfg.stall_timeout(),
            &mut |pct, line| {
                ctx.progress(Stage::Burn, pct, line);
            },
        )
        .inspect_err(|_| {
            ctx.info(format!(
                "burn transcript: {}",
                burn::burn_log_path(iso).display()
            ));
        })?;
        ctx.done(
            &mut stages,
            Stage::Burn,
            format!("{} written, disc ejected", human_bytes(iso_bytes)),
        );

        *stage = Stage::VerifyImage;
        ctx.start(Stage::VerifyImage);
        let expected = expected_iso_sha(ctx, Stage::VerifyImage, iso)?;
        // Burn carried -eject; readback waits for the reloaded medium.
        verify_image_stage(
            ctx,
            &mut stages,
            &device,
            iso_bytes,
            &expected,
            true,
            &mut degradations,
        )?;

        // burn-iso is always line mode; only an explicit config opt-in ejects
        eject_if_configured(ctx, &device, false);

        let mut written_files = Vec::new();
        if run_log_path.is_file() {
            written_files.push(run_log_path);
        }
        let burn_log = burn::burn_log_path(iso);
        if burn_log.is_file() {
            written_files.push(burn_log);
        }
        let report_path = iso.with_extension("report.txt");
        written_files.push(report_path.clone());
        let report = RunReport {
            iso_path: Some(iso.to_path_buf()),
            iso_sha256: Some(expected),
            iso_bytes,
            stages,
            reminders: Vec::new(),
            written_files,
            degradations,
        };
        send_finished(ctx, report, &report_path);
        Ok(())
    })
}

/// Verify an already-burned disc.
pub fn run_verify(ctx: &RunnerCtx, iso: Option<&Path>) -> Result<()> {
    with_failure(ctx, |stage| {
        let mut stages: Vec<(Stage, String)> = Vec::new();
        let mut degradations: Vec<String> = Vec::new();
        let mut report_sha = None;
        let mut report_bytes = 0u64;

        // probing needs exclusive access: unmount the configured drive first
        // or an automounted disc would misroute resolution into the scan
        verify::ensure_unmounted(&ctx.tools, &ctx.cfg.device)?;
        let (device, _media) = resolve_device(ctx)?;
        verify::ensure_unmounted(&ctx.tools, &device)?;

        if let Some(iso) = iso {
            *stage = Stage::VerifyImage;
            ctx.start(Stage::VerifyImage);
            let iso_bytes = std::fs::metadata(iso)
                .with_context(|| format!("stat ISO {}", iso.display()))?
                .len();
            let expected = expected_iso_sha(ctx, Stage::VerifyImage, iso)?;
            let reloaded = match verify::eject(&ctx.tools, &device) {
                Ok(()) => {
                    ctx.info("ejected disc - reload defeats the page cache".into());
                    true
                }
                Err(e) => {
                    ctx.warn(format!(
                        "no eject/reload before read-back ({e:#}); relying on O_DIRECT"
                    ));
                    false
                }
            };
            verify_image_stage(
                ctx,
                &mut stages,
                &device,
                iso_bytes,
                &expected,
                reloaded,
                &mut degradations,
            )?;
            report_sha = Some(expected);
            report_bytes = iso_bytes;
        } else {
            // Without --iso this checks the disc against its OWN on-disc
            // checksums (plus the MD5 session tags): it detects media decay,
            // not whether the burn matched the source. Say so, and record it.
            let caveat = "no --iso given: checked the disc against its own recorded checksums, \
                 which detects media decay but not an incorrect burn - pass --iso for \
                 byte-exact source verification"
                .to_string();
            ctx.warn(caveat.clone());
            degradations.push(caveat);
        }

        *stage = Stage::VerifyFiles;
        verify_files_stage(ctx, &mut stages, &device, EntriesSource::FromDisc)?;

        *stage = Stage::CheckMedia;
        ctx.start(Stage::CheckMedia);
        match verify::check_media(
            &ctx.tools,
            &device,
            ctx.cfg.stall_timeout(),
            &mut |pct, line| {
                ctx.progress(Stage::CheckMedia, pct, line);
            },
        ) {
            Ok(true) => ctx.done(
                &mut stages,
                Stage::CheckMedia,
                "MD5 tags and read check clean".into(),
            ),
            Ok(false) => bail!("xorriso -check_media reports damage or MD5 mismatch"),
            // A verify that exits 0 after silently skipping its media check is
            // false confidence. check_media needs only xorriso (always present),
            // so an error here is exceptional - fail rather than report success.
            Err(e) => return Err(e).context("check_media could not run"),
        }

        ctx.send(StageEvent::Finished {
            report: RunReport {
                iso_path: iso.map(Path::to_path_buf),
                iso_sha256: report_sha,
                iso_bytes: report_bytes,
                stages,
                reminders: Vec::new(),
                written_files: Vec::new(),
                degradations,
            },
        });
        Ok(())
    })
}

/// Source-free periodic health check (xorriso -check_media using MD5 tags).
pub fn run_check(ctx: &RunnerCtx, save_to: Option<&Path>) -> Result<()> {
    with_failure(ctx, |stage| {
        let mut stages: Vec<(Stage, String)> = Vec::new();
        *stage = Stage::CheckMedia;
        ctx.start(Stage::CheckMedia);
        // probing needs exclusive access: unmount the configured drive first
        // or an automounted disc would misroute resolution into the scan
        verify::ensure_unmounted(&ctx.tools, &ctx.cfg.device)?;
        let (device, media) = resolve_device(ctx)?;
        save_device_if(ctx, save_to, &device)?;
        ensure!(
            !media.blank,
            "medium in {device} is blank - nothing to check yet (check verifies a burned disc)"
        );
        verify::ensure_unmounted(&ctx.tools, &device)?;
        let clean = verify::check_media(
            &ctx.tools,
            &device,
            ctx.cfg.stall_timeout(),
            &mut |pct, line| {
                ctx.progress(Stage::CheckMedia, pct, line);
            },
        )?;
        ensure!(
            clean,
            "medium DAMAGED or MD5 mismatch - recover now: copy what reads, then par2 repair (see RECOVERY.txt on the disc)"
        );
        ctx.done(&mut stages, Stage::CheckMedia, "medium checks clean".into());
        ctx.send(StageEvent::Finished {
            report: RunReport {
                stages,
                ..RunReport::default()
            },
        });
        Ok(())
    })
}

/// Print media info to events.
pub fn run_info(ctx: &RunnerCtx, save_to: Option<&Path>) -> Result<()> {
    with_failure(ctx, |_stage| {
        let (device, media) = resolve_device(ctx)?;
        save_device_if(ctx, save_to, &device)?;
        ctx.out(format!("device : {device}"));
        ctx.out(format!("profile: {}", media.profile));
        ctx.out(format!("type   : {}", media.kind.label()));
        ctx.out(format!(
            "status : {}{}",
            if media.blank { "blank" } else { "written" },
            if media.formatted { ", formatted" } else { "" }
        ));
        ctx.out(format!(
            "free   : {} ({} bytes)",
            human_bytes(media.free_bytes),
            media.free_bytes
        ));
        match media.formatted_capacity {
            Some(cap) => ctx.out(format!(
                "capacity after defect-management format: {} ({cap} bytes)",
                human_bytes(cap)
            )),
            None => ctx.out("capacity after defect-management format: unknown".into()),
        }
        if media.speeds.is_empty() {
            ctx.out("write speeds: none reported".into());
        } else {
            let speeds: Vec<String> = media.speeds.iter().map(|s| format!("{s}x")).collect();
            ctx.out(format!("write speeds: {}", speeds.join(", ")));
        }
        if let Some(id) = &media.media_id {
            ctx.out(format!("media id: {id}"));
        }
        ctx.send(StageEvent::Finished {
            report: RunReport::default(),
        });
        Ok(())
    })
}

/// Capacity math without burning: probe if possible, otherwise synthetic media.
/// A `--media` hint always forces synthetic media.
pub fn run_plan(
    ctx: &RunnerCtx,
    payload_paths: &[PathBuf],
    media_hint: Option<&str>,
) -> Result<()> {
    with_failure(ctx, |_stage| {
        let (payloads, warnings) = inspect_payloads(payload_paths)?;
        for w in warnings {
            ctx.warn(w);
        }
        let (device, media) = match media_hint {
            Some(hint) => (ctx.cfg.device.clone(), media::synthetic(hint)?),
            None => match resolve_device(ctx) {
                Ok(found) => found,
                // ambiguity means real discs were probed - refusing beats
                // guessing synthetic capacity that matches neither
                Err(e) if e.downcast_ref::<AmbiguousDrives>().is_some() => return Err(e),
                Err(e) => {
                    ctx.info(format!(
                        "no disc probed ({e:#}); assuming a blank BD-R 25 (use --media to override)"
                    ));
                    (ctx.cfg.device.clone(), media::synthetic("bd25")?)
                }
            },
        };
        let params = BurnParams {
            label: default_label(),
            speed: ctx.cfg.speed,
            redundancy_pct: ctx.cfg.redundancy_pct,
            parity: true,
            defect_management: ctx.cfg.defect_management,
            staging: ctx.cfg.staging.clone(),
        };
        let mut plan =
            plan::build_plan(&params.plan_input(&payloads, ctx.cfg.headroom_pct), &media);
        if !plan.fits {
            let ecc = ctx.cfg.ecc && ctx.tools.dvdisaster.is_some();
            plan.span = span::plan_span(
                &payloads,
                &params.label,
                plan.budget,
                plan.overhead_bytes_est,
                params.parity,
                ecc,
            )?
            .map(Box::new);
        }
        for w in &plan.warnings {
            ctx.warn(w.clone());
        }
        if let Some(span) = &plan.span {
            for line in span_table(span) {
                ctx.info(line);
            }
        }
        ctx.send(StageEvent::Plan {
            device,
            media,
            plan: plan.clone(),
            params,
        });
        ensure_fits(&plan, ctx.cfg.headroom_pct)?;
        ctx.send(StageEvent::Finished {
            report: RunReport::default(),
        });
        Ok(())
    })
}

/// Distinct error type so read-only paths (plan) can propagate ambiguity
/// instead of masking it with a synthetic-media fallback.
#[derive(Debug)]
struct AmbiguousDrives(Vec<String>);

impl std::fmt::Display for AmbiguousDrives {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "multiple drives have media: {} - pass --device or set device in the config",
            self.0.join(", ")
        )
    }
}

impl std::error::Error for AmbiguousDrives {}

/// Pick the drive to operate on. A --device from the CLI is used as-is.
/// The built-in default or a config-file device is a soft preference:
/// probed first; if that fails, scan the host's drives — exactly one with
/// media wins, ambiguity refuses.
fn resolve_device(ctx: &RunnerCtx) -> Result<(String, MediaInfo)> {
    resolve_device_from(ctx, media::list_drives)
}

/// --save support: persist the resolved device, loudly. A failed write is a
/// stage failure — silently continuing would fake the persistence.
fn save_device_if(ctx: &RunnerCtx, save_to: Option<&Path>, device: &str) -> Result<()> {
    if let Some(path) = save_to {
        crate::config::save_device(path, device)?;
        ctx.info(format!("saved device = {device} to {}", path.display()));
    }
    Ok(())
}

fn resolve_device_from(
    ctx: &RunnerCtx,
    candidates: impl FnOnce() -> Vec<String>,
) -> Result<(String, MediaInfo)> {
    let (device, media, note) = detect_media_from(&ctx.cfg, &ctx.tools, candidates)?;
    if let Some(note) = note {
        ctx.info(note);
    }
    Ok((device, media))
}

/// Ctx-free device resolution (same policy as resolve_device) for advisory
/// consumers like the payload picker. The note reports an auto-select swap.
pub fn detect_media(cfg: &Config, tools: &Tools) -> Result<(String, MediaInfo, Option<String>)> {
    detect_media_from(cfg, tools, media::list_drives)
}

fn detect_media_from(
    cfg: &Config,
    tools: &Tools,
    candidates: impl FnOnce() -> Vec<String>,
) -> Result<(String, MediaInfo, Option<String>)> {
    let configured = cfg.device.clone();
    let configured_err = match media::probe(tools, &configured) {
        Ok(media) => return Ok((configured, media, None)),
        // xorriso itself failed to launch: scanning with the same binary
        // would fail identically and mask the real error
        Err(e)
            if e.chain()
                .any(|c| c.downcast_ref::<std::io::Error>().is_some()) =>
        {
            return Err(e)
        }
        Err(e) => e,
    };
    if cfg.device_explicit {
        return Err(configured_err);
    }
    let mut loaded: Vec<(String, MediaInfo)> = Vec::new();
    for dev in candidates() {
        if dev == configured {
            continue;
        }
        if let Ok(media) = media::probe(tools, &dev) {
            loaded.push((dev, media));
        }
    }
    match loaded.len() {
        0 => Err(configured_err),
        1 => {
            let (device, media) = loaded.remove(0);
            let note = format!(
                "no medium in {configured}; auto-selected {device} ({})",
                media.kind.label()
            );
            Ok((device, media, Some(note)))
        }
        _ => Err(anyhow::Error::new(AmbiguousDrives(
            loaded
                .iter()
                .map(|(d, m)| format!("{d} ({})", m.kind.label()))
                .collect(),
        ))),
    }
}

/// Preflight helper shared by run_burn: payload inspection, mounted-container
/// refusal (veracrypt --text --list), drive selection + media probe.
pub fn preflight_probe(
    ctx: &RunnerCtx,
    payload_paths: &[PathBuf],
) -> Result<(Vec<Payload>, String, MediaInfo)> {
    let (payloads, warnings) = inspect_payloads(payload_paths)?;
    for w in warnings {
        ctx.warn(w);
    }
    for p in &payloads {
        let container = if p.looks_like_container() {
            " (VeraCrypt container?)"
        } else {
            ""
        };
        ctx.info(if p.is_dir {
            format!(
                "payload {}/: {} ({} files){container}",
                p.name,
                human_bytes(p.total_size),
                p.files.len()
            )
        } else {
            format!(
                "payload {}: {}{container}",
                p.name,
                human_bytes(p.total_size)
            )
        });
    }

    if let Some(veracrypt) = &ctx.tools.veracrypt {
        let listing = veracrypt_list(veracrypt)?;
        for m in payloads.iter().flat_map(|p| p.files.iter()) {
            ensure!(
                !listing_mentions(&listing, &m.abs),
                "{} is a MOUNTED VeraCrypt container - dismount it first (veracrypt -d)",
                m.abs.display()
            );
        }
    }

    let (device, media) = resolve_device(ctx)?;
    Ok((payloads, device, media))
}

/// Best-effort completion eject; a failed eject never fails a verified
/// archive. `default` decides when eject_when_done is unset.
fn eject_if_configured(ctx: &RunnerCtx, device: &str, default: bool) {
    if ctx.cfg.eject_when_done.unwrap_or(default) {
        match verify::eject(&ctx.tools, device) {
            Ok(()) => ctx.info(format!("ejected {device} - label the disc before storing")),
            Err(e) => ctx.warn(format!("could not eject {device}: {e:#}")),
        }
    }
}

/// Everything that must hold before a burn is committed to; runs before the
/// prompt in non-amend paths and again on Proceed.
fn confirm_gate(ctx: &RunnerCtx, plan: &ArchivePlan, staging: &Path) -> Result<()> {
    ensure_fits(plan, ctx.cfg.headroom_pct)?;
    check_staging_space(plan, staging)
}

fn ensure_fits(plan: &ArchivePlan, headroom_pct: u32) -> Result<()> {
    if plan.span.is_some() {
        return Ok(());
    }
    ensure!(
        plan.fits,
        "does not fit: total {} ({} bytes) exceeds budget {} ({} bytes; {}% headroom off {} \
         capacity) - multi-disc sets take a single file payload; tar a directory first or \
         burn payloads separately",
        human_bytes(plan.total_bytes_est),
        plan.total_bytes_est,
        human_bytes(plan.budget),
        plan.budget,
        headroom_pct,
        human_bytes(plan.capacity)
    );
    Ok(())
}

/// Staging is never auto-cleaned (an archival tool never deletes staged
/// data), so earlier run dirs accumulate until burns fail on space. Flag
/// siblings that are >30 days old, or whose combined size exceeds what this
/// plan still needs, and leave the deleting to the operator.
fn stale_staging_note(staging: &Path, needed: u64) -> Option<String> {
    const STALE_DAYS: u64 = 30;
    let entries = std::fs::read_dir(staging).ok()?;
    let mut dirs: Vec<(String, u64, u64)> = Vec::new(); // (name, age days, bytes)
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let age_days = std::fs::metadata(&p)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs() / 86_400)
            .unwrap_or(0);
        dirs.push((master::file_name_of(&p), age_days, dir_size(&p)));
    }
    let total: u64 = dirs.iter().map(|(_, _, b)| *b).sum();
    let stale: Vec<&(String, u64, u64)> = dirs
        .iter()
        .filter(|(_, age, _)| *age > STALE_DAYS)
        .collect();
    if stale.is_empty() && total <= needed {
        return None;
    }
    let flagged: Vec<&(String, u64, u64)> = if stale.is_empty() {
        dirs.iter().collect()
    } else {
        stale
    };
    let bytes: u64 = flagged.iter().map(|(_, _, b)| *b).sum();
    let mut names: Vec<&str> = flagged.iter().map(|(n, _, _)| n.as_str()).collect();
    names.sort_unstable();
    let shown = names.len().min(5);
    let mut listed = names[..shown].join(", ");
    if names.len() > shown {
        listed.push_str(&format!(", … {} more", names.len() - shown));
    }
    Some(format!(
        "staging {} holds {} earlier run dir(s), {}: {listed} - remove ones you \
         have burned and verified (ovenmitts never deletes staged data)",
        staging.display(),
        flagged.len(),
        human_bytes(bytes),
    ))
}

// Never follows symlinks (DirEntry::file_type is lstat-based): a symlinked
// dir inside an old run dir must not be descended into - a cycle would spin
// this forever, on every future burn's preflight.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => {
                    total += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
                _ => {}
            }
        }
    }
    total
}

// payloads stay in place; staging holds parity + the ISO (which contains both)
fn check_staging_space(plan: &ArchivePlan, staging: &Path) -> Result<()> {
    if let Some(span) = &plan.span {
        return ensure_staging_free(
            staging,
            span.staging_peak,
            &format!(
                "multi-disc set peak: source parts {} + recovery volumes + {} images",
                human_bytes(span.source_bytes),
                span.discs.len()
            ),
        );
    }
    ensure_staging_free(
        staging,
        confirm_space_needed(plan),
        &format!(
            "parity {} + ISO {}",
            human_bytes(plan.parity_bytes_est),
            human_bytes(plan.total_bytes_est)
        ),
    )
}

/// Master-time re-check: the parity files are already on disk (sunk cost,
/// already subtracted from the free-space reading), so only the ISO — whose
/// estimate contains the parity copies — remains to allocate. Charging
/// parity again here would spuriously fail near-capacity burns after the
/// parity work is done.
fn check_staging_space_for_iso(plan: &ArchivePlan, staging: &Path) -> Result<()> {
    ensure_staging_free(
        staging,
        master_space_needed(plan),
        &format!("ISO {}", human_bytes(plan.total_bytes_est)),
    )
}

fn confirm_space_needed(plan: &ArchivePlan) -> u64 {
    plan.parity_bytes_est + plan.total_bytes_est
}

fn master_space_needed(plan: &ArchivePlan) -> u64 {
    plan.total_bytes_est
}

fn ensure_staging_free(staging: &Path, needed: u64, breakdown: &str) -> Result<()> {
    std::fs::create_dir_all(staging)
        .with_context(|| format!("create staging dir {}", staging.display()))?;
    let free = staging_free_bytes(staging)?;
    ensure!(
        free >= needed,
        "staging {} has {} free but needs ~{} ({breakdown})",
        staging.display(),
        human_bytes(free),
        human_bytes(needed),
    );
    Ok(())
}

fn inspect_payloads(paths: &[PathBuf]) -> Result<(Vec<Payload>, Vec<String>)> {
    ensure!(!paths.is_empty(), "no payload files given");
    let mut payloads = Vec::with_capacity(paths.len());
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    for p in paths {
        let (payload, mut w) = Payload::inspect(p.clone())?;
        warnings.append(&mut w);
        ensure!(
            seen.insert(payload.name.clone()),
            "duplicate payload filename '{}' - the disc root is flat, rename one copy",
            payload.name
        );
        payloads.push(payload);
    }
    Ok((payloads, warnings))
}

// The mounted-container check must fail CLOSED: burning a live container yields
// a silently corrupt archive, the exact disaster this tool exists to prevent.
// `veracrypt --text --list` exits non-zero with "No volumes mounted" when
// nothing is mounted (that is the clean, empty case); any other failure means
// we could not determine mount state and must refuse rather than assume safe.
fn veracrypt_list(bin: &Path) -> Result<String> {
    let out = crate::proc::output_deadline(
        bin,
        &["--text".into(), "--list".into()],
        crate::proc::SHORT_OP_DEADLINE,
    )
    .with_context(|| format!("running {} --text --list", bin.display()))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let combined =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    if combined.contains("No volumes mounted") {
        return Ok(String::new());
    }
    bail!(
        "could not determine whether a VeraCrypt container is mounted \
         (veracrypt --list failed: {}) - dismount any container manually, \
         or remove veracrypt from PATH to skip this check",
        combined.trim()
    )
}

fn listing_mentions(listing: &str, path: &Path) -> bool {
    if listing.is_empty() {
        return false;
    }
    if listing.contains(&path.display().to_string()) {
        return true;
    }
    path.canonicalize()
        .map(|c| listing.contains(&c.display().to_string()))
        .unwrap_or(false)
}

fn staging_free_bytes(dir: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c =
        std::ffi::CString::new(dir.as_os_str().as_bytes()).context("staging path contains NUL")?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    ensure!(
        rc == 0,
        "statvfs {}: {}",
        dir.display(),
        std::io::Error::last_os_error()
    );
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

fn default_label() -> String {
    format!("ARCHIVE_{}", chrono::Utc::now().format("%Y%m%d"))
}

fn sanitize_label(raw: &str) -> String {
    let mut label: String = raw
        .to_uppercase()
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect();
    label.truncate(32);
    if label.is_empty() {
        "ARCHIVE".into()
    } else {
        label
    }
}

/// Atomically claim a fresh run dir under staging: mkdir(0700) either
/// succeeds (ours alone - no check-then-create race, and mkdir never follows
/// a symlink) or already exists, in which case the label gets a numeric
/// suffix and the claim retries. The staging ROOT keeps create_dir_all: it
/// is the user's own configured path and a symlinked root is legitimate.
fn claim_stage_dir(staging: &Path, base: &str) -> Result<(String, PathBuf)> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(staging)
        .with_context(|| format!("create staging dir {}", staging.display()))?;
    let mut n = 1u64;
    loop {
        let label = if n == 1 {
            base.to_string()
        } else {
            let suffix = format!("_{n}");
            let keep = 32usize.saturating_sub(suffix.len()).min(base.len());
            format!("{}{suffix}", &base[..keep])
        };
        let dir = staging.join(&label);
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok((label, dir)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => {
                return Err(e).with_context(|| format!("create staging dir {}", dir.display()))
            }
        }
    }
}

/// One line per failed file, each stating whether it is a bad burn
/// (mismatch/missing) or a failing medium (read error) - the operator's next
/// move differs completely between the two.
fn ensure_all_match(verified: &[(String, hashing::FileCheck)]) -> Result<()> {
    let bad: Vec<String> = verified
        .iter()
        .filter_map(|(rel, check)| check.problem().map(|p| format!("{rel}: {p}")))
        .collect();
    ensure!(
        bad.is_empty(),
        "file verification FAILED on disc: {}",
        bad.join("; ")
    );
    Ok(())
}

fn wait_ready(ctx: &RunnerCtx, device: &str) -> Result<()> {
    verify::wait_medium_ready(&ctx.tools, device, READY_TIMEOUT, &mut |msg| ctx.warn(msg))
}

/// Read the disc back and hash it. `reloaded` states whether a physical
/// eject/reload preceded this read. The cache-proof guarantee holds iff the
/// page cache was defeated by O_DIRECT **or** by that reload; if neither
/// happened the buffered read could be served from cache and a match would be
/// meaningless, so we fail closed. A buffered read after a real reload is a
/// degradation (recorded), not a failure. Returns (sha256, used_o_direct).
fn readback_stage(
    ctx: &RunnerCtx,
    device: &str,
    iso_bytes: u64,
    reloaded: bool,
    degradations: &mut Vec<String>,
) -> Result<(String, bool)> {
    wait_ready(ctx, device)?;
    verify::ensure_unmounted(&ctx.tools, device)?;
    let mut th = Throttle::default();
    let rb = verify::readback_hash(device, iso_bytes, &mut |done, total| {
        emit_pct(
            ctx,
            Stage::VerifyImage,
            &mut th,
            done,
            total,
            "reading disc",
        );
    })?;
    if !rb.o_direct {
        ensure!(
            reloaded,
            "cannot defeat the page cache: O_DIRECT is unavailable and no \
             eject/reload happened, so a buffered read-back would compare \
             against cached data - install 'eject' or free the device, then retry"
        );
        let caveat = "image read-back used buffered reads (O_DIRECT unavailable); \
             cache defeat relied on the physical disc reload"
            .to_string();
        ctx.warn(caveat.clone());
        degradations.push(caveat);
    }
    Ok((rb.sha256, rb.o_direct))
}

/// The written report mirrors the on-screen summary plus provenance: which
/// ovenmitts and which tools made this disc - the facts a future reader needs
/// to trust or reconstruct it.
fn report_text(report: &RunReport, tools: &Tools) -> String {
    use std::fmt::Write as _;
    let mut t = String::new();
    let _ = writeln!(t, "ovenmitts {} run report", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(
        t,
        "finished: {}",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );
    let _ = writeln!(t, "tools:");
    let _ = writeln!(t, "  xorriso: {}", tools.xorriso.display());
    if let Some(par2) = &tools.par2 {
        match &tools.par2_version {
            Some(v) => {
                let _ = writeln!(t, "  par2: {} ({v})", par2.display());
            }
            None => {
                let _ = writeln!(t, "  par2: {}", par2.display());
            }
        }
    }
    for (name, path) in [
        ("udisksctl", &tools.udisksctl),
        ("veracrypt", &tools.veracrypt),
        ("eject", &tools.eject),
        ("dvd+rw-mediainfo", &tools.mediainfo),
        ("dvdisaster", &tools.dvdisaster),
    ] {
        if let Some(p) = path {
            let _ = writeln!(t, "  {name}: {}", p.display());
        }
    }
    let _ = writeln!(t);
    for (stage, summary) in &report.stages {
        let _ = writeln!(t, "{:<13} {summary}", stage.label());
    }
    if let Some(p) = &report.iso_path {
        let _ = writeln!(t, "iso: {}", p.display());
    }
    if let Some(h) = &report.iso_sha256 {
        let _ = writeln!(t, "iso sha256: {h}");
    }
    if report.iso_bytes > 0 {
        let _ = writeln!(
            t,
            "iso size: {} ({} bytes)",
            human_bytes(report.iso_bytes),
            report.iso_bytes
        );
    }
    for f in &report.written_files {
        let _ = writeln!(t, "wrote: {}", f.display());
    }
    for c in &report.degradations {
        let _ = writeln!(t, "caveat: {c}");
    }
    for r in &report.reminders {
        let _ = writeln!(t, "reminder: {r}");
    }
    t
}

/// VerifyImage stage summary noting how the page cache was defeated.
fn readback_summary(iso_bytes: u64, o_direct: bool) -> String {
    let how = if o_direct {
        "O_DIRECT"
    } else {
        "buffered after disc reload"
    };
    format!(
        "{} read back, sha256 matches ISO ({how})",
        human_bytes(iso_bytes)
    )
}

fn expected_iso_sha(ctx: &RunnerCtx, stage: Stage, iso: &Path) -> Result<String> {
    let mut sidecar = iso.as_os_str().to_os_string();
    sidecar.push(".sha256");
    let sidecar = PathBuf::from(sidecar);
    if sidecar.is_file() {
        let text = std::fs::read_to_string(&sidecar)
            .with_context(|| format!("reading {}", sidecar.display()))?;
        match hashing::parse_checksums(&text).map(|e| e.into_iter().next()) {
            Ok(Some((sha, _))) => {
                ctx.info(format!("using recorded sha256 from {}", sidecar.display()));
                return Ok(sha);
            }
            _ => ctx.warn(format!(
                "{}: unrecognized content; hashing the ISO instead",
                sidecar.display()
            )),
        }
    }
    let mut th = Throttle::default();
    hashing::sha256_file(iso, &mut |done, total| {
        emit_pct(ctx, stage, &mut th, done, total, "hashing ISO");
    })
}

// Byte-progress callbacks fire every MiB; only forward tenth-of-percent steps.
#[derive(Default)]
struct Throttle(Option<u32>);

impl Throttle {
    fn pass(&mut self, pct: f32) -> bool {
        let key = (pct * 10.0) as u32;
        if self.0 == Some(key) {
            false
        } else {
            self.0 = Some(key);
            true
        }
    }
}

fn emit_pct(ctx: &RunnerCtx, stage: Stage, th: &mut Throttle, done: u64, total: u64, what: &str) {
    let pct = if total == 0 {
        100.0
    } else {
        (done as f64 / total as f64 * 100.0) as f32
    };
    if th.pass(pct) {
        ctx.progress(
            stage,
            Some(pct),
            format!("{what} — {} / {}", human_bytes(done), human_bytes(total)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;

    const PROBE_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xorriso_probe_bdr_blank_full.txt"
    );
    const PROBE_WRITTEN_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xorriso_toc_dvdrw_closed.txt"
    );
    const CHECK_CLEAN_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/check_media_clean.txt"
    );
    const CHECK_DAMAGED_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/check_media_damaged.txt"
    );

    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn fake_xorriso(dir: &Path, device: &Path, check_fixture: &str) -> PathBuf {
        fake_xorriso_probing(dir, device, PROBE_FIXTURE, check_fixture)
    }

    fn fake_xorriso_probing(
        dir: &Path,
        device: &Path,
        probe_fixture: &str,
        check_fixture: &str,
    ) -> PathBuf {
        let path = dir.join("xorriso");
        write_script(
            &path,
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"-outdev\" ] && [ \"$3\" = \"-toc\" ]; then\n\
                   cat \"{probe}\"\n  exit 0\nfi\n\
                 if [ \"$1\" = \"-as\" ] && [ \"$2\" = \"mkisofs\" ]; then\n\
                   out=\"\"; prev=\"\"\n\
                   for a in \"$@\"; do\n\
                     if [ \"$prev\" = \"-o\" ]; then out=\"$a\"; fi\n\
                     prev=\"$a\"\n\
                   done\n\
                   {{ head -c 32768 /dev/zero; printf '\\001CD001\\001'; \
                      head -c 73 /dev/zero; printf '\\021\\000\\000\\000'; \
                      head -c 44 /dev/zero; printf '\\000\\010'; \
                      head -c 1918 /dev/zero; }} > \"$out\"\n\
                   printf 'xorriso : UPDATE :  52.22%% done\\n' >&2\n\
                   exit 0\nfi\n\
                 if [ \"$1\" = \"-outdev\" ] && [ \"$3\" = \"-format\" ]; then\n\
                   printf 'xorriso : UPDATE : Formatting  100.0%%\\n' >&2\n\
                   exit 0\nfi\n\
                 if [ \"$1\" = \"-as\" ] && [ \"$2\" = \"cdrecord\" ]; then\n\
                   printf '%s\\n' \"$@\" > \"{device}.cdrecord_argv\"\n\
                   iso=\"\"\n\
                   for a in \"$@\"; do\n\
                     case \"$a\" in\n\
                       dev=*|-*|*=*) ;;\n\
                       *) iso=\"$a\" ;;\n\
                     esac\n\
                   done\n\
                   cp \"$iso\" \"{device}\"\n\
                   printf 'xorriso : UPDATE : Writing:    4s  100.0%%   fifo   0%%\\n' >&2\n\
                   exit 0\nfi\n\
                 if [ \"$1\" = \"-indev\" ] && [ \"$3\" = \"-find\" ]; then\n\
                   printf \"File data lba:  0 ,       32 ,        4 ,     8192 , '/vault.hc'\\n\"\n\
                   exit 0\nfi\n\
                 if [ \"$1\" = \"-md5\" ] && [ \"$5\" = \"-check_media\" ]; then\n\
                   cat \"{check}\"\n  exit 0\nfi\n\
                 exit 1\n",
                probe = probe_fixture,
                device = device.display(),
                check = check_fixture,
            ),
        );
        path
    }

    fn fake_par2(dir: &Path) -> PathBuf {
        let path = dir.join("par2");
        write_script(
            &path,
            "#!/bin/sh\n\
             out=$7\n\
             printf '%s\\n' \"$@\" > \"$out.argv\"\n\
             printf 'Processing: 50.0%%\\r'\n\
             printf 'Done\\n'\n\
             printf 'recovery' > \"$out\"\n\
             printf 'volume' > \"${out%.par2}.vol000+01.par2\"\n",
        );
        path
    }

    fn fake_udisksctl(dir: &Path, mnt: &Path, payload: &Path, stage_dir: &Path) -> PathBuf {
        let path = dir.join("udisksctl");
        write_script(
            &path,
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"mount\" ]; then\n\
                   mkdir -p \"{mnt}/parity\"\n\
                   cp \"{payload}\" \"{mnt}/\"\n\
                   cp \"{stage}\"/parity/*.par2 \"{mnt}/parity/\" 2>/dev/null || true\n\
                   cp \"{stage}/checksums.sha256\" \"{mnt}/\" 2>/dev/null || true\n\
                   echo \"Mounted /dev/x at {mnt}.\"\n\
                 fi\n\
                 exit 0\n",
                mnt = mnt.display(),
                payload = payload.display(),
                stage = stage_dir.display(),
            ),
        );
        path
    }

    fn tools_with(
        xorriso: PathBuf,
        par2: Option<PathBuf>,
        udisksctl: Option<PathBuf>,
        veracrypt: Option<PathBuf>,
    ) -> Tools {
        let mut t = Tools::bare(xorriso);
        t.par2 = par2;
        t.udisksctl = udisksctl;
        t.veracrypt = veracrypt;
        t
    }

    fn cfg_with(device: &Path, staging: &Path) -> Config {
        Config {
            device: device.display().to_string(),
            device_explicit: true,
            staging: staging.to_path_buf(),
            speed: None,
            redundancy_pct: 15,
            headroom_pct: 5,
            defect_management: false,
            keep_iso: true,
            eject_when_done: None,
            stall_timeout_secs: 0,
            ecc: true,
        }
    }

    fn fake_eject(dir: &Path) -> PathBuf {
        let path = dir.join("eject");
        write_script(&path, "#!/bin/sh\ntouch \"$1.ejected\"\n");
        path
    }

    // Full pipeline with fakes; returns the device path (its ".ejected"
    // marker records whether the completion eject ran) and the events.
    fn burn_with_eject_cfg(
        eject_when_done: Option<bool>,
        interactive: bool,
    ) -> (PathBuf, Vec<StageEvent>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, vec![7u8; 128 * 1024]).unwrap();
        let device = dir.path().join("device");
        let staging = dir.path().join("staging");
        let stage_dir = staging.join("T1");
        let mnt = dir.path().join("mnt");
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let par2 = fake_par2(dir.path());
        let udisksctl = fake_udisksctl(dir.path(), &mnt, &payload, &stage_dir);
        let mut tools = tools_with(xorriso, Some(par2), Some(udisksctl), None);
        tools.eject = Some(fake_eject(dir.path()));
        let mut cfg = cfg_with(&device, &staging);
        cfg.eject_when_done = eject_when_done;
        let (ctx, rx, ack_tx) = ctx_pair(cfg, tools);
        if interactive {
            ack_tx.send(Ack::Proceed).unwrap();
        }
        let req = BurnRequest {
            payloads: vec![payload.clone()],
            label: Some("T1".into()),
            parity: true,
            dry_run: false,
            assume_yes: !interactive,
            amend: interactive,
            discard_iso: false,
        };
        run_burn(&ctx, &req).unwrap_or_else(|e| panic!("{e:#}"));
        (device, rx.try_iter().collect(), dir)
    }

    fn ejected_marker(device: &Path) -> PathBuf {
        PathBuf::from(format!("{}.ejected", device.display()))
    }

    #[test]
    fn tui_burn_ejects_disc_after_verified_archive() {
        let (device, events, _dir) = burn_with_eject_cfg(None, true);
        assert!(
            ejected_marker(&device).exists(),
            "unset eject_when_done must eject when an operator is present"
        );
        assert!(events
            .iter()
            .any(|ev| matches!(ev, StageEvent::Info(t) if t.contains("ejected"))));
    }

    #[test]
    fn line_mode_burn_leaves_disc_loaded() {
        let (device, _events, _dir) = burn_with_eject_cfg(None, false);
        assert!(
            !ejected_marker(&device).exists(),
            "unattended runs must not leave the tray open"
        );
    }

    #[test]
    fn eject_when_done_true_ejects_in_line_mode() {
        let (device, _events, _dir) = burn_with_eject_cfg(Some(true), false);
        assert!(ejected_marker(&device).exists());
    }

    #[test]
    fn eject_when_done_false_suppresses_tui_eject() {
        let (device, _events, _dir) = burn_with_eject_cfg(Some(false), true);
        assert!(!ejected_marker(&device).exists());
    }

    fn ctx_pair(cfg: Config, tools: Tools) -> (RunnerCtx, Receiver<StageEvent>, Sender<Ack>) {
        let (tx, rx) = mpsc::channel();
        let (ack_tx, ack_rx) = mpsc::channel();
        (RunnerCtx::new(cfg, tools, tx, ack_rx), rx, ack_tx)
    }

    fn set_mtime_days_ago(p: &Path, days: u64) {
        use std::os::unix::ffi::OsStrExt;
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
        let secs = t
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t;
        let tv = [
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
        ];
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::utimes(c.as_ptr(), tv.as_ptr()) }, 0);
    }

    #[test]
    fn stale_staging_dirs_are_reported_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        let old_run = staging.join("OLD_ARCHIVE");
        std::fs::create_dir_all(&old_run).unwrap();
        std::fs::write(old_run.join("OLD_ARCHIVE.iso"), vec![0u8; 4096]).unwrap();
        set_mtime_days_ago(&old_run, 40);

        let note = stale_staging_note(&staging, 1 << 40).expect("40-day dir must be flagged");
        assert!(note.contains("OLD_ARCHIVE"), "{note}");
        assert!(note.contains("burned and verified"), "{note}");
        assert!(old_run.exists(), "must never delete staged data");
    }

    #[test]
    fn crowding_staging_dirs_are_reported_by_size() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        let recent = staging.join("RECENT");
        std::fs::create_dir_all(&recent).unwrap();
        std::fs::write(recent.join("RECENT.iso"), vec![0u8; 8192]).unwrap();

        // fresh dir, but bigger than what this plan still needs to allocate
        let note = stale_staging_note(&staging, 4096).expect("crowding dirs must be flagged");
        assert!(note.contains("RECENT"), "{note}");
    }

    #[test]
    fn stale_staging_note_survives_symlink_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        let old_run = staging.join("OLD");
        std::fs::create_dir_all(&old_run).unwrap();
        std::fs::write(old_run.join("f"), vec![1u8; 1024]).unwrap();
        // a symlink cycle in an old run dir must not hang every future burn
        std::os::unix::fs::symlink(".", old_run.join("loop")).unwrap();
        set_mtime_days_ago(&old_run, 40);
        let note = stale_staging_note(&staging, u64::MAX).expect("stale dir must be flagged");
        assert!(note.contains("OLD"), "{note}");
    }

    #[test]
    fn master_recheck_charges_only_the_iso() {
        let payload = plan::Payload {
            root: "/data/vault.hc".into(),
            is_dir: false,
            files: vec![plan::PayloadMember {
                abs: "/data/vault.hc".into(),
                rel: "vault.hc".into(),
                size: 8 * 1024 * 1024,
                container: false,
            }],
            dirs: 0,
            total_size: 8 * 1024 * 1024,
            name: "vault.hc".into(),
        };
        let input = PlanInput {
            payloads: vec![payload],
            parity: true,
            redundancy_pct: 15,
            headroom_pct: 5,
            defect_management: false,
        };
        let plan = plan::build_plan(&input, &media::synthetic("bd25").unwrap());
        assert!(plan.parity_bytes_est > 0);
        // by Master time the parity files are on disk (sunk cost, already out
        // of the free-space reading) and the ISO estimate contains the parity
        // copies - charging parity again would fail near-capacity burns
        assert_eq!(master_space_needed(&plan), plan.total_bytes_est);
        assert_eq!(
            confirm_space_needed(&plan),
            plan.parity_bytes_est + plan.total_bytes_est
        );
    }

    #[test]
    fn clean_staging_produces_no_note() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        assert_eq!(stale_staging_note(&staging, 1024), None);
        let fresh = staging.join("FRESH");
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(fresh.join("f"), b"x").unwrap();
        assert_eq!(
            stale_staging_note(&staging, 1 << 30),
            None,
            "a small fresh dir is not worth a note"
        );
        assert_eq!(stale_staging_note(Path::new("/nonexistent-xyz"), 1), None);
    }

    #[test]
    fn sanitize_label_rules() {
        assert_eq!(sanitize_label("vault 2026!"), "VAULT_2026_");
        assert_eq!(sanitize_label("ok_LABEL-9"), "OK_LABEL_9");
        assert_eq!(sanitize_label(""), "ARCHIVE");
        assert_eq!(sanitize_label("über"), "_BER");
        let long = sanitize_label(&"a".repeat(50));
        assert_eq!(long.len(), 32);
        assert!(long.chars().all(|c| c == 'A'));
    }

    #[test]
    fn readback_summary_names_the_cache_defeat() {
        assert!(readback_summary(2048, true).contains("(O_DIRECT)"));
        assert!(readback_summary(2048, false).contains("(buffered after disc reload)"));
    }

    #[test]
    fn canonicalize_expands_tilde_in_staging() {
        let (p, warns) = BurnParams {
            label: "T1".into(),
            speed: None,
            redundancy_pct: 15,
            parity: true,
            defect_management: false,
            staging: PathBuf::from("~/burns"),
        }
        .canonicalize();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        assert_eq!(p.staging, home.join("burns"));
        assert!(
            warns.iter().any(|w| w.contains("staging")),
            "expansion must be surfaced as a warning, got {warns:?}"
        );
    }

    #[test]
    fn claim_stage_dir_dedupes_with_numeric_suffix_and_claims_atomically() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (label, path) = claim_stage_dir(dir.path(), "T1").unwrap();
        assert_eq!(label, "T1");
        assert!(path.is_dir(), "claim must create the dir, not just name it");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700,
            "run dirs hold archive material - owner-only"
        );
        let (label, _) = claim_stage_dir(dir.path(), "T1").unwrap();
        assert_eq!(label, "T1_2");
        let (label, _) = claim_stage_dir(dir.path(), "T1").unwrap();
        assert_eq!(label, "T1_3");
        let base = "B".repeat(32);
        std::fs::create_dir(dir.path().join(&base)).unwrap();
        let (next, _) = claim_stage_dir(dir.path(), &base).unwrap();
        assert_eq!(next.len(), 32);
        assert!(next.ends_with("_2"));
    }

    #[test]
    fn staging_free_bytes_reports_space() {
        let dir = tempfile::tempdir().unwrap();
        assert!(staging_free_bytes(dir.path()).unwrap() > 0);
        assert!(staging_free_bytes(Path::new("/nonexistent/nowhere")).is_err());
    }

    #[test]
    fn staging_space_check_rejects_shortfall() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        let mut plan = ArchivePlan {
            payload_bytes: 0,
            parity_bytes_est: u64::MAX / 4,
            overhead_bytes_est: 0,
            total_bytes_est: u64::MAX / 4,
            capacity: 0,
            budget: 0,
            fits: true,
            span: None,
            warnings: vec![],
        };
        let err = check_staging_space(&plan, &staging).unwrap_err();
        assert!(err.to_string().contains("staging"), "{err:#}");
        plan.parity_bytes_est = 0;
        plan.total_bytes_est = 0;
        assert!(check_staging_space(&plan, &staging).is_ok());
    }

    #[test]
    fn listing_mentions_matches_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.hc");
        std::fs::write(&p, b"x").unwrap();
        let listing = format!("1: {} /dev/mapper/veracrypt1 /mnt/v\n", p.display());
        assert!(listing_mentions(&listing, &p));
        assert!(!listing_mentions("", &p));
        assert!(!listing_mentions(
            "1: /other.hc /dev/mapper/veracrypt1 /mnt/v\n",
            &p
        ));
    }

    #[test]
    fn inspect_rejects_duplicate_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a").join("vault.hc");
        let b = dir.path().join("b").join("vault.hc");
        for p in [&a, &b] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        let err = inspect_payloads(&[a, b]).unwrap_err();
        assert!(err.to_string().contains("duplicate payload filename"));
        assert!(inspect_payloads(&[]).is_err());
    }

    #[test]
    fn preflight_refuses_mounted_container() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, b"secret").unwrap();
        let veracrypt = dir.path().join("veracrypt");
        write_script(
            &veracrypt,
            &format!(
                "#!/bin/sh\necho '1: {} /dev/mapper/veracrypt1 /mnt/v'\n",
                payload.display()
            ),
        );
        let tools = tools_with("/bin/true".into(), None, None, Some(veracrypt));
        let (ctx, _rx, _ack) = ctx_pair(
            cfg_with(Path::new("/dev/null"), &dir.path().join("staging")),
            tools,
        );
        let err = preflight_probe(&ctx, &[payload]).unwrap_err();
        assert!(
            err.to_string().contains("MOUNTED VeraCrypt container"),
            "{err:#}"
        );
    }

    #[test]
    fn veracrypt_no_volumes_is_the_clean_empty_case() {
        // `--text --list` exits 1 with this line when nothing is mounted.
        let dir = tempfile::tempdir().unwrap();
        let veracrypt = dir.path().join("veracrypt");
        write_script(
            &veracrypt,
            "#!/bin/sh\necho 'No volumes mounted.' >&2\nexit 1\n",
        );
        assert_eq!(veracrypt_list(&veracrypt).unwrap(), "");
    }

    #[test]
    fn veracrypt_other_failure_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let veracrypt = dir.path().join("veracrypt");
        write_script(&veracrypt, "#!/bin/sh\necho 'boom' >&2\nexit 3\n");
        let err = veracrypt_list(&veracrypt).unwrap_err();
        assert!(err.to_string().contains("could not determine"), "{err:#}");
    }

    // Probe-only fake: reports media in exactly the `loaded` devices,
    // "no medium" for every other -outdev.
    fn fake_xorriso_probe_devices(dir: &Path, loaded: &[&Path]) -> PathBuf {
        let path = dir.join("xorriso");
        let cases: String = loaded
            .iter()
            .map(|d| {
                format!(
                    "             \"{}\") cat \"{PROBE_FIXTURE}\"; exit 0 ;;\n",
                    d.display()
                )
            })
            .collect();
        write_script(
            &path,
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"-outdev\" ] && [ \"$3\" = \"-toc\" ]; then\n\
                   case \"$2\" in\n\
                 {cases}\
                             *) echo 'xorriso : SORRY : Cannot obtain format list info: no medium in drive' >&2; exit 1 ;;\n\
                   esac\n\
                 fi\n\
                 exit 1\n"
            ),
        );
        path
    }

    fn probe_ctx(
        dir: &Path,
        configured: &Path,
        explicit: bool,
        loaded: &[&Path],
    ) -> (RunnerCtx, Receiver<StageEvent>) {
        let xorriso = fake_xorriso_probe_devices(dir, loaded);
        let tools = tools_with(xorriso, None, None, None);
        let mut cfg = cfg_with(configured, &dir.join("staging"));
        cfg.device_explicit = explicit;
        let (ctx, rx, _ack) = ctx_pair(cfg, tools);
        (ctx, rx)
    }

    fn auto_select_infos(rx: &Receiver<StageEvent>) -> Vec<String> {
        rx.try_iter()
            .filter_map(|ev| match ev {
                StageEvent::Info(t) if t.contains("auto-selected") => Some(t),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn resolve_device_uses_configured_when_it_has_media() {
        let dir = tempfile::tempdir().unwrap();
        let sr0 = dir.path().join("sr0");
        let sr1 = dir.path().join("sr1");
        let (ctx, rx) = probe_ctx(dir.path(), &sr0, false, &[&sr0, &sr1]);
        let candidates = vec![sr0.display().to_string(), sr1.display().to_string()];
        let (device, media) =
            resolve_device_from(&ctx, move || candidates).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(device, sr0.display().to_string());
        assert!(media.blank);
        assert!(auto_select_infos(&rx).is_empty());
    }

    #[test]
    fn resolve_device_auto_selects_single_drive_with_media() {
        let dir = tempfile::tempdir().unwrap();
        let sr0 = dir.path().join("sr0");
        let sr1 = dir.path().join("sr1");
        let sr2 = dir.path().join("sr2");
        let (ctx, rx) = probe_ctx(dir.path(), &sr0, false, &[&sr1]);
        let candidates = vec![
            sr0.display().to_string(),
            sr1.display().to_string(),
            sr2.display().to_string(),
        ];
        let (device, _media) =
            resolve_device_from(&ctx, move || candidates).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(device, sr1.display().to_string());
        let infos = auto_select_infos(&rx);
        assert_eq!(infos.len(), 1, "one loud auto-select line: {infos:?}");
        assert!(infos[0].contains(&sr1.display().to_string()));
    }

    #[test]
    fn resolve_device_error_when_explicit_and_no_medium() {
        let dir = tempfile::tempdir().unwrap();
        let sr0 = dir.path().join("sr0");
        let sr1 = dir.path().join("sr1");
        let (ctx, rx) = probe_ctx(dir.path(), &sr0, true, &[&sr1]);
        let candidates = vec![sr0.display().to_string(), sr1.display().to_string()];
        let e = resolve_device_from(&ctx, move || candidates).unwrap_err();
        assert!(e.to_string().contains("probing"), "{e:#}");
        assert!(
            auto_select_infos(&rx).is_empty(),
            "an explicit device must never be swapped"
        );
    }

    #[test]
    fn resolve_device_refuses_ambiguous_drives() {
        let dir = tempfile::tempdir().unwrap();
        let sr0 = dir.path().join("sr0");
        let sr1 = dir.path().join("sr1");
        let sr2 = dir.path().join("sr2");
        let (ctx, _rx) = probe_ctx(dir.path(), &sr0, false, &[&sr1, &sr2]);
        let candidates = vec![
            sr0.display().to_string(),
            sr1.display().to_string(),
            sr2.display().to_string(),
        ];
        let e = resolve_device_from(&ctx, move || candidates).unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("multiple drives have media"), "{msg}");
        assert!(msg.contains(&sr1.display().to_string()), "{msg}");
        assert!(msg.contains(&sr2.display().to_string()), "{msg}");
        assert!(msg.contains("--device"), "{msg}");
        assert!(
            e.downcast_ref::<AmbiguousDrives>().is_some(),
            "plan needs the typed error to propagate ambiguity"
        );
    }

    // TWO oversized payloads: a single big file would legitimately span a
    // multi-disc set now, so the does-not-fit bail needs a non-spannable mix.
    fn oversized_setup(dir: &Path) -> (Vec<PathBuf>, PathBuf) {
        let mut payloads = Vec::new();
        for name in ["big.hc", "big2.hc"] {
            let payload = dir.join(name);
            let f = std::fs::File::create(&payload).unwrap();
            f.set_len(30 * 1024 * 1024 * 1024).unwrap();
            payloads.push(payload);
        }
        let device = dir.join("device");
        (payloads, device)
    }

    #[test]
    fn burn_bails_with_numbers_on_proceed_when_plan_does_not_fit() {
        let dir = tempfile::tempdir().unwrap();
        let (payloads, device) = oversized_setup(dir.path());
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, rx, ack_tx) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
        ack_tx.send(Ack::Proceed).unwrap();
        let req = BurnRequest {
            payloads,
            label: None,
            parity: true,
            dry_run: false,
            assume_yes: false,
            amend: true,
            discard_iso: false,
        };
        let e = run_burn(&ctx, &req).unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("does not fit"), "{msg}");
        assert!(msg.contains("bytes"), "{msg}");
        let events: Vec<StageEvent> = rx.try_iter().collect();
        assert!(events
            .iter()
            .any(|ev| matches!(ev, StageEvent::Plan { .. })));
        assert!(
            events
                .iter()
                .any(|ev| matches!(ev, StageEvent::NeedAck { .. })),
            "amend mode must surface a non-fitting plan instead of bailing early"
        );
        assert!(matches!(
            events.last(),
            Some(StageEvent::Failed {
                stage: Stage::Preflight,
                ..
            })
        ));
    }

    #[test]
    fn cli_burn_without_amend_bails_before_prompt_when_not_fitting() {
        let dir = tempfile::tempdir().unwrap();
        let (payloads, device) = oversized_setup(dir.path());
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, rx, ack_tx) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
        // a wrongly-emitted NeedAck must fail as "ui channel closed", not hang
        drop(ack_tx);
        let req = BurnRequest {
            payloads,
            label: None,
            parity: true,
            dry_run: false,
            assume_yes: false,
            amend: false,
            discard_iso: false,
        };
        let e = run_burn(&ctx, &req).unwrap_err();
        assert!(format!("{e:#}").contains("does not fit"), "{e:#}");
        let events: Vec<StageEvent> = rx.try_iter().collect();
        assert!(!events
            .iter()
            .any(|ev| matches!(ev, StageEvent::NeedAck { .. })));
    }

    #[test]
    fn burn_params_resolve_from_config_and_request() {
        let mut cfg = cfg_with(Path::new("/dev/sr0"), Path::new("/tmp/s"));
        cfg.speed = Some(8);
        cfg.redundancy_pct = 20;
        cfg.defect_management = true;
        let mut req = BurnRequest {
            payloads: vec![],
            label: Some("hello world".into()),
            parity: false,
            dry_run: false,
            assume_yes: false,
            amend: false,
            discard_iso: false,
        };
        let p = BurnParams::resolve(&cfg, &req);
        assert_eq!(p.label, "HELLO_WORLD");
        assert_eq!(p.speed, Some(8));
        assert_eq!(p.redundancy_pct, 20);
        assert!(!p.parity);
        assert!(p.defect_management);
        req.label = None;
        assert!(BurnParams::resolve(&cfg, &req)
            .label
            .starts_with("ARCHIVE_"));
    }

    #[test]
    fn ask_rejects_amend_outside_confirm_loop() {
        let tools = tools_with("/bin/true".into(), None, None, None);
        let (ctx, _rx, ack_tx) =
            ctx_pair(cfg_with(Path::new("/dev/null"), Path::new("/tmp/s")), tools);
        ack_tx
            .send(Ack::Amend(BurnParams {
                label: "X".into(),
                speed: None,
                redundancy_pct: 15,
                parity: true,
                defect_management: false,
                staging: "/tmp/s".into(),
            }))
            .unwrap();
        let err = ctx.ask("proceed?").unwrap_err();
        assert!(err.to_string().contains("unexpected parameter amendment"));
    }

    #[test]
    fn amend_replans_and_pipeline_uses_amended_params() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        // 8 MiB = 128 par2 blocks, so 15% vs 25% redundancy differ visibly
        std::fs::write(&payload, vec![7u8; 8 * 1024 * 1024]).unwrap();
        let device = dir.path().join("device");
        let staging = dir.path().join("staging");
        let stage_dir = staging.join("T2_");
        let mnt = dir.path().join("mnt");
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let par2 = fake_par2(dir.path());
        let udisksctl = fake_udisksctl(dir.path(), &mnt, &payload, &stage_dir);
        let tools = tools_with(xorriso, Some(par2), Some(udisksctl), None);
        let (ctx, rx, ack_tx) = ctx_pair(cfg_with(&device, &staging), tools);
        ack_tx
            .send(Ack::Amend(BurnParams {
                label: "t2!".into(),
                speed: Some(6),
                redundancy_pct: 25,
                parity: true,
                defect_management: false,
                staging: staging.clone(),
            }))
            .unwrap();
        ack_tx.send(Ack::Proceed).unwrap();
        let req = BurnRequest {
            payloads: vec![payload.clone()],
            label: Some("T1".into()),
            parity: true,
            dry_run: false,
            assume_yes: false,
            amend: true,
            discard_iso: false,
        };
        run_burn(&ctx, &req).unwrap_or_else(|e| panic!("{e:#}"));
        let events: Vec<StageEvent> = rx.try_iter().collect();

        let plans: Vec<(&BurnParams, u64)> = events
            .iter()
            .filter_map(|ev| match ev {
                StageEvent::Plan { params, plan, .. } => Some((params, plan.parity_bytes_est)),
                _ => None,
            })
            .collect();
        assert_eq!(plans.len(), 2, "one Plan event per confirm-loop iteration");
        assert_eq!(plans[0].0.label, "T1");
        assert_eq!(plans[0].0.redundancy_pct, 15);
        assert_eq!(plans[1].0.label, "T2_");
        assert_eq!(plans[1].0.speed, Some(6));
        assert_eq!(plans[1].0.redundancy_pct, 25);
        assert!(
            plans[1].1 > plans[0].1,
            "25% redundancy must estimate more parity than 15%"
        );

        let stage_dir = dir.path().join("staging").join("T2_");
        assert!(stage_dir.join("T2_.iso").is_file());
        let argv =
            std::fs::read_to_string(stage_dir.join("parity").join("vault.hc.par2.argv")).unwrap();
        assert!(argv.lines().any(|l| l == "-r25"), "par2 argv: {argv}");
        let cdrecord = std::fs::read_to_string(dir.path().join("device.cdrecord_argv")).unwrap();
        assert!(
            cdrecord.lines().any(|l| l == "speed=6"),
            "cdrecord argv: {cdrecord}"
        );
    }

    #[test]
    fn amend_toggle_defect_management_runs_format_stage() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, vec![7u8; 128 * 1024]).unwrap();
        let device = dir.path().join("device");
        let staging = dir.path().join("staging");
        let stage_dir = staging.join("T1");
        let mnt = dir.path().join("mnt");
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let par2 = fake_par2(dir.path());
        let udisksctl = fake_udisksctl(dir.path(), &mnt, &payload, &stage_dir);
        let tools = tools_with(xorriso, Some(par2), Some(udisksctl), None);
        let (ctx, rx, ack_tx) = ctx_pair(cfg_with(&device, &staging), tools);
        ack_tx
            .send(Ack::Amend(BurnParams {
                label: "T1".into(),
                speed: None,
                redundancy_pct: 15,
                parity: true,
                defect_management: true,
                staging: staging.clone(),
            }))
            .unwrap();
        ack_tx.send(Ack::Proceed).unwrap();
        let req = BurnRequest {
            payloads: vec![payload.clone()],
            label: Some("T1".into()),
            parity: true,
            dry_run: false,
            assume_yes: false,
            amend: true,
            discard_iso: false,
        };
        run_burn(&ctx, &req).unwrap_or_else(|e| panic!("{e:#}"));
        let events: Vec<StageEvent> = rx.try_iter().collect();

        let last_params = events
            .iter()
            .rev()
            .find_map(|ev| match ev {
                StageEvent::Plan { params, .. } => Some(params),
                _ => None,
            })
            .expect("a Plan event");
        assert!(last_params.defect_management);
        assert!(
            events
                .iter()
                .any(|ev| matches!(ev, StageEvent::StageStart(Stage::Format))),
            "amended defect management must run the Format stage"
        );
    }

    #[test]
    fn amend_canonicalizes_out_of_range_values() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, vec![7u8; 4096]).unwrap();
        let device = dir.path().join("device");
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, rx, ack_tx) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
        ack_tx
            .send(Ack::Amend(BurnParams {
                label: "".into(),
                speed: Some(0),
                redundancy_pct: 0,
                parity: true,
                defect_management: false,
                staging: dir.path().join("staging"),
            }))
            .unwrap();
        ack_tx.send(Ack::Abort).unwrap();
        let req = BurnRequest {
            payloads: vec![payload],
            label: None,
            parity: true,
            dry_run: false,
            assume_yes: false,
            amend: true,
            discard_iso: false,
        };
        let e = run_burn(&ctx, &req).unwrap_err();
        assert!(e.to_string().contains("aborted by user"), "{e:#}");
        let events: Vec<StageEvent> = rx.try_iter().collect();
        let last_params = events
            .iter()
            .rev()
            .find_map(|ev| match ev {
                StageEvent::Plan { params, .. } => Some(params),
                _ => None,
            })
            .expect("a Plan event");
        assert_eq!(last_params.label, "ARCHIVE");
        assert_eq!(last_params.speed, None);
        assert_eq!(last_params.redundancy_pct, 1);
        let warns: Vec<&String> = events
            .iter()
            .filter_map(|ev| match ev {
                StageEvent::Warn(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(warns.iter().any(|t| t.contains("sanitized")), "{warns:?}");
        assert!(
            warns.iter().any(|t| t.contains("out of range")),
            "{warns:?}"
        );
        assert!(
            warns.iter().any(|t| t.contains("drive default")),
            "{warns:?}"
        );
    }

    #[test]
    fn run_burn_dry_run_stops_after_plan() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, vec![7u8; 4096]).unwrap();
        let device = dir.path().join("device");
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
        let req = BurnRequest {
            payloads: vec![payload],
            label: None,
            parity: true,
            dry_run: true,
            assume_yes: false,
            amend: false,
            discard_iso: false,
        };
        run_burn(&ctx, &req).unwrap_or_else(|e| panic!("{e:#}"));
        let events: Vec<StageEvent> = rx.try_iter().collect();
        assert!(matches!(
            events.first(),
            Some(StageEvent::StageStart(Stage::Preflight))
        ));
        assert!(events
            .iter()
            .any(|ev| matches!(ev, StageEvent::Plan { .. })));
        let Some(StageEvent::Finished { report }) = events.last() else {
            panic!("expected Finished, got {:?}", events.last());
        };
        assert!(report.iso_path.is_none());
        assert!(report.stages.is_empty());
    }

    #[test]
    fn run_burn_aborts_on_nack() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, vec![7u8; 4096]).unwrap();
        let device = dir.path().join("device");
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, rx, ack_tx) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
        ack_tx.send(Ack::Abort).unwrap();
        let req = BurnRequest {
            payloads: vec![payload],
            label: None,
            parity: false,
            dry_run: false,
            assume_yes: false,
            amend: false,
            discard_iso: false,
        };
        let e = run_burn(&ctx, &req).unwrap_err();
        assert!(e.to_string().contains("aborted by user"));
        let events: Vec<StageEvent> = rx.try_iter().collect();
        assert!(events
            .iter()
            .any(|ev| matches!(ev, StageEvent::NeedAck { .. })));
        assert!(events
            .iter()
            .any(|ev| matches!(ev, StageEvent::Failed { .. })));
    }

    #[test]
    fn run_burn_full_pipeline_with_fakes() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, vec![7u8; 128 * 1024]).unwrap();
        let device = dir.path().join("device");
        let staging = dir.path().join("staging");
        let stage_dir = staging.join("T1");
        let mnt = dir.path().join("mnt");
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let par2 = fake_par2(dir.path());
        let udisksctl = fake_udisksctl(dir.path(), &mnt, &payload, &stage_dir);
        let tools = tools_with(xorriso, Some(par2), Some(udisksctl), None);
        let (ctx, rx, _ack) = ctx_pair(cfg_with(&device, &staging), tools);
        let req = BurnRequest {
            payloads: vec![payload.clone()],
            label: Some("T1".into()),
            parity: true,
            dry_run: false,
            assume_yes: true,
            amend: false,
            discard_iso: false,
        };
        run_burn(&ctx, &req).unwrap_or_else(|e| panic!("{e:#}"));
        let events: Vec<StageEvent> = rx.try_iter().collect();
        let Some(StageEvent::Finished { report }) = events.last() else {
            panic!("expected Finished, got {:?}", events.last());
        };

        let starts: Vec<Stage> = events
            .iter()
            .filter_map(|ev| match ev {
                StageEvent::StageStart(s) => Some(*s),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![
                Stage::Preflight,
                Stage::Parity,
                Stage::Checksums,
                Stage::Master,
                Stage::Burn,
                Stage::VerifyImage,
                Stage::VerifyFiles,
            ]
        );
        assert_eq!(report.stages.len(), 7);
        // the fake master emits a minimal valid ISO: 16 zero sectors + PVD
        assert_eq!(report.iso_bytes, 34816);

        let stage_dir = dir.path().join("staging").join("T1");
        let iso = stage_dir.join("T1.iso");
        assert_eq!(report.iso_path.as_deref(), Some(iso.as_path()));
        assert!(iso.is_file());
        assert!(stage_dir.join("T1.lba.txt").is_file());
        assert!(stage_dir.join("MANIFEST.txt").is_file());
        assert!(stage_dir.join("RECOVERY.txt").is_file());

        let iso_sha = hashing::sha256_file(&iso, &mut |_, _| {}).unwrap();
        assert_eq!(report.iso_sha256.as_deref(), Some(iso_sha.as_str()));
        let sidecar = std::fs::read_to_string(stage_dir.join("T1.iso.sha256")).unwrap();
        assert!(sidecar.starts_with(&iso_sha));

        let checksums = std::fs::read_to_string(stage_dir.join("checksums.sha256")).unwrap();
        let parsed = hashing::parse_checksums(&checksums).unwrap();
        assert_eq!(parsed.len(), 3); // payload + 2 par2 files
        assert!(parsed.iter().any(|(_, rel)| rel == "vault.hc"));
        assert!(parsed.iter().any(|(_, rel)| rel == "parity/vault.hc.par2"));

        assert!(report.reminders.iter().any(|r| r.contains("burn-iso")));
        assert!(report.reminders.iter().any(|r| r.contains("off-disc")));
        assert!(report.reminders.iter().any(|r| r.contains("VeraCrypt")));
        assert!(events.iter().any(
            |ev| matches!(ev, StageEvent::Progress { stage: Stage::Burn, pct: Some(p), .. } if *p == 100.0)
        ));
    }

    // Minimal valid ISO 9660 wrapper around `data` (the truncation self-check
    // parses the PVD, so a bare byte blob no longer passes as an image).
    fn synthetic_iso_bytes(data: &[u8]) -> Vec<u8> {
        let pad = (2048 - data.len() % 2048) % 2048;
        let total_blocks = ((32768 + 2048 + data.len() + pad) / 2048) as u32;
        let mut v = vec![0u8; 32768];
        let mut pvd = [0u8; 2048];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        pvd[80..84].copy_from_slice(&total_blocks.to_le_bytes());
        pvd[84..88].copy_from_slice(&total_blocks.to_be_bytes());
        pvd[128..130].copy_from_slice(&2048u16.to_le_bytes());
        pvd[130..132].copy_from_slice(&2048u16.to_be_bytes());
        v.extend_from_slice(&pvd);
        v.extend_from_slice(data);
        v.extend(std::iter::repeat_n(0u8, pad));
        v
    }

    #[test]
    fn run_burn_iso_verifies_against_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("copy.iso");
        std::fs::write(&iso, synthetic_iso_bytes(&[9u8; 8192])).unwrap();
        let sha = hashing::sha256_file(&iso, &mut |_, _| {}).unwrap();
        hashing::write_checksums(
            &[(sha.clone(), "copy.iso".into())],
            &dir.path().join("copy.iso.sha256"),
        )
        .unwrap();
        let device = dir.path().join("device");
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
        run_burn_iso(&ctx, &iso, true).unwrap_or_else(|e| panic!("{e:#}"));
        let events: Vec<StageEvent> = rx.try_iter().collect();
        let Some(StageEvent::Finished { report }) = events.last() else {
            panic!("expected Finished, got {:?}", events.last());
        };
        assert_eq!(report.iso_sha256.as_deref(), Some(sha.as_str()));
        let log = dir.path().join("copy.burn.log");
        assert_eq!(
            report.written_files,
            vec![
                dir.path().join("copy.run.log"),
                log.clone(),
                dir.path().join("copy.report.txt"),
            ]
        );
        assert!(log.is_file());
        assert!(events
            .iter()
            .any(|ev| matches!(ev, StageEvent::Info(t) if t.contains("recorded sha256"))));
    }

    #[test]
    fn run_burn_iso_flags_readback_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("copy.iso");
        std::fs::write(&iso, synthetic_iso_bytes(&[9u8; 8192])).unwrap();
        hashing::write_checksums(
            &[("ab".repeat(32), "copy.iso".into())],
            &dir.path().join("copy.iso.sha256"),
        )
        .unwrap();
        let device = dir.path().join("device");
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, _rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
        let e = run_burn_iso(&ctx, &iso, true).unwrap_err();
        assert!(e.to_string().contains("DO NOT TRUST"), "{e:#}");
    }

    #[test]
    fn run_check_reports_clean_and_damaged() {
        for (fixture, want_clean) in [(CHECK_CLEAN_FIXTURE, true), (CHECK_DAMAGED_FIXTURE, false)] {
            let dir = tempfile::tempdir().unwrap();
            let device = dir.path().join("device");
            std::fs::write(&device, vec![0u8; 4096]).unwrap();
            let xorriso = fake_xorriso_probing(dir.path(), &device, PROBE_WRITTEN_FIXTURE, fixture);
            let tools = tools_with(xorriso, None, None, None);
            let (ctx, rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
            let res = run_check(&ctx, None);
            match (&res, want_clean) {
                (Ok(()), true) => {
                    let events: Vec<StageEvent> = rx.try_iter().collect();
                    assert!(matches!(events.last(), Some(StageEvent::Finished { .. })));
                }
                (Err(e), false) => assert!(e.to_string().contains("DAMAGED"), "{e:#}"),
                (r, _) => panic!("unexpected result {r:?} for clean={want_clean}"),
            }
        }
    }

    #[test]
    fn run_info_save_persists_resolved_device() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device");
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
        let cfg_path = dir.path().join("cfg").join("config.toml");
        run_info(&ctx, Some(&cfg_path)).unwrap_or_else(|e| panic!("{e:#}"));
        let text = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(text.contains(&device.display().to_string()));
        let events: Vec<StageEvent> = rx.try_iter().collect();
        assert!(events
            .iter()
            .any(|ev| matches!(ev, StageEvent::Info(t) if t.contains("saved device"))));
    }

    #[test]
    fn run_check_save_persists_resolved_device() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device");
        std::fs::write(&device, vec![0u8; 4096]).unwrap();
        let xorriso = fake_xorriso_probing(
            dir.path(),
            &device,
            PROBE_WRITTEN_FIXTURE,
            CHECK_CLEAN_FIXTURE,
        );
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, _rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
        let cfg_path = dir.path().join("config.toml");
        run_check(&ctx, Some(&cfg_path)).unwrap_or_else(|e| panic!("{e:#}"));
        let text = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(text.contains(&device.display().to_string()));
    }

    #[test]
    fn run_check_refuses_blank_medium() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device");
        std::fs::write(&device, vec![0u8; 4096]).unwrap();
        // default probe fixture reports a blank BD-R
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, _rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
        let e = run_check(&ctx, None).unwrap_err();
        assert!(e.to_string().contains("blank"), "{e:#}");
    }

    #[test]
    fn run_check_auto_detect_fails_fast_without_medium() {
        let dir = tempfile::tempdir().unwrap();
        let xorriso = dir.path().join("xorriso");
        write_script(
            &xorriso,
            "#!/bin/sh\necho 'xorriso : FAILURE : Cannot acquire drive' >&2\nexit 1\n",
        );
        let tools = tools_with(xorriso, None, None, None);
        let mut cfg = cfg_with(Path::new("/dev/sr9"), &dir.path().join("s"));
        cfg.device_explicit = false;
        let (ctx, rx, _ack) = ctx_pair(cfg, tools);
        let start = std::time::Instant::now();
        let err = run_check(&ctx, None).unwrap_err();
        // the old code polled wait_ready for 180s before giving up
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "check must fail fast when no drive has a medium"
        );
        assert!(err.to_string().contains("probing"), "{err:#}");
        let events: Vec<StageEvent> = rx.try_iter().collect();
        assert!(matches!(
            events.last(),
            Some(StageEvent::Failed {
                stage: Stage::CheckMedia,
                ..
            })
        ));
    }

    #[test]
    fn run_info_emits_media_fields() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device");
        let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
        run_info(&ctx, None).unwrap_or_else(|e| panic!("{e:#}"));
        let events: Vec<StageEvent> = rx.try_iter().collect();
        let outs: Vec<&String> = events
            .iter()
            .filter_map(|ev| match ev {
                StageEvent::Out(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(outs.iter().any(|t| t.contains("BD-R 25 GB")));
        assert!(outs.iter().any(|t| t.contains("free")));
        assert!(outs.iter().any(|t| t.contains("write speeds")));
        assert!(matches!(events.last(), Some(StageEvent::Finished { .. })));
    }

    #[test]
    fn run_plan_uses_synthetic_media_on_hint() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, vec![1u8; 4096]).unwrap();
        let tools = tools_with("/bin/false".into(), None, None, None);
        let (ctx, rx, _ack) = ctx_pair(
            cfg_with(Path::new("/dev/null"), &dir.path().join("s")),
            tools,
        );
        run_plan(&ctx, &[payload], Some("bd25")).unwrap();
        let events: Vec<StageEvent> = rx.try_iter().collect();
        let Some(StageEvent::Plan { media, plan, .. }) = events
            .iter()
            .find(|ev| matches!(ev, StageEvent::Plan { .. }))
        else {
            panic!("no Plan event");
        };
        assert_eq!(media.free_bytes, plan::BD_R_25);
        assert!(plan.fits);
        assert!(matches!(events.last(), Some(StageEvent::Finished { .. })));
    }

    #[test]
    fn run_plan_falls_back_to_bd25_when_probe_fails() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, vec![1u8; 4096]).unwrap();
        let xorriso = dir.path().join("xorriso");
        write_script(
            &xorriso,
            "#!/bin/sh\necho 'xorriso : FAILURE : Cannot acquire drive' >&2\nexit 1\n",
        );
        let tools = tools_with(xorriso, None, None, None);
        let (ctx, rx, _ack) = ctx_pair(
            cfg_with(Path::new("/dev/sr9"), &dir.path().join("s")),
            tools,
        );
        run_plan(&ctx, &[payload], None).unwrap_or_else(|e| panic!("{e:#}"));
        let events: Vec<StageEvent> = rx.try_iter().collect();
        assert!(events
            .iter()
            .any(|ev| matches!(ev, StageEvent::Info(t) if t.contains("assuming a blank BD-R 25"))));
        assert!(events
            .iter()
            .any(|ev| matches!(ev, StageEvent::Plan { .. })));
    }
}
