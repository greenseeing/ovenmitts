use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;

use clap::Parser;

use ovenmitts::cli::{Cli, Command};
use ovenmitts::config::Config;
use ovenmitts::plan::{human_bytes, ArchivePlan, MediaInfo};
use ovenmitts::runner::{self, Ack, RunReport, RunnerCtx, StageEvent};
use ovenmitts::tools::Tools;

fn main() -> ExitCode {
    // A panic anywhere (e.g. a TUI render on the main thread) must not leave a
    // burning xorriso running: terminate registered tools before the default
    // hook prints and unwinds. Worker-thread panics are already covered by the
    // per-child reaper guards; this covers a main-thread panic.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ovenmitts::proc::terminate_active(false);
        prev(info);
    }));

    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if e.downcast_ref::<AlreadyReported>().is_none() {
                eprintln!("error: {e:#}");
            }
            ExitCode::FAILURE
        }
    }
}

/// drive() already rendered this failure as an `error:` line; main must
/// skip the duplicate top-level line but still exit nonzero.
#[derive(Debug, Clone, Copy)]
struct AlreadyReported;

impl std::fmt::Display for AlreadyReported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("failure already reported")
    }
}

fn dispatch(cli: Cli) -> anyhow::Result<()> {
    // the updater must run even when the config is broken or xorriso is
    // missing — it is how both get fixed
    if matches!(cli.command, Some(Command::Update)) {
        return ovenmitts::update::run();
    }
    let file_cfg = ovenmitts::config::load(cli.config.as_deref())?;
    let mut cfg = ovenmitts::config::Config::resolve(file_cfg)?;
    if let Some(dev) = &cli.device {
        ovenmitts::config::validate_device(dev)?;
        cfg.device = dev.clone();
        cfg.device_explicit = true;
    }
    if let Some(staging) = &cli.staging {
        cfg.staging = ovenmitts::config::expand_tilde(staging);
    }

    match cli.command {
        None => {
            let interactive = !cli.no_tui && std::io::stdout().is_terminal();
            anyhow::ensure!(
                interactive || !cli.payloads.is_empty(),
                "nothing to do: pass payload files (TUI) or a subcommand; see --help"
            );
            let tools = ovenmitts::tools::discover()?;
            let payloads = if cli.payloads.is_empty() {
                match ovenmitts::picker::pick_payloads(&cfg, &tools, std::env::current_dir()?)? {
                    Some(paths) => paths,
                    None => {
                        eprintln!("nothing selected");
                        return Ok(());
                    }
                }
            } else {
                cli.payloads
            };
            let req = ovenmitts::runner::BurnRequest {
                payloads,
                label: None,
                parity: true,
                dry_run: false,
                assume_yes: false,
                amend: false,
                discard_iso: !cfg.keep_iso,
            };
            if interactive {
                ovenmitts::tui::run(cfg, tools, req)
            } else {
                run_cli_burn(cfg, tools, req)
            }
        }
        Some(cmd) => {
            let config_path = cli
                .config
                .clone()
                .unwrap_or_else(ovenmitts::config::default_path);
            run_command(cfg, cmd, config_path)
        }
    }
}

fn run_command(mut cfg: Config, cmd: Command, config_path: PathBuf) -> anyhow::Result<()> {
    // plan must work on a machine without xorriso: probe failure already
    // falls back to synthetic media inside run_plan
    let tools = match &cmd {
        Command::Plan { .. } => {
            ovenmitts::tools::discover().unwrap_or_else(|_| ovenmitts::tools::lenient())
        }
        _ => ovenmitts::tools::discover()?,
    };
    match cmd {
        Command::Burn {
            payloads,
            label,
            speed,
            redundancy,
            defect_management,
            no_parity,
            dry_run,
            yes,
            discard_iso,
        } => {
            if let Some(s) = speed {
                cfg.speed = Some(s);
            }
            if let Some(r) = redundancy {
                cfg.redundancy_pct = r;
            }
            if defect_management {
                cfg.defect_management = true;
            }
            let req = runner::BurnRequest {
                payloads,
                label,
                parity: !no_parity,
                dry_run,
                assume_yes: yes,
                amend: false,
                // the documented contract: keep_iso = false in the config
                // behaves like --discard-iso; the flag can only tighten it
                discard_iso: discard_iso || !cfg.keep_iso,
            };
            run_cli_burn(cfg, tools, req)
        }
        Command::BurnIso { iso, speed, yes } => {
            if let Some(s) = speed {
                cfg.speed = Some(s);
            }
            drive(cfg, tools, move |ctx| runner::run_burn_iso(ctx, &iso, yes))
        }
        Command::Verify { iso } => drive(cfg, tools, move |ctx| {
            runner::run_verify(ctx, iso.as_deref())
        }),
        Command::Update => unreachable!("handled in dispatch before config load"),
        Command::Check { save } => {
            let save_to = save.then_some(config_path);
            drive(cfg, tools, move |ctx| {
                runner::run_check(ctx, save_to.as_deref())
            })
        }
        Command::Info { save } => {
            let save_to = save.then_some(config_path);
            drive(cfg, tools, move |ctx| {
                runner::run_info(ctx, save_to.as_deref())
            })
        }
        Command::Plan {
            payloads,
            media,
            redundancy,
        } => {
            if let Some(r) = redundancy {
                cfg.redundancy_pct = r;
            }
            drive(cfg, tools, move |ctx| {
                runner::run_plan(ctx, &payloads, media.as_deref())
            })
        }
    }
}

