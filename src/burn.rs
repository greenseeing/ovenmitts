use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{bail, Context, Result};

use crate::tools::Tools;

/// Optional drive-level defect management: `xorriso -outdev <dev> -format as_needed`.
/// Caller MUST re-probe media afterwards - formatted capacity shrinks
/// (observed: 768 MiB loss on BD-R 25).
pub fn format_defect_management(
    tools: &Tools,
    device: &str,
    cb: &mut dyn FnMut(Option<f32>, String),
) -> Result<()> {
    let args: Vec<String> = vec![
        "-outdev".into(),
        device.into(),
        "-format".into(),
        "as_needed".into(),
    ];
    run_streaming(&tools.xorriso, &args, &mut |line| forward(line, 0, cb))
}

/// Burn: `xorriso -as cdrecord -v dev=<dev> [speed=<n>] fs=64m blank=as_needed -eject <iso>`.
/// Unformatted BD-R = stream recording at full speed (research decision #3).
pub fn burn_iso(
    tools: &Tools,
    device: &str,
    iso: &Path,
    speed: Option<u32>,
    cb: &mut dyn FnMut(Option<f32>, String),
) -> Result<()> {
    let total_bytes = std::fs::metadata(iso)
        .with_context(|| format!("stat ISO {}", iso.display()))?
        .len();
    let args = burn_args(device, iso, speed);
    run_streaming(&tools.xorriso, &args, &mut |line| {
        forward(line, total_bytes, cb)
    })
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

// Live child PIDs of long-running external tools, so an interactive
// force-quit can terminate them instead of orphaning a burn in progress.
static ACTIVE_CHILDREN: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

pub(crate) struct ChildGuard(i32);

impl ChildGuard {
    pub(crate) fn new(pid: u32) -> Self {
        let pid = pid as i32;
        if let Ok(mut v) = ACTIVE_CHILDREN.lock() {
            v.push(pid);
        }
        Self(pid)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(mut v) = ACTIVE_CHILDREN.lock() {
            v.retain(|p| *p != self.0);
        }
    }
}

/// Signal every registered child (force-quit path); best effort.
pub fn terminate_active(force: bool) {
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    if let Ok(v) = ACTIVE_CHILDREN.lock() {
        for pid in v.iter() {
            unsafe { libc::kill(*pid, sig) };
        }
    }
}

// libburn rewrites progress in place with '\r'; split on both terminators.
pub(crate) fn run_streaming(
    bin: &Path,
    args: &[String],
    on_line: &mut dyn FnMut(&str),
) -> Result<()> {
    let mut child = spawn_retrying(bin, args)?;
    let _guard = ChildGuard::new(child.id());
    let stdout = child.stdout.take().context("no stdout pipe")?;
    let stderr = child.stderr.take().context("no stderr pipe")?;
    let (tx, rx) = mpsc::channel::<(bool, String)>();
    let tx_err = tx.clone();
    let t_out = std::thread::spawn(move || pump(stdout, false, tx));
    let t_err = std::thread::spawn(move || pump(stderr, true, tx_err));

    let mut tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL);
    for (is_err, line) in rx {
        if is_err {
            if tail.len() == STDERR_TAIL {
                tail.pop_front();
            }
            tail.push_back(line.clone());
        }
        on_line(&line);
    }
    let _ = t_out.join();
    let _ = t_err.join();
    let status = child
        .wait()
        .with_context(|| format!("wait for {}", bin.display()))?;
    if !status.success() {
        let tail: Vec<String> = tail.into();
        bail!("{} failed ({status}):\n{}", bin.display(), tail.join("\n"));
    }
    Ok(())
}

// ETXTBSY at exec is a transient fork/exec race (a concurrent fork briefly
// inherits a write fd to the binary); it only ever fires in tests with fake
// scripts, never on an installed xorriso.
pub(crate) fn spawn_retrying(bin: &Path, args: &[String]) -> Result<std::process::Child> {
    let mut tries = 0;
    loop {
        match Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 20 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e).with_context(|| format!("spawn {}", bin.display())),
        }
    }
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
        burn_iso(&tools, "/dev/sr0", &iso, Some(4), &mut |p, l| {
            events.push((p, l))
        })
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
        let err = burn_iso(&tools, "/dev/sr0", &iso, None, &mut |_, _| {}).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Medium error"), "{msg}");
        assert!(msg.contains("failed"), "{msg}");
    }

    #[test]
    fn burn_iso_requires_existing_iso() {
        let dir = tempfile::tempdir().unwrap();
        let tools = fake_tools("#!/bin/sh\nexit 0\n", dir.path());
        let missing = PathBuf::from("/nonexistent/x.iso");
        let err = burn_iso(&tools, "/dev/sr0", &missing, None, &mut |_, _| {}).unwrap_err();
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
        format_defect_management(&tools, "/dev/sr0", &mut |p, l| events.push((p, l))).unwrap();
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
        run_streaming(&tools.xorriso, &[], &mut |l| lines.push(l.to_string())).unwrap();
        assert_eq!(lines, vec!["a", "b", "c"]);
    }
}
