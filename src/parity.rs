use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, ensure, Context, Result};

use crate::plan;
use crate::tools::Tools;

/// par2 create for one payload (file or directory tree): ONE recovery set.
/// cwd = the payload root's parent and -B<parent> pins the basepath there —
/// par2 otherwise derives it from the staged .par2 path and skips the sources
/// as "out of basepath source file". Member operands are passed relative so
/// the set stores disc-layout paths (repair with -B works from a disc copy);
/// slice size from Payload::slice_bytes (never the 2000-block default); -n1;
/// -m from available memory (min 512 MB, cap 4096 MB).
/// Output files land in out_dir; returns their paths.
pub fn create(
    tools: &Tools,
    payload: &plan::Payload,
    out_dir: &Path,
    redundancy_pct: u32,
    cb: &mut dyn FnMut(Option<f32>, String),
) -> Result<Vec<PathBuf>> {
    let par2 = tools
        .par2
        .as_ref()
        .context("par2 not found (install par2cmdline or par2cmdline-turbo)")?;
    let payload_name = payload.name.clone();
    let parent = payload
        .root
        .parent()
        .context("payload has no parent directory")?;
    let operands: Vec<String> = payload
        .parity_operands()
        .iter()
        .map(|s| s.to_string())
        .collect();
    ensure!(
        !operands.is_empty(),
        "payload has no non-empty files to protect: {}",
        payload.root.display()
    );

    std::fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;
    let out_dir = out_dir
        .canonicalize()
        .with_context(|| format!("resolve {}", out_dir.display()))?;
    let out_par2 = out_dir.join(format!("{payload_name}.par2"));

    let args = create_args(
        &operands,
        &out_par2,
        parent,
        redundancy_pct,
        payload.slice_bytes(),
        mem_mb(),
    );
    let mut child = Command::new(par2)
        .args(&args)
        .current_dir(parent)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {}", par2.display()))?;
    let _guard = crate::burn::ChildGuard::new(child.id());

    let mut stderr = child.stderr.take().context("no stderr pipe")?;
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let stdout = child.stdout.take().context("no stdout pipe")?;
    pump_lines(stdout, &mut |line| {
        cb(parse_progress_line(line), line.to_string())
    });

    let status = child.wait().context("wait for par2")?;
    let err_text = err_reader.join().unwrap_or_default();
    if !status.success() {
        bail!("par2 create failed ({status}): {}", err_text.trim());
    }

    let mut produced: Vec<PathBuf> = std::fs::read_dir(&out_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&payload_name) && n.ends_with(".par2"))
        })
        .collect();
    produced.sort();
    ensure!(
        !produced.is_empty(),
        "par2 exited 0 but no .par2 files appeared in {}",
        out_dir.display()
    );
    Ok(produced)
}

/// Pure: build the par2 argv (testable without par2 installed).
/// No -q: par2 prints the Processing:/Constructing: percent stream only at
/// default verbosity (par2creator.cpp gates it on noiselevel > nlQuiet).
pub fn create_args(
    operands: &[String],
    out_par2: &Path,
    basepath: &Path,
    redundancy_pct: u32,
    slice_bytes: u64,
    mem_mb: u32,
) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        format!("-B{}", basepath.display()),
        format!("-r{redundancy_pct}"),
        "-n1".to_string(),
        format!("-s{slice_bytes}"),
        format!("-m{mem_mb}"),
        out_par2.display().to_string(),
    ];
    args.extend(operands.iter().cloned());
    args
}

/// Pure: parse par2 stdout progress ("Constructing: 12.3%" / "Processing: ...").
pub fn parse_progress_line(line: &str) -> Option<f32> {
    let t = line.trim();
    if t == "Done" {
        return Some(100.0);
    }
    let (phase, rest) = t.split_once(": ")?;
    if !matches!(phase, "Processing" | "Constructing" | "Solving") {
        return None;
    }
    if rest == "done." {
        return Some(100.0);
    }
    let pct: f32 = rest.strip_suffix('%')?.parse().ok()?;
    (0.0..=100.0).contains(&pct).then_some(pct)
}

// par2 rewrites progress in place with '\r'; split on both terminators.
// Read errors end the pump instead of erroring out: create() must always
// reach child.wait() so the par2 process is reaped.
fn pump_lines(mut r: impl Read, f: &mut dyn FnMut(&str)) {
    let mut buf = [0u8; 8192];
    let mut acc: Vec<u8> = Vec::new();
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                if !acc.is_empty() {
                    f(&String::from_utf8_lossy(&acc));
                    acc.clear();
                }
            } else {
                acc.push(b);
            }
        }
    }
    if !acc.is_empty() {
        f(&String::from_utf8_lossy(&acc));
    }
}

