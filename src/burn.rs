use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::tools::Tools;

/// Optional drive-level defect management: `xorriso -outdev <dev> -format as_needed`.
/// Caller MUST re-probe media afterwards - formatted capacity shrinks
/// (observed: 768 MiB loss on BD-R 25).
pub fn format_defect_management(
    tools: &Tools,
    device: &str,
    stall: Duration,
    cb: &mut dyn FnMut(Option<f32>, String),
) -> Result<()> {
    let args: Vec<String> = vec![
        "-outdev".into(),
        device.into(),
        "-format".into(),
        "as_needed".into(),
    ];
    run_streaming(&tools.xorriso, &args, stall, &mut |line| {
        forward(line, 0, cb)
    })
}

/// Burn: `xorriso -as cdrecord -v dev=<dev> [speed=<n>] fs=64m blank=as_needed -eject <iso>`.
/// Unformatted BD-R = stream recording at full speed (research decision #3).
/// The full xorriso transcript tees to `burn_log_path(iso)`, best effort.
pub fn burn_iso(
    tools: &Tools,
    device: &str,
    iso: &Path,
    speed: Option<u32>,
    stall: Duration,
    cb: &mut dyn FnMut(Option<f32>, String),
) -> Result<()> {
    let total_bytes = std::fs::metadata(iso)
        .with_context(|| format!("stat ISO {}", iso.display()))?
        .len();
    let args = burn_args(device, iso, speed);
    let mut log = std::fs::File::create(burn_log_path(iso)).ok();
    run_streaming(&tools.xorriso, &args, stall, &mut |line| {
        if let Some(f) = log.as_mut() {
            let _ = writeln!(f, "{line}");
        }
        forward(line, total_bytes, cb)
    })
}

/// Where the burn transcript lands: next to the ISO, `<stem>.burn.log`.
pub fn burn_log_path(iso: &Path) -> PathBuf {
    iso.with_extension("burn.log")
}

fn forward(line: &str, total_bytes: u64, cb: &mut dyn FnMut(Option<f32>, String)) {
    match parse_progress_line(line, total_bytes) {
        Some((pct, detail)) => cb(pct, detail),
        None => {
            let t = line.trim();
            if !t.is_empty() {
                cb(None, t.to_string());
            }
        }
    }
}

/// Pure: build the burn argv (testable without xorriso).
pub fn burn_args(device: &str, iso: &Path, speed: Option<u32>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-as".into(),
        "cdrecord".into(),
        "-v".into(),
        format!("dev={device}"),
    ];
    if let Some(s) = speed {
        args.push(format!("speed={s}"));
    }
    args.push("fs=64m".into());
    args.push("blank=as_needed".into());
    args.push("-eject".into());
    args.push(iso.display().to_string());
    args
}

/// Pure: parse xorriso -as cdrecord progress lines to (pct, detail).
/// Handles both "xorriso : UPDATE : ..." status lines and byte-count lines.
/// Needs the total image size to derive a percentage.
pub fn parse_progress_line(line: &str, total_bytes: u64) -> Option<(Option<f32>, String)> {
    let rest = line.trim_start().strip_prefix("xorriso : UPDATE :")?.trim();
    if rest.is_empty() {
        return None;
    }
    let toks: Vec<&str> = rest.split_whitespace().collect();
    // "Writing:    20208s   38.2%   fifo  52%  buf  99%  4.0xD"
    if toks.len() >= 3 && toks[0] == "Writing:" {
        if let Some(pct) = pct_token(toks[2]) {
            return Some((Some(pct), rest.to_string()));
        }
    }
    // " 512 of 3665 MB written (fifo 97%) [buf  94%]   2.4x."
    if toks.len() >= 5 && toks[1] == "of" && toks[3] == "MB" && toks[4] == "written" {
        if let (Ok(done), Ok(total)) = (toks[0].parse::<u64>(), toks[2].parse::<u64>()) {
            let pct = if total > 0 {
                Some(((done as f64 / total as f64 * 100.0) as f32).clamp(0.0, 100.0))
            } else if total_bytes > 0 {
                let done_bytes = done as f64 * 1024.0 * 1024.0;
                Some(((done_bytes / total_bytes as f64 * 100.0) as f32).clamp(0.0, 100.0))
            } else {
                None
            };
            return Some((pct, rest.to_string()));
        }
    }
    // "Formatting  ( 99.0% done in 912 seconds )" / "Blanking  ( 1.0% done ... )"
    if toks.len() >= 4
        && matches!(toks[0], "Formatting" | "Blanking")
        && toks[1] == "("
        && toks[3] == "done"
    {
        if let Some(pct) = pct_token(toks[2]) {
            return Some((Some(pct), rest.to_string()));
        }
    }
    // other UPDATE lines are status keepalives ("Thank you for being
    // patient...", "Closing track/session...") - forward without a percent
    Some((None, rest.to_string()))
}