fn run_cli_burn(
    cfg: ovenmitts::config::Config,
    tools: ovenmitts::tools::Tools,
    req: ovenmitts::runner::BurnRequest,
) -> anyhow::Result<()> {
    drive(cfg, tools, move |ctx| runner::run_burn(ctx, &req))
}

/// Spawn the runner on a worker thread and print StageEvents as lines.
fn drive<F>(cfg: Config, tools: Tools, run: F) -> anyhow::Result<()>
where
    F: FnOnce(&RunnerCtx) -> anyhow::Result<()> + Send + 'static,
{
    let headroom_pct = cfg.headroom_pct;
    let (tx, rx) = mpsc::channel();
    let (ack_tx, ack_rx) = mpsc::channel();
    let ctx = RunnerCtx::new(cfg, tools, tx, ack_rx);
    let worker = std::thread::spawn(move || run(&ctx));
    let stop = ovenmitts::shutdown::install();

    let mut line = LinePrinter::default();
    let mut failed_rendered = false;
    loop {
        let ev = match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(ev) => ev,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if ovenmitts::shutdown::stopping(&stop) {
                    line.close();
                    return Err(interrupted(worker, &ack_tx));
                }
                continue;
            }
        };
        match ev {
            StageEvent::Plan {
                device,
                media,
                plan,
                params,
            } => {
                line.close();
                println!("[plan] device: {device}");
                print_plan(&media, &plan, headroom_pct, params.redundancy_pct);
            }
            StageEvent::StageStart(stage) => {
                line.close();
                println!("[{}] start", stage.label());
            }
            StageEvent::Progress { stage, pct, detail } => {
                let text = match pct {
                    Some(p) => format!("[{}] {:5.1}% {}", stage.label(), p, detail),
                    None => format!("[{}] {}", stage.label(), detail),
                };
                line.progress(&text);
            }
            StageEvent::StageDone { stage, summary } => {
                line.close();
                println!("[{}] done — {}", stage.label(), summary);
            }
            StageEvent::Info(text) => {
                line.close();
                println!("info: {text}");
            }
            StageEvent::DiscStart {
                index,
                total,
                label,
                parity,
            } => {
                line.close();
                println!(
                    "=== disc {index} of {total} - {label}{} ===",
                    if parity { " (parity)" } else { "" }
                );
            }
            StageEvent::Out(text) => {
                line.close();
                println!("{text}");
            }
            StageEvent::Warn(text) => {
                line.close();
                eprintln!("warning: {text}");
            }
            StageEvent::NeedAck { prompt } => {
                line.close();
                print!("{prompt} [Y/n] ");
                let _ = std::io::stdout().flush();
                // The answer is read on its own thread: a blocking stdin read
                // here would stop the signal polling (SA_RESTART restarts the
                // read), so a SIGTERM at a prompt would hang forever.
                let (answer_tx, answer_rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let mut answer = String::new();
                    // EOF (closed/redirected stdin) must NOT read as consent —
                    // an unattended burn needs an explicit --yes
                    let read = std::io::stdin().lock().read_line(&mut answer);
                    let _ = answer_tx.send(match read {
                        Ok(0) | Err(_) => None,
                        Ok(_) => Some(answer),
                    });
                });
                let ack = loop {
                    match answer_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                        Ok(Some(answer)) => break ack_from(&answer),
                        Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                            eprintln!(
                                "warning: no interactive stdin — aborting (use --yes for unattended runs)"
                            );
                            break Ack::Abort;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if ovenmitts::shutdown::stopping(&stop) {
                                return Err(interrupted(worker, &ack_tx));
                            }
                        }
                    }
                };
                let _ = ack_tx.send(ack);
            }
            StageEvent::Finished { report } => {
                line.close();
                print_report(&report);
            }
            StageEvent::Failed { stage, error } => {
                line.close();
                eprintln!("error: [{}] {error}", stage.label());
                failed_rendered = true;
            }
        }
    }
    match worker.join() {
        Ok(Err(e)) if failed_rendered => Err(e.context(AlreadyReported)),
        Ok(res) => res,
        Err(_) => anyhow::bail!("pipeline thread panicked"),
    }
}