fn mem_mb() -> u32 {
    std::fs::read_to_string("/proc/meminfo")
        .map(|t| mem_mb_from_meminfo(&t))
        .unwrap_or(512)
}

fn mem_mb_from_meminfo(text: &str) -> u32 {
    let avail_kb: u64 = text
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    (avail_kb / 1024 / 2).clamp(512, 4096) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real par2cmdline capture: https://tty.se/notes/par2/
    const CAPTURE: &str = include_str!("../tests/fixtures/par2_create_output.txt");
    // Live-stream reconstruction of the same run per the source format strings
    // (par2creator.cpp "Processing: ", reedsolomon.h "Constructing: "), with \r.
    const STREAM: &[u8] = include_bytes!("../tests/fixtures/par2_create_stream.txt");

    #[test]
    fn args_match_design_contract() {
        let args = create_args(
            &["vault.hc".to_string()],
            Path::new("/stage/parity/vault.hc.par2"),
            Path::new("/data"),
            15,
            2_097_152,
            2048,
        );
        assert_eq!(
            args,
            vec![
                "create",
                "-B/data",
                "-r15",
                "-n1",
                "-s2097152",
                "-m2048",
                "/stage/parity/vault.hc.par2",
                "vault.hc",
            ]
        );
    }

    #[test]
    fn args_append_every_dir_member_operand() {
        let args = create_args(
            &["extras/a.bin".to_string(), "extras/sub/b.bin".to_string()],
            Path::new("/stage/parity/extras.par2"),
            Path::new("/data"),
            15,
            65_536,
            512,
        );
        assert_eq!(
            &args[6..],
            &[
                "/stage/parity/extras.par2",
                "extras/a.bin",
                "extras/sub/b.bin",
            ]
        );
    }

    #[test]
    fn slice_arg_never_defaults_to_2000_blocks() {
        let size = 93 * 1024 * 1024 * 1024u64;
        let slice = plan::slice_bytes_for(size, 1);
        assert!(size.div_ceil(slice) <= plan::PAR2_MAX_BLOCKS);
        assert!(size.div_ceil(slice) > 2000);
        let args = create_args(
            &["v".to_string()],
            Path::new("v.par2"),
            Path::new("."),
            15,
            slice,
            512,
        );
        assert!(args.iter().any(|a| *a == format!("-s{slice}")));
    }

    #[test]
    fn progress_lines_from_capture() {
        for line in CAPTURE.lines() {
            match line {
                "Constructing: done." | "Done" => {
                    assert_eq!(parse_progress_line(line), Some(100.0), "{line}")
                }
                _ => assert_eq!(parse_progress_line(line), None, "{line}"),
            }
        }
    }

    #[test]
    fn progress_percent_variants() {
        assert_eq!(parse_progress_line("Processing: 12.3%"), Some(12.3));
        assert_eq!(parse_progress_line("Constructing: 45.6%"), Some(45.6));
        assert_eq!(parse_progress_line("Solving: 0.1%"), Some(0.1));
        assert_eq!(parse_progress_line("Processing: 100.0%"), Some(100.0));
        assert_eq!(parse_progress_line("  Processing: 5.0%  "), Some(5.0));
        assert_eq!(parse_progress_line("Redundancy: 15%"), None);
        assert_eq!(parse_progress_line("Processing: junk%"), None);
        assert_eq!(parse_progress_line("Processing: 12.3"), None);
        assert_eq!(parse_progress_line("Processing: 850.0%"), None);
        assert_eq!(parse_progress_line("Wrote 5244000 bytes to disk"), None);
        assert_eq!(parse_progress_line(""), None);
    }

    #[test]
    fn pump_splits_on_carriage_returns() {
        let mut lines = Vec::new();
        pump_lines(STREAM, &mut |l| lines.push(l.to_string()));
        assert!(lines.contains(&"Constructing: 45.6%".to_string()));
        assert!(lines.contains(&"Processing: 99.9%".to_string()));
        assert_eq!(lines.last().map(String::as_str), Some("Done"));
        let pcts: Vec<f32> = lines
            .iter()
            .filter_map(|l| parse_progress_line(l))
            .collect();
        assert_eq!(pcts, vec![12.3, 45.6, 99.9, 100.0, 0.1, 12.3, 99.9, 100.0]);
    }

    #[test]
    fn mem_halves_available_and_clamps() {
        assert_eq!(mem_mb_from_meminfo("MemAvailable:    4194304 kB\n"), 2048);
        assert_eq!(mem_mb_from_meminfo("MemAvailable:   33554432 kB\n"), 4096);
        assert_eq!(mem_mb_from_meminfo("MemAvailable:     524288 kB\n"), 512);
        assert_eq!(mem_mb_from_meminfo("MemTotal: 1 kB\n"), 512);
        assert_eq!(mem_mb_from_meminfo(""), 512);
    }

    fn inspect(path: &Path) -> plan::Payload {
        plan::Payload::inspect(path.to_path_buf()).unwrap().0
    }

    // fork/exec ETXTBSY race: another test thread may hold a just-written fake
    // script open for write at our child's execve; retry, never in prod code.
    fn create_retrying(
        tools: &Tools,
        payload: &plan::Payload,
        out_dir: &Path,
        events: &mut Vec<(Option<f32>, String)>,
    ) -> Result<Vec<PathBuf>> {
        loop {
            events.clear();
            match create(tools, payload, out_dir, 15, &mut |p, l| events.push((p, l))) {
                Err(e) if format!("{e:#}").contains("Text file busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                r => return r,
            }
        }
    }

    #[test]
    fn create_runs_par2_in_payload_dir_and_collects_outputs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, vec![0u8; 128 * 1024]).unwrap();
        let out_dir = dir.path().join("parity");
        let fake = dir.path().join("par2");
        std::fs::write(
            &fake,
            "#!/bin/sh\n\
             out=$7\n\
             d=$(dirname \"$out\")\n\
             { pwd; printf '%s\\n' \"$@\"; } > \"$d/argv.txt\"\n\
             printf 'Constructing: 50.0%%\\r'\n\
             printf 'Constructing: done.\\n'\n\
             printf 'Processing: 99.9%%\\r'\n\
             printf 'Done\\n'\n\
             : > \"$out\"\n\
             : > \"${out%.par2}.vol000+01.par2\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tools = Tools {
            xorriso: "/bin/true".into(),
            par2: Some(fake),
            par2_version: None,
            udisksctl: None,
            veracrypt: None,
            eject: None,
            mediainfo: None,
        };

        let mut events = Vec::new();
        let files = create_retrying(&tools, &inspect(&payload), &out_dir, &mut events).unwrap();

        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["vault.hc.par2", "vault.hc.vol000+01.par2"]);
        assert!(events.iter().any(|(p, _)| *p == Some(50.0)));
        assert!(events.iter().any(|(p, l)| *p == Some(100.0) && l == "Done"));

        let argv = std::fs::read_to_string(out_dir.join("argv.txt")).unwrap();
        let mut it = argv.lines();
        assert_eq!(
            std::fs::canonicalize(it.next().unwrap()).unwrap(),
            payload.parent().unwrap().canonicalize().unwrap()
        );
        let rest: Vec<_> = it.collect();
        assert_eq!(rest[0], "create");
        assert_eq!(
            std::fs::canonicalize(rest[1].strip_prefix("-B").unwrap()).unwrap(),
            payload.parent().unwrap().canonicalize().unwrap()
        );
        assert_eq!(rest[2], "-r15");
        assert_eq!(rest[3], "-n1");
        assert_eq!(rest[4], "-s65536");
        assert!(rest[5].starts_with("-m"));
        assert!(rest[6].ends_with("parity/vault.hc.par2"));
        assert_eq!(rest[7], "vault.hc");
    }

    #[test]
    fn create_fails_cleanly_without_par2() {
        let tools = Tools {
            xorriso: "/bin/true".into(),
            par2: None,
            par2_version: None,
            udisksctl: None,
            veracrypt: None,
            eject: None,
            mediainfo: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("v");
        std::fs::write(&payload, b"x").unwrap();
        let err = create(&tools, &inspect(&payload), dir.path(), 15, &mut |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("par2 not found"));
    }

    #[test]
    fn create_rejects_payload_with_only_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let extras = dir.path().join("extras");
        std::fs::create_dir(&extras).unwrap();
        std::fs::write(extras.join("empty.bin"), b"").unwrap();
        let tools = Tools {
            xorriso: "/bin/true".into(),
            par2: Some("/bin/true".into()),
            par2_version: None,
            udisksctl: None,
            veracrypt: None,
            eject: None,
            mediainfo: None,
        };
        let err = create(
            &tools,
            &inspect(&extras),
            &dir.path().join("out"),
            15,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("no non-empty files to protect"),
            "{err}"
        );
    }

    #[test]
    fn create_surfaces_par2_failure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("v");
        std::fs::write(&payload, b"x").unwrap();
        let fake = dir.path().join("par2");
        std::fs::write(&fake, "#!/bin/sh\necho 'boom' >&2\nexit 3\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tools = Tools {
            xorriso: "/bin/true".into(),
            par2: Some(fake),
            par2_version: None,
            udisksctl: None,
            veracrypt: None,
            eject: None,
            mediainfo: None,
        };
        let mut events = Vec::new();
        let err = create_retrying(
            &tools,
            &inspect(&payload),
            &dir.path().join("out"),
            &mut events,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("par2 create failed"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }
}