fn pct_token(t: &str) -> Option<f32> {
    let pct: f32 = t.strip_suffix('%')?.parse().ok()?;
    Some(pct.clamp(0.0, 100.0))
}

const STDERR_TAIL: usize = 12;

// xorriso keeps emitting UPDATE keepalives to stderr while aborting; without
// severity filtering they bury (or evict) the FATAL/FAILURE cause.
fn is_diagnostic(line: &str) -> bool {
    [
        " FATAL : ",
        " FAILURE : ",
        " SORRY : ",
        " ABORT : ",
        " : aborting :",
    ]
    .iter()
    .any(|needle| line.contains(needle))
}

fn push_capped(buf: &mut VecDeque<String>, line: String) {
    if buf.len() == STDERR_TAIL {
        buf.pop_front();
    }
    buf.push_back(line);
}

// Poll cadence for the inactivity watchdog, and how long a tool may be silent
// before we surface a reassuring "still working" note (not a kill).
const WATCH_POLL: Duration = Duration::from_secs(5);
const WARN_AFTER: Duration = Duration::from_secs(120);

// libburn rewrites progress in place with '\r'; split on both terminators.
// `stall` kills the child after that long with no output at all (Duration::ZERO
// disables). A healthy xorriso/par2 emits keepalives every second, so only a
// genuinely wedged drive stays silent that long.
pub(crate) fn run_streaming(
    bin: &Path,
    args: &[String],
    stall: Duration,
    on_line: &mut dyn FnMut(&str),
) -> Result<()> {
    let mut reaper = crate::proc::Reaper::spawn(bin, args)?;
    let stdout = reaper.stdout().context("no stdout pipe")?;
    let stderr = reaper.stderr().context("no stderr pipe")?;
    let (tx, rx) = mpsc::channel::<(bool, String)>();
    let tx_err = tx.clone();
    let t_out = std::thread::spawn(move || pump(stdout, false, tx));
    let t_err = std::thread::spawn(move || pump(stderr, true, tx_err));

    let mut tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL);
    let mut diags: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL);
    let mut last_activity = Instant::now();
    let mut last_warn = Instant::now();
    let mut stalled = false;
    loop {
        match rx.recv_timeout(WATCH_POLL) {
            Ok((is_err, line)) => {
                last_activity = Instant::now();
                if is_err {
                    if is_diagnostic(&line) {
                        push_capped(&mut diags, line.clone());
                    }
                    push_capped(&mut tail, line.clone());
                }
                on_line(&line);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let quiet = last_activity.elapsed();
                if quiet >= WARN_AFTER && last_warn.elapsed() >= Duration::from_secs(60) {
                    on_line(&format!("(no tool output for {}s)", quiet.as_secs()));
                    last_warn = Instant::now();
                }
                if !stall.is_zero() && quiet >= stall {
                    reaper.kill_now();
                    stalled = true;
                    break;
                }
            }
        }
    }
    let _ = t_out.join();
    let _ = t_err.join();
    if stalled {
        bail!(
            "{}: no output for {}s - terminated (wedged drive?)",
            bin.display(),
            stall.as_secs()
        );
    }
    let status = reaper.wait()?;
    if !status.success() {
        let lines: Vec<String> = if diags.is_empty() { tail } else { diags }.into();
        bail!("{} failed ({status}): {}", bin.display(), lines.join("\n"));
    }
    Ok(())
}

