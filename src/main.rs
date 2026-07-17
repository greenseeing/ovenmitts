use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::mpsc;

use clap::Parser;

use ovenmitts::cli::{Cli, Command};
use ovenmitts::config::Config;
use ovenmitts::plan::{human_bytes, ArchivePlan, MediaInfo};
use ovenmitts::runner::{self, Ack, RunReport, RunnerCtx, StageEvent};
use ovenmitts::tools::Tools;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let file_cfg = ovenmitts::config::load(cli.config.as_deref())?;
    let mut cfg = ovenmitts::config::Config::resolve(file_cfg)?;
    if let Some(dev) = &cli.device {
        cfg.device = dev.clone();
        cfg.device_explicit = true;
    }

    match cli.command {
        None => {
            anyhow::ensure!(
                !cli.payloads.is_empty(),
                "nothing to do: pass payload files (TUI) or a subcommand; see --help"
            );
            let tools = ovenmitts::tools::discover()?;
            let req = ovenmitts::runner::BurnRequest {
                payloads: cli.payloads,
                label: None,
                parity: true,
                dry_run: false,
                assume_yes: false,
                amend: false,
                discard_iso: false,
            };
            if cli.no_tui || !std::io::stdout().is_terminal() {
                run_cli_burn(cfg, tools, req)
            } else {
                ovenmitts::tui::run(cfg, tools, req)
            }
        }
        Some(cmd) => run_command(cfg, cmd),
    }
}

fn run_command(mut cfg: Config, cmd: Command) -> anyhow::Result<()> {
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
            staging,
            defect_management,
            no_parity,
            dry_run,
            yes,
            discard_iso,
        } => {
            if let Some(s) = staging {
                cfg.staging = s;
            }
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
                discard_iso,
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
        Command::Check => drive(cfg, tools, runner::run_check),
        Command::Info => drive(cfg, tools, runner::run_info),
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
    let ctx = RunnerCtx {
        cfg,
        tools,
        tx,
        ack_rx,
    };
    let worker = std::thread::spawn(move || run(&ctx));

    let mut line = LinePrinter::default();
    for ev in rx {
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
                let mut answer = String::new();
                // EOF (closed/redirected stdin) must NOT read as consent —
                // an unattended burn needs an explicit --yes
                let ack = match std::io::stdin().lock().read_line(&mut answer) {
                    Ok(0) | Err(_) => {
                        eprintln!(
                            "no interactive stdin — aborting (use --yes for unattended runs)"
                        );
                        Ack::Abort
                    }
                    Ok(_) => ack_from(&answer),
                };
                let _ = ack_tx.send(ack);
            }
            StageEvent::Finished { report } => {
                line.close();
                print_report(&report);
            }
            StageEvent::Failed { stage, error } => {
                line.close();
                eprintln!("[{}] FAILED: {error}", stage.label());
            }
        }
    }
    match worker.join() {
        Ok(res) => res,
        Err(_) => anyhow::bail!("pipeline thread panicked"),
    }
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
    println!(
        "[plan] total {} of {} budget ({headroom_pct}% headroom off {} capacity) — {}",
        human_bytes(plan.total_bytes_est),
        human_bytes(plan.budget),
        human_bytes(plan.capacity),
        if plan.fits { "fits" } else { "DOES NOT FIT" }
    );
}

fn print_report(report: &RunReport) {
    let empty = report.stages.is_empty()
        && report.iso_path.is_none()
        && report.iso_sha256.is_none()
        && report.reminders.is_empty();
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
    for r in &report.reminders {
        println!("  reminder: {r}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