/// Shared signal exit: announce, terminate running tools, join the worker,
/// and produce the (already-reported) error. Consumes the worker handle —
/// callers return immediately.
fn interrupted(
    worker: std::thread::JoinHandle<anyhow::Result<()>>,
    ack_tx: &mpsc::Sender<Ack>,
) -> anyhow::Error {
    eprintln!(
        "\nsignal received - terminating running tools; the disc in the \
         drive may be partially written and must not be trusted without \
         a verify run"
    );
    ovenmitts::shutdown::escalate(|| worker.is_finished(), ack_tx);
    let _ = worker.join();
    anyhow::anyhow!("interrupted by signal").context(AlreadyReported)
}

fn ack_from(answer: &str) -> Ack {
    let a = answer.trim();
    if a.is_empty() || a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes") {
        Ack::Proceed
    } else {
        Ack::Abort
    }
}

// One \r-overwritten progress line at a time; pad to clear leftovers.
#[derive(Default)]
struct LinePrinter {
    open_width: usize,
}

impl LinePrinter {
    fn progress(&mut self, text: &str) {
        let width = text.chars().count();
        print!(
            "\r{text}{}",
            " ".repeat(self.open_width.saturating_sub(width))
        );
        let _ = std::io::stdout().flush();
        self.open_width = self.open_width.max(width);
    }

    fn close(&mut self) {
        if self.open_width > 0 {
            println!();
            self.open_width = 0;
        }
    }
}

fn print_plan(media: &MediaInfo, plan: &ArchivePlan, headroom_pct: u32, redundancy_pct: u32) {
    let mut media_line = format!(
        "[plan] media: {} — {} free",
        media.kind.label(),
        human_bytes(media.free_bytes)
    );
    if let Some(id) = &media.media_id {
        media_line.push_str(&format!(" ({id})"));
    }
    println!("{media_line}");
    println!(
        "[plan] payload {} + parity ~{} ({redundancy_pct}% redundancy) + overhead {}",
        human_bytes(plan.payload_bytes),
        human_bytes(plan.parity_bytes_est),
        human_bytes(plan.overhead_bytes_est)
    );
    let verdict = if plan.fits {
        "fits".to_string()
    } else if let Some(span) = &plan.span {
        format!("spans {} discs", span.discs.len())
    } else {
        "DOES NOT FIT".to_string()
    };
    println!(
        "[plan] total {} of {} budget ({headroom_pct}% headroom off {} capacity) — {verdict}",
        human_bytes(plan.total_bytes_est),
        human_bytes(plan.budget),
        human_bytes(plan.capacity),
    );
}

fn print_report(report: &RunReport) {
    let empty = report.stages.is_empty()
        && report.iso_path.is_none()
        && report.iso_sha256.is_none()
        && report.reminders.is_empty()
        && report.written_files.is_empty()
        && report.degradations.is_empty();
    if empty {
        return;
    }
    println!();
    for (stage, summary) in &report.stages {
        println!("  {:<13} {}", stage.label(), summary);
    }
    if let Some(p) = &report.iso_path {
        println!("  iso: {}", p.display());
    }
    if let Some(h) = &report.iso_sha256 {
        println!("  iso sha256: {h}");
    }
    if report.iso_bytes > 0 {
        println!(
            "  iso size: {} ({} bytes)",
            human_bytes(report.iso_bytes),
            report.iso_bytes
        );
    }
    for f in &report.written_files {
        println!("  wrote: {}", f.display());
    }
    for c in &report.degradations {
        println!("  caveat: {c}");
    }
    for r in &report.reminders {
        println!("  reminder: {r}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn already_reported_downcasts_through_context() {
        let e = anyhow::anyhow!("boom").context(AlreadyReported);
        assert!(e.downcast_ref::<AlreadyReported>().is_some());
        assert!(anyhow::anyhow!("boom")
            .downcast_ref::<AlreadyReported>()
            .is_none());
    }

    #[test]
    fn ack_parsing_accepts_yes_variants() {
        assert_eq!(ack_from(""), Ack::Proceed);
        assert_eq!(ack_from("\n"), Ack::Proceed);
        assert_eq!(ack_from("y\n"), Ack::Proceed);
        assert_eq!(ack_from("Y\n"), Ack::Proceed);
        assert_eq!(ack_from("yes\n"), Ack::Proceed);
        assert_eq!(ack_from("n\n"), Ack::Abort);
        assert_eq!(ack_from("anything else\n"), Ack::Abort);
    }
}