fn pump(mut r: impl Read, is_err: bool, tx: mpsc::Sender<(bool, String)>) {
    let mut buf = [0u8; 8192];
    let mut acc: Vec<u8> = Vec::new();
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                if !acc.is_empty() {
                    let _ = tx.send((is_err, String::from_utf8_lossy(&acc).into_owned()));
                    acc.clear();
                }
            } else {
                acc.push(b);
            }
        }
    }
    if !acc.is_empty() {
        let _ = tx.send((is_err, String::from_utf8_lossy(&acc).into_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Real captures: bug-xorriso list 2015-08 msg00002 (DVD+RW burn via
    // -as cdrecord) and xorriso doc/qemu_xorriso.wiki (Writing:/Blanking/
    // Formatting UPDATE lines).
    const BURN_LOG: &str = include_str!("../tests/fixtures/xorriso_cdrecord_burn.txt");
    const FORMAT_LOG: &str = include_str!("../tests/fixtures/xorriso_format_progress.txt");

    #[test]
    fn burn_args_match_design_contract() {
        assert_eq!(
            burn_args("/dev/sr0", Path::new("/stage/v.iso"), Some(4)),
            vec![
                "-as",
                "cdrecord",
                "-v",
                "dev=/dev/sr0",
                "speed=4",
                "fs=64m",
                "blank=as_needed",
                "-eject",
                "/stage/v.iso",
            ]
        );
    }

    #[test]
    fn burn_args_omit_speed_for_drive_default() {
        let args = burn_args("/dev/sr1", Path::new("x.iso"), None);
        assert!(!args.iter().any(|a| a.starts_with("speed=")));
        assert_eq!(args[3], "dev=/dev/sr1");
        assert_eq!(args.last().map(String::as_str), Some("x.iso"));
    }

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 0.01, "{a} vs {b}");
    }

    #[test]
    fn mb_written_lines_from_capture() {
        let total = 3665u64 * 1024 * 1024;
        let pcts: Vec<f32> = BURN_LOG
            .lines()
            .filter(|l| l.contains("MB written"))
            .map(|l| parse_progress_line(l, total).unwrap().0.unwrap())
            .collect();
        assert_eq!(pcts.len(), 4);
        approx(pcts[0], 512.0 / 3665.0 * 100.0);
        approx(pcts[3], 522.0 / 3665.0 * 100.0);
    }

    #[test]
    fn writing_sector_lines_from_capture() {
        let pcts: Vec<f32> = BURN_LOG
            .lines()
            .filter(|l| l.contains("Writing:"))
            .filter_map(|l| parse_progress_line(l, 0).and_then(|(p, _)| p))
            .collect();
        assert_eq!(pcts, vec![38.2, 100.0]);
    }

    #[test]
    fn keepalive_updates_have_no_percent() {
        for needle in [
            "Thank you for being patient",
            "Closing track/session",
            "Formatting. Working",
        ] {
            let line = BURN_LOG.lines().find(|l| l.contains(needle)).unwrap();
            let (pct, detail) = parse_progress_line(line, 0).unwrap();
            assert_eq!(pct, None, "{line}");
            assert!(detail.contains(needle));
            assert!(!detail.starts_with("xorriso"));
        }
    }

    #[test]
    fn non_update_lines_are_unknown() {
        for line in [
            "Beginning to write data track.",
            "Writing to '/dev/sr0' completed successfully.",
            "Media summary: 0 sessions, 0 data blocks, 0 data, 4488m free",
            "",
        ] {
            assert_eq!(parse_progress_line(line, 0), None, "{line}");
        }
    }

    #[test]
    fn format_and_blank_lines_from_capture() {
        let pcts: Vec<f32> = FORMAT_LOG
            .lines()
            .filter_map(|l| parse_progress_line(l, 0).and_then(|(p, _)| p))
            .collect();
        assert_eq!(pcts, vec![1.0, 95.4, 99.0, 99.0]);
        assert_eq!(parse_progress_line("Blanking done", 0), None);
        assert_eq!(parse_progress_line("Formatting done", 0), None);
    }

    #[test]
    fn mb_written_falls_back_to_total_bytes() {
        let line = "xorriso : UPDATE :  512 of 0 MB written (fifo 97%) [buf  94%]   2.4x.";
        let (pct, _) = parse_progress_line(line, 1024 * 1024 * 1024).unwrap();
        approx(pct.unwrap(), 50.0);
        assert_eq!(parse_progress_line(line, 0).unwrap().0, None);
    }

    fn fake_tools(script: &str, dir: &Path) -> Tools {
        use std::os::unix::fs::PermissionsExt;
        let fake = dir.join("xorriso");
        std::fs::write(&fake, script).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        Tools {
            xorriso: fake,
            par2: None,
            par2_version: None,
            udisksctl: None,
            veracrypt: None,
            eject: None,
            mediainfo: None,
        }
    }

    #[test]
    fn burn_iso_streams_both_pipes_and_passes_argv() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("v.iso");
        std::fs::write(&iso, vec![0u8; 2048]).unwrap();
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > {argv}\n\
             printf 'Beginning to write data track.\\n'\n\
             printf 'xorriso : UPDATE :  512 of 3665 MB written (fifo 97%%) [buf  94%%]   2.4x.\\r' >&2\n\
             printf 'xorriso : UPDATE : Writing:    52885s  100.0%%   fifo   0%%  buf  99%%  0.0xD\\n' >&2\n\
             printf \"Writing to '/dev/sr0' completed successfully.\\n\"\n",
            argv = dir.path().join("argv.txt").display(),
        );
        let tools = fake_tools(&script, dir.path());
        let mut events = Vec::new();
        burn_iso(
            &tools,
            "/dev/sr0",
            &iso,
            Some(4),
            Duration::ZERO,
            &mut |p, l| events.push((p, l)),
        )
        .unwrap();

        let argv = std::fs::read_to_string(dir.path().join("argv.txt")).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            burn_args("/dev/sr0", &iso, Some(4))
        );
        assert!(events
            .iter()
            .any(|(p, _)| p.is_some_and(|v| (v - 13.97).abs() < 0.01)));
        assert!(events.iter().any(|(p, _)| *p == Some(100.0)));
        assert!(events
            .iter()
            .any(|(p, l)| p.is_none() && l.contains("completed successfully")));
        let log = std::fs::read_to_string(burn_log_path(&iso)).unwrap();
        assert!(log.contains("completed successfully"), "{log}");
    }

    #[test]
    fn burn_iso_failure_leads_with_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("v.iso");
        std::fs::write(&iso, b"x").unwrap();
        // real Verbatim-43887 failure shape: keepalives precede the diagnostics
        let script = "#!/bin/sh\n\
                      i=267\n\
                      while [ $i -le 273 ]; do\n\
                        echo \"xorriso : UPDATE : Thank you for being patient. Working since $i seconds.\" >&2\n\
                        i=$((i+1))\n\
                      done\n\
                      echo 'xorriso : FAILURE : libburn indicates failure with writing.' >&2\n\
                      echo \"xorriso : NOTE : Gave up -outdev ''\" >&2\n\
                      echo \"xorriso : NOTE : Giving up for -eject whole -dev ''\" >&2\n\
                      echo 'xorriso : FAILURE : -as cdrecord: Job could not be performed properly.' >&2\n\
                      echo \"xorriso : aborting : -abort_on 'FAILURE' encountered 'FATAL'\" >&2\n\
                      exit 5\n";
        let tools = fake_tools(script, dir.path());
        let err = burn_iso(
            &tools,
            "/dev/sr0",
            &iso,
            None,
            Duration::ZERO,
            &mut |_, _| {},
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.lines().next().unwrap().contains(" FAILURE : "), "{msg}");
        assert!(msg.contains("aborting"), "{msg}");
        assert!(!msg.contains("patient"), "{msg}");
        let log = std::fs::read_to_string(burn_log_path(&iso)).unwrap();
        assert!(log.contains("Working since 273 seconds"), "{log}");
        assert!(
            log.contains("libburn indicates failure with writing"),
            "{log}"
        );
    }

    #[test]
    fn burn_iso_failure_survives_trailing_noise() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("v.iso");
        std::fs::write(&iso, b"x").unwrap();
        let script = "#!/bin/sh\n\
                      echo 'libburn : FATAL : SCSI error on write(2048,16): [3 0C 00] Medium error.' >&2\n\
                      i=0\n\
                      while [ $i -lt 14 ]; do\n\
                        echo 'xorriso : UPDATE : Thank you for being patient.' >&2\n\
                        i=$((i+1))\n\
                      done\n\
                      exit 5\n";
        let tools = fake_tools(script, dir.path());
        let err = burn_iso(
            &tools,
            "/dev/sr0",
            &iso,
            None,
            Duration::ZERO,
            &mut |_, _| {},
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Medium error"), "{msg}");
        assert!(!msg.contains("patient"), "{msg}");
    }

    #[test]
    fn burn_iso_failure_falls_back_to_raw_tail() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("v.iso");
        std::fs::write(&iso, b"x").unwrap();
        let script = "#!/bin/sh\n\
                      echo 'something went sideways' >&2\n\
                      echo 'no severity markers here' >&2\n\
                      exit 1\n";
        let tools = fake_tools(script, dir.path());
        let err = burn_iso(
            &tools,
            "/dev/sr0",
            &iso,
            None,
            Duration::ZERO,
            &mut |_, _| {},
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("something went sideways"), "{msg}");
        assert!(msg.contains("no severity markers here"), "{msg}");
    }

    #[test]
    fn burn_iso_fails_with_last_stderr_lines() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("v.iso");
        std::fs::write(&iso, b"x").unwrap();
        let script = "#!/bin/sh\n\
                      echo 'xorriso : UPDATE :  1 of 100 MB written' >&2\n\
                      echo 'libburn : FATAL : SCSI error on write(268416,16): [3 0C 00] Medium error.' >&2\n\
                      exit 32\n";
        let tools = fake_tools(script, dir.path());
        let err = burn_iso(
            &tools,
            "/dev/sr0",
            &iso,
            None,
            Duration::ZERO,
            &mut |_, _| {},
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Medium error"), "{msg}");
        assert!(msg.contains("failed"), "{msg}");
    }

    #[test]
    fn burn_iso_requires_existing_iso() {
        let dir = tempfile::tempdir().unwrap();
        let tools = fake_tools("#!/bin/sh\nexit 0\n", dir.path());
        let missing = PathBuf::from("/nonexistent/x.iso");
        let err = burn_iso(
            &tools,
            "/dev/sr0",
            &missing,
            None,
            Duration::ZERO,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("stat ISO"));
    }

    #[test]
    fn format_streams_progress_and_passes_argv() {
        let dir = tempfile::tempdir().unwrap();
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > {argv}\n\
             printf 'xorriso : UPDATE : Formatting  ( 99.0%% done in 912 seconds )\\n' >&2\n\
             printf 'Formatting done\\n' >&2\n",
            argv = dir.path().join("argv.txt").display(),
        );
        let tools = fake_tools(&script, dir.path());
        let mut events = Vec::new();
        format_defect_management(&tools, "/dev/sr0", Duration::ZERO, &mut |p, l| {
            events.push((p, l))
        })
        .unwrap();
        let argv = std::fs::read_to_string(dir.path().join("argv.txt")).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            vec!["-outdev", "/dev/sr0", "-format", "as_needed"]
        );
        assert!(events.iter().any(|(p, _)| *p == Some(99.0)));
        assert!(events
            .iter()
            .any(|(p, l)| p.is_none() && l == "Formatting done"));
    }

    #[test]
    fn run_streaming_splits_carriage_returns() {
        let dir = tempfile::tempdir().unwrap();
        let script = "#!/bin/sh\n\
                      printf 'a\\rb\\r\\nc' >&2\n";
        let tools = fake_tools(script, dir.path());
        let mut lines = Vec::new();
        run_streaming(&tools.xorriso, &[], Duration::ZERO, &mut |l| {
            lines.push(l.to_string())
        })
        .unwrap();
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn watchdog_kills_a_stalled_tool() {
        let dir = tempfile::tempdir().unwrap();
        // one line, then silence forever. `exec` so the process under the
        // watchdog is a single process (like real xorriso), not a shell whose
        // orphaned `sleep` child would keep the pipe open after a kill.
        let script = "#!/bin/sh\nprintf 'started\\n'\nexec sleep 30\n";
        let tools = fake_tools(script, dir.path());
        let start = Instant::now();
        let err =
            run_streaming(&tools.xorriso, &[], Duration::from_secs(1), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("no output"), "{err}");
        // must not wait for the 30s sleep
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "watchdog too slow"
        );
    }

    #[test]
    fn watchdog_does_not_kill_a_chatty_tool() {
        let dir = tempfile::tempdir().unwrap();
        // steady keepalives for ~3s, then exit cleanly - inactivity never trips
        let script = "#!/bin/sh\n\
                      i=0\n\
                      while [ $i -lt 15 ]; do printf 'working %s\\n' \"$i\"; sleep 0.2; i=$((i+1)); done\n";
        let tools = fake_tools(script, dir.path());
        let mut lines = Vec::new();
        run_streaming(&tools.xorriso, &[], Duration::from_secs(1), &mut |l| {
            lines.push(l.to_string())
        })
        .unwrap();
        assert!(lines.len() >= 15, "expected keepalives, got {lines:?}");
    }
}
