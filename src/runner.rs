use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};

use crate::config::Config;
use crate::plan::{self, human_bytes, ArchivePlan, MediaInfo, Payload, PlanInput};
use crate::tools::Tools;
use crate::{burn, hashing, master, media, parity, verify};

const READY_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    Preflight,
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
}

impl BurnParams {
    pub fn resolve(cfg: &Config, req: &BurnRequest) -> Self {
        Self {
            label: sanitize_label(req.label.as_deref().unwrap_or(&default_label())),
            speed: cfg.speed,
            redundancy_pct: cfg.redundancy_pct,
            parity: req.parity,
            defect_management: cfg.defect_management,
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
}

impl RunnerCtx {
    pub fn send(&self, ev: StageEvent) {
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

fn burn_pipeline(ctx: &RunnerCtx, req: &BurnRequest, stage: &mut Stage) -> Result<()> {
    let mut stages: Vec<(Stage, String)> = Vec::new();

    *stage = Stage::Preflight;
    ctx.start(Stage::Preflight);
    let (payloads, device, media) = preflight_probe(ctx, &req.payloads)?;
    let mut params = BurnParams::resolve(&ctx.cfg, req);
    let mut prev_warnings: Vec<String> = Vec::new();
    let mut prev_staging_warn: Option<String> = None;
    // Confirm loop: the plan on screen is always one this loop computed for the
    // params it holds; Amend re-plans (pure — media stays probed once).
    let (params, plan) = loop {
        let plan = plan::build_plan(&params.plan_input(&payloads, ctx.cfg.headroom_pct), &media);
        for w in plan.warnings.iter().filter(|w| !prev_warnings.contains(w)) {
            ctx.warn(w.clone());
        }
        prev_warnings.clone_from(&plan.warnings);
        ctx.send(StageEvent::Plan {
            device: device.clone(),
            media: media.clone(),
            plan: plan.clone(),
            params: params.clone(),
        });

        if req.dry_run {
            confirm_gate(ctx, &plan)?;
            ctx.info("dry run - stopping after plan".into());
            ctx.send(StageEvent::Finished {
                report: RunReport::default(),
            });
            return Ok(());
        }
        if req.assume_yes {
            confirm_gate(ctx, &plan)?;
            break (params, plan);
        }
        if !req.amend {
            confirm_gate(ctx, &plan)?;
        } else if let Err(e) = check_staging_space(ctx, &plan) {
            // surfaced pre-confirm: lowering redundancy shrinks the need
            let msg = format!("{e:#}");
            if prev_staging_warn.as_deref() != Some(msg.as_str()) {
                ctx.warn(msg.clone());
                prev_staging_warn = Some(msg);
            }
        }

        let prompt = if plan.fits {
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
                confirm_gate(ctx, &plan)?;
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

    ctx.done(
        &mut stages,
        Stage::Preflight,
        format!(
            "{}: payload {} + parity ~{} fits {} budget",
            media.kind.label(),
            human_bytes(plan.payload_bytes),
            human_bytes(plan.parity_bytes_est),
            human_bytes(plan.budget)
        ),
    );

    let label = unique_label(&ctx.cfg.staging, &params.label);
    let stage_dir = ctx.cfg.staging.join(&label);
    std::fs::create_dir_all(stage_dir.join("parity"))
        .with_context(|| format!("create staging dir {}", stage_dir.display()))?;
    ctx.info(format!("staging into {}", stage_dir.display()));

    let mut parity_files: Vec<PathBuf> = Vec::new();
    if params.parity {
        *stage = Stage::Parity;
        ctx.start(Stage::Parity);
        let n = payloads.len() as f32;
        for (i, p) in payloads.iter().enumerate() {
            let name = p.name.clone();
            let mut produced = parity::create(
                &ctx.tools,
                p,
                &stage_dir.join("parity"),
                params.redundancy_pct,
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
        ctx.done(
            &mut stages,
            Stage::Parity,
            format!(
                "{} recovery files, {} ({}% redundancy)",
                parity_files.len(),
                human_bytes(parity_bytes),
                params.redundancy_pct
            ),
        );
    } else {
        ctx.warn("parity disabled - a single bad sector can cost the whole payload".into());
    }

    *stage = Stage::Checksums;
    ctx.start(Stage::Checksums);
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut manifest_rows: Vec<master::ManifestEntry> = Vec::new();
    {
        let parity_sizes: Vec<u64> = parity_files
            .iter()
            .map(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0))
            .collect();
        let total: u64 =
            payloads.iter().map(|p| p.total_size).sum::<u64>() + parity_sizes.iter().sum::<u64>();
        let mut base = 0u64;
        let mut th = Throttle::default();
        for p in &payloads {
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
            let rel = format!("parity/{}", file_name_string(f));
            let sha = hashing::sha256_file(f, &mut |done, _| {
                emit_pct(ctx, Stage::Checksums, &mut th, base + done, total, &rel);
            })?;
            base += size;
            entries.push((sha, rel));
        }
    }
    let checksums_path = stage_dir.join("checksums.sha256");
    hashing::write_checksums(&entries, &checksums_path)?;
    ctx.done(
        &mut stages,
        Stage::Checksums,
        format!("{} entries in checksums.sha256", entries.len()),
    );

    *stage = Stage::Master;
    ctx.start(Stage::Master);
    let manifest_path = stage_dir.join("MANIFEST.txt");
    let recovery_path = stage_dir.join("RECOVERY.txt");
    master::write_manifest(
        &manifest_path,
        &label,
        &manifest_rows,
        params.parity.then_some(params.redundancy_pct),
        params.defect_management,
    )?;
    master::write_recovery(&recovery_path, &label, &payloads)?;
    let iso = stage_dir.join(format!("{label}.iso"));
    let input = master::MasterInput {
        label: &label,
        payloads: &payloads,
        parity_files: &parity_files,
        checksums: &checksums_path,
        manifest: &manifest_path,
        recovery: &recovery_path,
        out_iso: &iso,
    };
    let iso_bytes = master::build_iso(&ctx.tools, &input, &mut |pct, line| {
        ctx.progress(Stage::Master, pct, line);
    })?;
    master::report_lba(
        &ctx.tools,
        &iso,
        &stage_dir.join(format!("{label}.lba.txt")),
    )?;
    let iso_sha = {
        let mut th = Throttle::default();
        hashing::sha256_file(&iso, &mut |done, total| {
            emit_pct(ctx, Stage::Master, &mut th, done, total, "hashing ISO");
        })?
    };
    hashing::write_checksums(
        &[(iso_sha.clone(), format!("{label}.iso"))],
        &stage_dir.join(format!("{label}.iso.sha256")),
    )?;
    ctx.done(
        &mut stages,
        Stage::Master,
        format!("{label}.iso {} sha256 {iso_sha}", human_bytes(iso_bytes)),
    );

    if params.defect_management {
        *stage = Stage::Format;
        ctx.start(Stage::Format);
        burn::format_defect_management(&ctx.tools, &device, &mut |pct, line| {
            ctx.progress(Stage::Format, pct, line);
        })?;
        let formatted = media::probe(&ctx.tools, &device)?;
        let capacity = formatted.formatted_capacity.unwrap_or(formatted.free_bytes);
        ensure!(
            iso_bytes <= capacity,
            "ISO {} ({iso_bytes} bytes) no longer fits: formatted capacity is {} ({capacity} bytes)",
            human_bytes(iso_bytes),
            human_bytes(capacity)
        );
        ctx.done(
            &mut stages,
            Stage::Format,
            format!(
                "spare areas formatted, capacity now {}",
                human_bytes(capacity)
            ),
        );
    }

    *stage = Stage::Burn;
    ctx.start(Stage::Burn);
    burn::burn_iso(&ctx.tools, &device, &iso, params.speed, &mut |pct, line| {
        ctx.progress(Stage::Burn, pct, line);
    })
    .inspect_err(|_| {
        ctx.info(format!(
            "burn transcript: {}",
            burn::burn_log_path(&iso).display()
        ));
        ctx.info(format!(
            "staged ISO survives - insert a fresh disc and retry: ovenmitts burn-iso {}",
            iso.display()
        ));
    })?;
    ctx.done(
        &mut stages,
        Stage::Burn,
        format!("{} written, disc ejected", human_bytes(iso_bytes)),
    );

    *stage = Stage::VerifyImage;
    ctx.start(Stage::VerifyImage);
    let disc_sha = readback_stage(ctx, &device, iso_bytes)?;
    ensure!(
        disc_sha == iso_sha,
        "READ-BACK MISMATCH - DO NOT TRUST THIS DISC: disc sha256 {disc_sha} != ISO sha256 {iso_sha}"
    );
    ctx.done(
        &mut stages,
        Stage::VerifyImage,
        format!("{} read back, sha256 matches ISO", human_bytes(iso_bytes)),
    );

    *stage = Stage::VerifyFiles;
    ctx.start(Stage::VerifyFiles);
    let mountpoint = verify::mount_ro(&ctx.tools, &device)?;
    let verified = {
        let mut th = Throttle::default();
        let res = hashing::verify_checksums(&mountpoint, &entries, &mut |done, total| {
            emit_pct(
                ctx,
                Stage::VerifyFiles,
                &mut th,
                done,
                total,
                "hashing files on disc",
            );
        });
        if let Err(e) = verify::unmount(&ctx.tools, &device) {
            ctx.warn(format!("could not unmount {device}: {e:#}"));
        }
        res?
    };
    let bad: Vec<&str> = verified
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(rel, _)| rel.as_str())
        .collect();
    ensure!(
        bad.is_empty(),
        "file verification FAILED on disc: {}",
        bad.join(", ")
    );
    ctx.done(
        &mut stages,
        Stage::VerifyFiles,
        format!("{} files on disc match checksums.sha256", verified.len()),
    );

    // req.amend is only ever set by the TUI, so it doubles as "an operator is
    // present to take the disc"; unattended runs must not leave the tray open
    eject_if_configured(ctx, &device, req.amend);

    let mut reminders = Vec::new();
    let keep_iso = !req.discard_iso;
    if keep_iso {
        reminders.push(format!(
            "second copy: insert a fresh disc and run `ovenmitts burn-iso {}`",
            iso.display()
        ));
    } else {
        std::fs::remove_file(&iso).with_context(|| format!("remove {}", iso.display()))?;
        ctx.info(format!(
            "discarded {} after successful verification",
            iso.display()
        ));
    }
    reminders.push(format!(
        "keep {} off-disc: parity, {label}.lba.txt and checksums.sha256 are what repair a damaged disc",
        stage_dir.display()
    ));
    if payloads.iter().any(|p| p.looks_like_container()) {
        reminders.push(
            "VeraCrypt: keep an EXTERNAL volume-header backup (Tools > Backup Volume Header); \
             create a fresh container per archive generation"
                .into(),
        );
    }
    ctx.send(StageEvent::Finished {
        report: RunReport {
            iso_path: keep_iso.then_some(iso),
            iso_sha256: Some(iso_sha),
            iso_bytes,
            stages,
            reminders,
        },
    });
    Ok(())
}

/// Burn + verify an existing staged ISO (bit-identical second copy).
pub fn run_burn_iso(ctx: &RunnerCtx, iso: &Path, assume_yes: bool) -> Result<()> {
    with_failure(ctx, |stage| {
        let mut stages: Vec<(Stage, String)> = Vec::new();

        *stage = Stage::Preflight;
        ctx.start(Stage::Preflight);
        let iso_bytes = std::fs::metadata(iso)
            .with_context(|| format!("stat ISO {}", iso.display()))?
            .len();
        ensure!(iso_bytes > 0, "ISO is empty: {}", iso.display());
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
        burn::burn_iso(&ctx.tools, &device, iso, ctx.cfg.speed, &mut |pct, line| {
            ctx.progress(Stage::Burn, pct, line);
        })
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
        let disc_sha = readback_stage(ctx, &device, iso_bytes)?;
        ensure!(
            disc_sha == expected,
            "READ-BACK MISMATCH - DO NOT TRUST THIS DISC: disc sha256 {disc_sha} != ISO sha256 {expected}"
        );
        ctx.done(
            &mut stages,
            Stage::VerifyImage,
            format!("{} read back, sha256 matches ISO", human_bytes(iso_bytes)),
        );

        // burn-iso is always line mode; only an explicit config opt-in ejects
        eject_if_configured(ctx, &device, false);

        ctx.send(StageEvent::Finished {
            report: RunReport {
                iso_path: Some(iso.to_path_buf()),
                iso_sha256: Some(expected),
                iso_bytes,
                stages,
                reminders: Vec::new(),
            },
        });
        Ok(())
    })
}

/// Verify an already-burned disc.
pub fn run_verify(ctx: &RunnerCtx, iso: Option<&Path>) -> Result<()> {
    with_failure(ctx, |stage| {
        let mut stages: Vec<(Stage, String)> = Vec::new();
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
            match verify::eject(&ctx.tools, &device) {
                Ok(()) => ctx.info("ejected disc - reload defeats the page cache".into()),
                Err(e) => ctx.warn(format!(
                    "no eject/reload before read-back ({e:#}); relying on O_DIRECT"
                )),
            }
            let disc_sha = readback_stage(ctx, &device, iso_bytes)?;
            ensure!(
                disc_sha == expected,
                "READ-BACK MISMATCH - DO NOT TRUST THIS DISC: disc sha256 {disc_sha} != ISO sha256 {expected}"
            );
            ctx.done(
                &mut stages,
                Stage::VerifyImage,
                format!("{} read back, sha256 matches ISO", human_bytes(iso_bytes)),
            );
            report_sha = Some(expected);
            report_bytes = iso_bytes;
        }

        *stage = Stage::VerifyFiles;
        ctx.start(Stage::VerifyFiles);
        let mountpoint = verify::mount_ro(&ctx.tools, &device)?;
        let verified = {
            let res = (|| {
                let path = mountpoint.join("checksums.sha256");
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {} from the disc", path.display()))?;
                let entries = hashing::parse_checksums(&text)?;
                let mut th = Throttle::default();
                hashing::verify_checksums(&mountpoint, &entries, &mut |done, total| {
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
            if let Err(e) = verify::unmount(&ctx.tools, &device) {
                ctx.warn(format!("could not unmount {device}: {e:#}"));
            }
            res?
        };
        let bad: Vec<&str> = verified
            .iter()
            .filter(|(_, ok)| !ok)
            .map(|(rel, _)| rel.as_str())
            .collect();
        ensure!(
            bad.is_empty(),
            "file verification FAILED on disc: {}",
            bad.join(", ")
        );
        ctx.done(
            &mut stages,
            Stage::VerifyFiles,
            format!("{} files on disc match checksums.sha256", verified.len()),
        );

        *stage = Stage::CheckMedia;
        ctx.start(Stage::CheckMedia);
        match verify::check_media(&ctx.tools, &device, &mut |pct, line| {
            ctx.progress(Stage::CheckMedia, pct, line);
        }) {
            Ok(true) => ctx.done(
                &mut stages,
                Stage::CheckMedia,
                "MD5 tags and read check clean".into(),
            ),
            Ok(false) => bail!("xorriso -check_media reports damage or MD5 mismatch"),
            Err(e) => {
                ctx.warn(format!("check_media could not run: {e:#}"));
                ctx.done(&mut stages, Stage::CheckMedia, "skipped (no result)".into());
            }
        }

        ctx.send(StageEvent::Finished {
            report: RunReport {
                iso_path: iso.map(Path::to_path_buf),
                iso_sha256: report_sha,
                iso_bytes: report_bytes,
                stages,
                reminders: Vec::new(),
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
        let clean = verify::check_media(&ctx.tools, &device, &mut |pct, line| {
            ctx.progress(Stage::CheckMedia, pct, line);
        })?;
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
        };
        let plan = plan::build_plan(&params.plan_input(&payloads, ctx.cfg.headroom_pct), &media);
        for w in &plan.warnings {
            ctx.warn(w.clone());
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
        let listing = veracrypt_list(veracrypt);
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
fn confirm_gate(ctx: &RunnerCtx, plan: &ArchivePlan) -> Result<()> {
    ensure_fits(plan, ctx.cfg.headroom_pct)?;
    check_staging_space(ctx, plan)
}

fn ensure_fits(plan: &ArchivePlan, headroom_pct: u32) -> Result<()> {
    ensure!(
        plan.fits,
        "does not fit: total {} ({} bytes) exceeds budget {} ({} bytes; {}% headroom off {} capacity)",
        human_bytes(plan.total_bytes_est),
        plan.total_bytes_est,
        human_bytes(plan.budget),
        plan.budget,
        headroom_pct,
        human_bytes(plan.capacity)
    );
    Ok(())
}

// payloads stay in place; staging holds parity + the ISO (which contains both)
fn check_staging_space(ctx: &RunnerCtx, plan: &ArchivePlan) -> Result<()> {
    let needed = plan.parity_bytes_est + plan.total_bytes_est;
    std::fs::create_dir_all(&ctx.cfg.staging)
        .with_context(|| format!("create staging dir {}", ctx.cfg.staging.display()))?;
    let free = staging_free_bytes(&ctx.cfg.staging)?;
    ensure!(
        free >= needed,
        "staging {} has {} free but needs ~{} (parity {} + ISO {})",
        ctx.cfg.staging.display(),
        human_bytes(free),
        human_bytes(needed),
        human_bytes(plan.parity_bytes_est),
        human_bytes(plan.total_bytes_est)
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

fn veracrypt_list(bin: &Path) -> String {
    match std::process::Command::new(bin)
        .args(["--text", "--list"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => String::new(),
    }
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

fn unique_label(staging: &Path, base: &str) -> String {
    if !staging.join(base).exists() {
        return base.to_string();
    }
    let mut n = 2u64;
    loop {
        let suffix = format!("_{n}");
        let keep = 32usize.saturating_sub(suffix.len()).min(base.len());
        let candidate = format!("{}{suffix}", &base[..keep]);
        if !staging.join(&candidate).exists() {
            return candidate;
        }
        n += 1;
    }
}

fn file_name_string(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn wait_ready(ctx: &RunnerCtx, device: &str) -> Result<()> {
    verify::wait_medium_ready(&ctx.tools, device, READY_TIMEOUT, &mut |msg| ctx.warn(msg))
}

fn readback_stage(ctx: &RunnerCtx, device: &str, iso_bytes: u64) -> Result<String> {
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
        ctx.warn(
            "O_DIRECT unavailable - read-back used buffered reads; \
             cache defeat relies on the eject/reload cycle"
                .into(),
        );
    }
    Ok(rb.sha256)
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
                   head -c 8192 /dev/zero > \"$out\"\n\
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
        Tools {
            xorriso,
            par2,
            par2_version: None,
            udisksctl,
            veracrypt,
            eject: None,
            mediainfo: None,
        }
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
        loop {
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
            match run_burn(&ctx, &req) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok(()) => {}
            }
            return (device, rx.try_iter().collect(), dir);
        }
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
        (
            RunnerCtx {
                cfg,
                tools,
                tx,
                ack_rx,
            },
            rx,
            ack_tx,
        )
    }

    fn is_busy(e: &anyhow::Error) -> bool {
        format!("{e:#}").contains("Text file busy")
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
    fn unique_label_dedupes_with_numeric_suffix() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unique_label(dir.path(), "T1"), "T1");
        std::fs::create_dir(dir.path().join("T1")).unwrap();
        assert_eq!(unique_label(dir.path(), "T1"), "T1_2");
        std::fs::create_dir(dir.path().join("T1_2")).unwrap();
        assert_eq!(unique_label(dir.path(), "T1"), "T1_3");
        let base = "B".repeat(32);
        std::fs::create_dir(dir.path().join(&base)).unwrap();
        let next = unique_label(dir.path(), &base);
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
        let tools = tools_with("/bin/true".into(), None, None, None);
        let (ctx, _rx, _ack) = ctx_pair(
            cfg_with(Path::new("/dev/null"), &dir.path().join("staging")),
            tools,
        );
        let mut plan = ArchivePlan {
            payload_bytes: 0,
            parity_bytes_est: u64::MAX / 4,
            overhead_bytes_est: 0,
            total_bytes_est: u64::MAX / 4,
            capacity: 0,
            budget: 0,
            fits: true,
            warnings: vec![],
        };
        let err = check_staging_space(&ctx, &plan).unwrap_err();
        assert!(err.to_string().contains("staging"), "{err:#}");
        plan.parity_bytes_est = 0;
        plan.total_bytes_est = 0;
        assert!(check_staging_space(&ctx, &plan).is_ok());
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
        // an ETXTBSY-hit veracrypt spawn is swallowed as "not mounted", so
        // retry on any wrong outcome, bounded
        let mut last = String::new();
        for _ in 0..50 {
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
            match preflight_probe(&ctx, &[payload]) {
                Err(e) if e.to_string().contains("MOUNTED VeraCrypt container") => return,
                Err(e) => last = format!("{e:#}"),
                Ok(_) => last = "preflight succeeded".into(),
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("never refused the mounted container; last outcome: {last}");
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
        loop {
            let dir = tempfile::tempdir().unwrap();
            let sr0 = dir.path().join("sr0");
            let sr1 = dir.path().join("sr1");
            let (ctx, rx) = probe_ctx(dir.path(), &sr0, false, &[&sr0, &sr1]);
            let candidates = vec![sr0.display().to_string(), sr1.display().to_string()];
            match resolve_device_from(&ctx, move || candidates) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok((device, media)) => {
                    assert_eq!(device, sr0.display().to_string());
                    assert!(media.blank);
                    assert!(auto_select_infos(&rx).is_empty());
                    return;
                }
            }
        }
    }

    #[test]
    fn resolve_device_auto_selects_single_drive_with_media() {
        loop {
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
            match resolve_device_from(&ctx, move || candidates) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok((device, _media)) => {
                    assert_eq!(device, sr1.display().to_string());
                    let infos = auto_select_infos(&rx);
                    assert_eq!(infos.len(), 1, "one loud auto-select line: {infos:?}");
                    assert!(infos[0].contains(&sr1.display().to_string()));
                    return;
                }
            }
        }
    }

    #[test]
    fn resolve_device_error_when_explicit_and_no_medium() {
        loop {
            let dir = tempfile::tempdir().unwrap();
            let sr0 = dir.path().join("sr0");
            let sr1 = dir.path().join("sr1");
            let (ctx, rx) = probe_ctx(dir.path(), &sr0, true, &[&sr1]);
            let candidates = vec![sr0.display().to_string(), sr1.display().to_string()];
            match resolve_device_from(&ctx, move || candidates) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => {
                    assert!(e.to_string().contains("probing"), "{e:#}");
                    assert!(
                        auto_select_infos(&rx).is_empty(),
                        "an explicit device must never be swapped"
                    );
                    return;
                }
                Ok((device, _)) => panic!("explicit empty drive must not fall back to {device}"),
            }
        }
    }

    #[test]
    fn resolve_device_refuses_ambiguous_drives() {
        loop {
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
            match resolve_device_from(&ctx, move || candidates) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => {
                    let msg = format!("{e:#}");
                    assert!(msg.contains("multiple drives have media"), "{msg}");
                    assert!(msg.contains(&sr1.display().to_string()), "{msg}");
                    assert!(msg.contains(&sr2.display().to_string()), "{msg}");
                    assert!(msg.contains("--device"), "{msg}");
                    assert!(
                        e.downcast_ref::<AmbiguousDrives>().is_some(),
                        "plan needs the typed error to propagate ambiguity"
                    );
                    return;
                }
                Ok((device, _)) => panic!("ambiguity must refuse, got {device}"),
            }
        }
    }

    fn oversized_setup(dir: &Path) -> (PathBuf, PathBuf) {
        let payload = dir.join("big.hc");
        let f = std::fs::File::create(&payload).unwrap();
        f.set_len(30 * 1024 * 1024 * 1024).unwrap();
        let device = dir.join("device");
        (payload, device)
    }

    #[test]
    fn burn_bails_with_numbers_on_proceed_when_plan_does_not_fit() {
        loop {
            let dir = tempfile::tempdir().unwrap();
            let (payload, device) = oversized_setup(dir.path());
            let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
            let tools = tools_with(xorriso, None, None, None);
            let (ctx, rx, ack_tx) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
            ack_tx.send(Ack::Proceed).unwrap();
            let req = BurnRequest {
                payloads: vec![payload],
                label: None,
                parity: true,
                dry_run: false,
                assume_yes: false,
                amend: true,
                discard_iso: false,
            };
            match run_burn(&ctx, &req) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => {
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
                    return;
                }
                Ok(_) => panic!("30 GiB must not fit a BD-R 25"),
            }
        }
    }

    #[test]
    fn cli_burn_without_amend_bails_before_prompt_when_not_fitting() {
        loop {
            let dir = tempfile::tempdir().unwrap();
            let (payload, device) = oversized_setup(dir.path());
            let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
            let tools = tools_with(xorriso, None, None, None);
            let (ctx, rx, ack_tx) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
            // a wrongly-emitted NeedAck must fail as "ui channel closed", not hang
            drop(ack_tx);
            let req = BurnRequest {
                payloads: vec![payload],
                label: None,
                parity: true,
                dry_run: false,
                assume_yes: false,
                amend: false,
                discard_iso: false,
            };
            match run_burn(&ctx, &req) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => {
                    assert!(format!("{e:#}").contains("does not fit"), "{e:#}");
                    let events: Vec<StageEvent> = rx.try_iter().collect();
                    assert!(!events
                        .iter()
                        .any(|ev| matches!(ev, StageEvent::NeedAck { .. })));
                    return;
                }
                Ok(_) => panic!("30 GiB must not fit a BD-R 25"),
            }
        }
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
            }))
            .unwrap();
        let err = ctx.ask("proceed?").unwrap_err();
        assert!(err.to_string().contains("unexpected parameter amendment"));
    }

    #[test]
    fn amend_replans_and_pipeline_uses_amended_params() {
        let (events, dir) = loop {
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
            match run_burn(&ctx, &req) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok(()) => {}
            }
            break (rx.try_iter().collect::<Vec<StageEvent>>(), dir);
        };

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
        let events = loop {
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
            match run_burn(&ctx, &req) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok(()) => {}
            }
            break rx.try_iter().collect::<Vec<StageEvent>>();
        };

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
        loop {
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
            match run_burn(&ctx, &req) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => assert!(e.to_string().contains("aborted by user"), "{e:#}"),
                Ok(()) => panic!("abort must fail the run"),
            }
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
            return;
        }
    }

    #[test]
    fn run_burn_dry_run_stops_after_plan() {
        loop {
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
            match run_burn(&ctx, &req) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok(()) => {}
            }
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
            return;
        }
    }

    #[test]
    fn run_burn_aborts_on_nack() {
        loop {
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
            match run_burn(&ctx, &req) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => {
                    assert!(e.to_string().contains("aborted by user"));
                    let events: Vec<StageEvent> = rx.try_iter().collect();
                    assert!(events
                        .iter()
                        .any(|ev| matches!(ev, StageEvent::NeedAck { .. })));
                    assert!(events
                        .iter()
                        .any(|ev| matches!(ev, StageEvent::Failed { .. })));
                    return;
                }
                Ok(()) => panic!("abort must fail the run"),
            }
        }
    }

    #[test]
    fn run_burn_full_pipeline_with_fakes() {
        let (events, report, dir) = loop {
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
            match run_burn(&ctx, &req) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok(()) => {}
            }
            let events: Vec<StageEvent> = rx.try_iter().collect();
            let Some(StageEvent::Finished { report }) = events.last() else {
                panic!("expected Finished, got {:?}", events.last());
            };
            break (events.clone(), report.clone(), dir);
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
        assert_eq!(report.iso_bytes, 8192);

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

    #[test]
    fn run_burn_iso_verifies_against_sidecar() {
        loop {
            let dir = tempfile::tempdir().unwrap();
            let iso = dir.path().join("copy.iso");
            std::fs::write(&iso, vec![9u8; 8192]).unwrap();
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
            match run_burn_iso(&ctx, &iso, true) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok(()) => {}
            }
            let events: Vec<StageEvent> = rx.try_iter().collect();
            let Some(StageEvent::Finished { report }) = events.last() else {
                panic!("expected Finished, got {:?}", events.last());
            };
            assert_eq!(report.iso_sha256.as_deref(), Some(sha.as_str()));
            assert!(events
                .iter()
                .any(|ev| matches!(ev, StageEvent::Info(t) if t.contains("recorded sha256"))));
            return;
        }
    }

    #[test]
    fn run_burn_iso_flags_readback_mismatch() {
        loop {
            let dir = tempfile::tempdir().unwrap();
            let iso = dir.path().join("copy.iso");
            std::fs::write(&iso, vec![9u8; 8192]).unwrap();
            hashing::write_checksums(
                &[("ab".repeat(32), "copy.iso".into())],
                &dir.path().join("copy.iso.sha256"),
            )
            .unwrap();
            let device = dir.path().join("device");
            let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
            let tools = tools_with(xorriso, None, None, None);
            let (ctx, _rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
            match run_burn_iso(&ctx, &iso, true) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => {
                    assert!(e.to_string().contains("DO NOT TRUST"), "{e:#}");
                    return;
                }
                Ok(()) => panic!("mismatching sidecar must fail verification"),
            }
        }
    }

    #[test]
    fn run_check_reports_clean_and_damaged() {
        for (fixture, want_clean) in [(CHECK_CLEAN_FIXTURE, true), (CHECK_DAMAGED_FIXTURE, false)] {
            loop {
                let dir = tempfile::tempdir().unwrap();
                let device = dir.path().join("device");
                std::fs::write(&device, vec![0u8; 4096]).unwrap();
                let xorriso =
                    fake_xorriso_probing(dir.path(), &device, PROBE_WRITTEN_FIXTURE, fixture);
                let tools = tools_with(xorriso, None, None, None);
                let (ctx, rx, _ack) =
                    ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
                let res = run_check(&ctx, None);
                match (&res, want_clean) {
                    (Err(e), _) if is_busy(e) => continue,
                    (Ok(()), true) => {
                        let events: Vec<StageEvent> = rx.try_iter().collect();
                        assert!(matches!(events.last(), Some(StageEvent::Finished { .. })));
                    }
                    (Err(e), false) => assert!(e.to_string().contains("DAMAGED"), "{e:#}"),
                    (r, _) => panic!("unexpected result {r:?} for clean={want_clean}"),
                }
                break;
            }
        }
    }

    #[test]
    fn run_info_save_persists_resolved_device() {
        loop {
            let dir = tempfile::tempdir().unwrap();
            let device = dir.path().join("device");
            let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
            let tools = tools_with(xorriso, None, None, None);
            let (ctx, rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
            let cfg_path = dir.path().join("cfg").join("config.toml");
            match run_info(&ctx, Some(&cfg_path)) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok(()) => {}
            }
            let text = std::fs::read_to_string(&cfg_path).unwrap();
            assert!(text.contains(&device.display().to_string()));
            let events: Vec<StageEvent> = rx.try_iter().collect();
            assert!(events
                .iter()
                .any(|ev| matches!(ev, StageEvent::Info(t) if t.contains("saved device"))));
            return;
        }
    }

    #[test]
    fn run_check_save_persists_resolved_device() {
        loop {
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
            match run_check(&ctx, Some(&cfg_path)) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok(()) => {}
            }
            let text = std::fs::read_to_string(&cfg_path).unwrap();
            assert!(text.contains(&device.display().to_string()));
            return;
        }
    }

    #[test]
    fn run_check_refuses_blank_medium() {
        loop {
            let dir = tempfile::tempdir().unwrap();
            let device = dir.path().join("device");
            std::fs::write(&device, vec![0u8; 4096]).unwrap();
            // default probe fixture reports a blank BD-R
            let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
            let tools = tools_with(xorriso, None, None, None);
            let (ctx, _rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
            match run_check(&ctx, None) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => {
                    assert!(e.to_string().contains("blank"), "{e:#}");
                    return;
                }
                Ok(()) => panic!("check on a blank medium must refuse, not report clean"),
            }
        }
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
        loop {
            let dir = tempfile::tempdir().unwrap();
            let device = dir.path().join("device");
            let xorriso = fake_xorriso(dir.path(), &device, CHECK_CLEAN_FIXTURE);
            let tools = tools_with(xorriso, None, None, None);
            let (ctx, rx, _ack) = ctx_pair(cfg_with(&device, &dir.path().join("staging")), tools);
            match run_info(&ctx, None) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok(()) => {}
            }
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
            return;
        }
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
        loop {
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
            match run_plan(&ctx, &[payload], None) {
                Err(e) if is_busy(&e) => continue,
                Err(e) => panic!("{e:#}"),
                Ok(()) => {}
            }
            let events: Vec<StageEvent> = rx.try_iter().collect();
            assert!(events.iter().any(
                |ev| matches!(ev, StageEvent::Info(t) if t.contains("assuming a blank BD-R 25"))
            ));
            assert!(events
                .iter()
                .any(|ev| matches!(ev, StageEvent::Plan { .. })));
            return;
        }
    }
}
