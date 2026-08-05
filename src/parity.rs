use std::path::{Path, PathBuf};
use std::time::Duration;

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
/// `stall` is the inactivity watchdog (a wedged disk mid-par2 must not hang).
/// Output files land in out_dir; returns their paths.
pub fn create(
    tools: &Tools,
    payload: &plan::Payload,
    out_dir: &Path,
    redundancy_pct: u32,
    stall: Duration,
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
        mem_mb(MEM_CAP_MB),
    );
    run_par2_create(par2, &args, Some(parent), stall, cb)?;
    collect_outputs(&out_dir, &payload_name)
}

/// par2 create for a multi-disc SET: ONE recovery set across the part files
/// in `set_dir`. cwd = set_dir and -B<set_dir>, so the set stores bare part
/// names and `par2 r` works from any directory of collected files. Exact
/// -c<recovery_blocks>, NEVER -r: the count was derived from the parity
/// disc's capacity at plan time (span::plan_span); a percentage would let
/// par2 re-round it. Outputs land in out_dir: `<source>.par2` (the index,
/// burned onto EVERY disc) plus the recovery volume(s).
pub fn create_set(
    tools: &Tools,
    span: &crate::span::SpanPlan,
    set_dir: &Path,
    out_dir: &Path,
    stall: Duration,
    cb: &mut dyn FnMut(Option<f32>, String),
) -> Result<Vec<PathBuf>> {
    let par2 = tools
        .par2
        .as_ref()
        .context("par2 not found (install par2cmdline or par2cmdline-turbo)")?;
    ensure!(
        span.recovery_blocks > 0,
        "create_set called with no recovery blocks planned"
    );
    let part_names: Vec<String> = span
        .discs
        .iter()
        .filter_map(|d| d.part.as_ref())
        .map(|p| p.file_name.clone())
        .collect();
    ensure!(!part_names.is_empty(), "span plan has no parts");
    let set_dir = set_dir
        .canonicalize()
        .with_context(|| format!("resolve {}", set_dir.display()))?;
    std::fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;
    let out_dir = out_dir
        .canonicalize()
        .with_context(|| format!("resolve {}", out_dir.display()))?;
    let out_par2 = out_dir.join(format!("{}.par2", span.source_name));

    let cap = if span.source_bytes > LARGE_SOURCE_BYTES {
        MEM_CAP_LARGE_MB
    } else {
        MEM_CAP_MB
    };
    let args = create_set_args(
        &part_names,
        &out_par2,
        &set_dir,
        span.block,
        span.recovery_blocks,
        mem_mb(cap),
    );
    run_par2_create(par2, &args, Some(&set_dir), stall, cb)?;
    collect_outputs(&out_dir, &span.source_name)
}

// stdout carries the Processing:/Constructing: progress stream, stderr the
// failure cause; both ride the shared watchdog-equipped pump.
fn run_par2_create(
    par2: &Path,
    args: &[String],
    cwd: Option<&Path>,
    stall: Duration,
    cb: &mut dyn FnMut(Option<f32>, String),
) -> Result<()> {
    let mut err_text = String::new();
    let status = crate::proc::stream_lines(par2, args, cwd, stall, &mut |is_err, line| {
        if is_err {
            err_text.push_str(line);
            err_text.push('\n');
        } else {
            cb(parse_progress_line(line), line.to_string());
        }
    })?;
    if !status.success() {
        bail!("par2 create failed ({status}): {}", err_text.trim());
    }
    Ok(())
}

fn collect_outputs(out_dir: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let mut produced: Vec<PathBuf> = std::fs::read_dir(out_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".par2"))
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

/// Pure: build the set-level par2 argv (testable without par2). Exact
/// -c count, never -r; operands are the bare part names, resolved against
/// -B<set_dir> which is also the cwd.
pub fn create_set_args(
    part_names: &[String],
    out_par2: &Path,
    set_dir: &Path,
    block: u64,
    recovery_blocks: u64,
    mem_mb: u32,
) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        format!("-B{}", set_dir.display()),
        format!("-s{block}"),
        format!("-c{recovery_blocks}"),
        "-n1".to_string(),
        format!("-m{mem_mb}"),
        out_par2.display().to_string(),
    ];
    args.extend(part_names.iter().cloned());
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

const MEM_CAP_MB: u32 = 4096;
// A set-level run hashes 32k blocks across tens of GB; the higher ceiling
// roughly halves its wall-clock hours on machines that have the RAM.
const MEM_CAP_LARGE_MB: u32 = 8192;
const LARGE_SOURCE_BYTES: u64 = 50 * 1024 * 1024 * 1024;

fn mem_mb(cap_mb: u32) -> u32 {
    std::fs::read_to_string("/proc/meminfo")
        .map(|t| mem_mb_from_meminfo(&t, cap_mb))
        .unwrap_or(512)
}

fn mem_mb_from_meminfo(text: &str, cap_mb: u32) -> u32 {
    let avail_kb: u64 = text
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    (avail_kb / 1024 / 2).clamp(512, cap_mb as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real par2cmdline capture: https://tty.se/notes/par2/
    const CAPTURE: &str = include_str!("../tests/fixtures/par2_create_output.txt");
    // Live-stream reconstruction of the same run per the source format strings
    // (par2creator.cpp "Processing: ", reedsolomon.h "Constructing: "), with \r.
    const STREAM_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/par2_create_stream.txt"
    );

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
    fn create_splits_carriage_return_progress() {
        use std::os::unix::fs::PermissionsExt;
        // a fake par2 replays the real \r-terminated stream; every in-place
        // progress update must arrive as its own callback, not one mega-line
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, vec![0u8; 64 * 1024]).unwrap();
        let fake = dir.path().join("par2");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n\
                 out=$7\n\
                 cat {STREAM_PATH}\n\
                 : > \"$out\"\n\
                 : > \"${{out%.par2}}.vol000+01.par2\"\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut tools = Tools::bare("/bin/true");
        tools.par2 = Some(fake);
        let mut events = Vec::new();
        create(
            &tools,
            &inspect(&payload),
            &dir.path().join("out"),
            15,
            Duration::ZERO,
            &mut |p, l| events.push((p, l)),
        )
        .unwrap();
        let pcts: Vec<f32> = events.iter().filter_map(|(p, _)| *p).collect();
        assert_eq!(pcts, vec![12.3, 45.6, 99.9, 100.0, 0.1, 12.3, 99.9, 100.0]);
    }

    #[test]
    fn create_kills_a_stalled_par2() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("vault.hc");
        std::fs::write(&payload, vec![0u8; 1024]).unwrap();
        let fake = dir.path().join("par2");
        // one line then silence; exec so the watched process is par2 itself
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf 'Processing: 1.0%%\\n'\nexec sleep 120\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut tools = Tools::bare("/bin/true");
        tools.par2 = Some(fake);
        let start = std::time::Instant::now();
        let err = create(
            &tools,
            &inspect(&payload),
            &dir.path().join("out"),
            15,
            Duration::from_secs(1),
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("no output"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "watchdog never fired for par2"
        );
    }

    #[test]
    fn mem_halves_available_and_clamps() {
        let m = |t| mem_mb_from_meminfo(t, MEM_CAP_MB);
        assert_eq!(m("MemAvailable:    4194304 kB\n"), 2048);
        assert_eq!(m("MemAvailable:   33554432 kB\n"), 4096);
        assert_eq!(m("MemAvailable:     524288 kB\n"), 512);
        assert_eq!(m("MemTotal: 1 kB\n"), 512);
        assert_eq!(m(""), 512);
    }

    #[test]
    fn mem_cap_rises_for_huge_sources() {
        // 32 GiB available: half is 16 GiB, clamped by whichever cap applies
        assert_eq!(
            mem_mb_from_meminfo("MemAvailable: 33554432 kB\n", MEM_CAP_MB),
            4096
        );
        assert_eq!(
            mem_mb_from_meminfo("MemAvailable: 33554432 kB\n", MEM_CAP_LARGE_MB),
            8192
        );
        // below the cap the caps agree
        assert_eq!(
            mem_mb_from_meminfo("MemAvailable: 4194304 kB\n", MEM_CAP_LARGE_MB),
            2048
        );
    }

    fn inspect(path: &Path) -> plan::Payload {
        plan::Payload::inspect(path.to_path_buf()).unwrap().0
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
        let mut tools = Tools::bare("/bin/true");
        tools.par2 = Some(fake);

        let mut events = Vec::new();
        let files = create(
            &tools,
            &inspect(&payload),
            &out_dir,
            15,
            Duration::ZERO,
            &mut |p, l| events.push((p, l)),
        )
        .unwrap();

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
        let tools = Tools::bare("/bin/true");
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("v");
        std::fs::write(&payload, b"x").unwrap();
        let err = create(
            &tools,
            &inspect(&payload),
            dir.path(),
            15,
            Duration::ZERO,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("par2 not found"));
    }

    #[test]
    fn create_rejects_payload_with_only_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let extras = dir.path().join("extras");
        std::fs::create_dir(&extras).unwrap();
        std::fs::write(extras.join("empty.bin"), b"").unwrap();
        let mut tools = Tools::bare("/bin/true");
        tools.par2 = Some("/bin/true".into());
        let err = create(
            &tools,
            &inspect(&extras),
            &dir.path().join("out"),
            15,
            Duration::ZERO,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("no non-empty files to protect"),
            "{err}"
        );
    }

    #[test]
    fn create_set_args_match_design_contract() {
        let args = create_set_args(
            &["vault.hc.001".to_string(), "vault.hc.002".to_string()],
            Path::new("/stage/parity/vault.hc.par2"),
            Path::new("/stage/set"),
            1_966_204,
            11_357,
            2048,
        );
        assert_eq!(
            args,
            vec![
                "create",
                "-B/stage/set",
                "-s1966204",
                "-c11357",
                "-n1",
                "-m2048",
                "/stage/parity/vault.hc.par2",
                "vault.hc.001",
                "vault.hc.002",
            ]
        );
        // NEVER -r: the block count is exact, a percentage would re-round it
        assert!(!args.iter().any(|a| a.starts_with("-r")), "{args:?}");
    }

    fn span_plan(
        set_dir: &Path,
        source: &str,
        part_bytes: u64,
        n: u32,
        r: u64,
    ) -> crate::span::SpanPlan {
        use crate::span::{DiscPlan, DiscRole, PartPlan, SpanPlan};
        let mut discs: Vec<DiscPlan> = (1..=n)
            .map(|k| {
                let name = crate::span::part_file_name(source, k);
                let bytes: Vec<u8> = (0..part_bytes)
                    .map(|i| (i as u8).wrapping_mul(k as u8).wrapping_add(k as u8))
                    .collect();
                std::fs::write(set_dir.join(&name), bytes).unwrap();
                DiscPlan {
                    index: k,
                    label: format!("T_{k}OF{}", n + 1),
                    role: DiscRole::Data,
                    part: Some(PartPlan {
                        file_name: name,
                        offset: (k as u64 - 1) * part_bytes,
                        bytes: part_bytes,
                    }),
                }
            })
            .collect();
        discs.push(DiscPlan {
            index: n + 1,
            label: format!("T_{}OF{}", n + 1, n + 1),
            role: DiscRole::Parity,
            part: None,
        });
        SpanPlan {
            base_label: "T".into(),
            discs,
            source_name: source.into(),
            source_bytes: part_bytes * n as u64,
            block: 65_536,
            recovery_blocks: r,
            per_disc_iso_est: 0,
            staging_peak: 0,
        }
    }

    #[test]
    fn create_set_runs_par2_in_set_dir_with_bare_part_names() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let set_dir = dir.path().join("set");
        std::fs::create_dir(&set_dir).unwrap();
        let span = span_plan(&set_dir, "vault.hc", 128 * 1024, 3, 7);
        let out_dir = dir.path().join("parity");
        let fake = dir.path().join("par2");
        std::fs::write(
            &fake,
            "#!/bin/sh\n\
             out=$7\n\
             d=$(dirname \"$out\")\n\
             { pwd; printf '%s\\n' \"$@\"; } > \"$d/argv.txt\"\n\
             printf 'Constructing: 50.0%%\\r'\n\
             printf 'Done\\n'\n\
             : > \"$out\"\n\
             : > \"${out%.par2}.vol000+07.par2\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut tools = Tools::bare("/bin/true");
        tools.par2 = Some(fake);

        let mut events = Vec::new();
        let files = create_set(
            &tools,
            &span,
            &set_dir,
            &out_dir,
            Duration::ZERO,
            &mut |p, l| events.push((p, l)),
        )
        .unwrap();

        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["vault.hc.par2", "vault.hc.vol000+07.par2"]);
        assert!(events.iter().any(|(p, _)| *p == Some(50.0)));
        assert!(events.iter().any(|(p, l)| *p == Some(100.0) && l == "Done"));

        let argv = std::fs::read_to_string(out_dir.join("argv.txt")).unwrap();
        let mut it = argv.lines();
        let canon_set = set_dir.canonicalize().unwrap();
        assert_eq!(
            std::fs::canonicalize(it.next().unwrap()).unwrap(),
            canon_set,
            "par2 must run in the set dir"
        );
        let rest: Vec<&str> = it.collect();
        assert_eq!(rest[0], "create");
        assert_eq!(
            std::fs::canonicalize(rest[1].strip_prefix("-B").unwrap()).unwrap(),
            canon_set
        );
        assert_eq!(rest[2], "-s65536");
        assert_eq!(rest[3], "-c7");
        assert_eq!(rest[4], "-n1");
        assert!(rest[5].starts_with("-m"));
        assert!(rest[6].ends_with("parity/vault.hc.par2"));
        assert_eq!(
            &rest[7..],
            &["vault.hc.001", "vault.hc.002", "vault.hc.003"]
        );
        assert!(!rest.iter().any(|a| a.starts_with("-r")), "{rest:?}");
    }

    #[test]
    fn create_set_rejects_a_parity_free_plan() {
        let dir = tempfile::tempdir().unwrap();
        let set_dir = dir.path().join("set");
        std::fs::create_dir(&set_dir).unwrap();
        let span = span_plan(&set_dir, "v", 1024, 2, 0);
        let mut tools = Tools::bare("/bin/true");
        tools.par2 = Some("/bin/true".into());
        let err = create_set(
            &tools,
            &span,
            &set_dir,
            &dir.path().join("out"),
            Duration::ZERO,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("no recovery blocks"), "{err}");
    }

    // The most important test in M7: with real par2, the set-level recovery
    // volumes rebuild a COMPLETELY LOST part from the survivors - the
    // one-disc-loss promise, end to end.
    #[test]
    fn create_set_rebuilds_a_lost_part_with_real_par2() {
        let Some(par2) = crate::tools::which("par2") else {
            eprintln!("skipping: par2 not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let set_dir = dir.path().join("set");
        std::fs::create_dir(&set_dir).unwrap();
        // 3 parts x 3 blocks at -s65536; -c4 covers any one part plus slack
        let span = span_plan(&set_dir, "blob.bin", 192 * 1024, 3, 4);
        let lost = std::fs::read(set_dir.join("blob.bin.002")).unwrap();
        let out_dir = dir.path().join("parity");
        let mut tools = Tools::bare("/bin/true");
        tools.par2 = Some(par2.clone());
        let files = create_set(
            &tools,
            &span,
            &set_dir,
            &out_dir,
            Duration::from_secs(120),
            &mut |_, _| {},
        )
        .unwrap();
        assert!(files.len() >= 2, "index + volumes expected: {files:?}");

        // a lost disc: its part is GONE; survivors + /parity/* in one dir
        let collected = dir.path().join("collected");
        std::fs::create_dir(&collected).unwrap();
        for name in ["blob.bin.001", "blob.bin.003"] {
            std::fs::copy(set_dir.join(name), collected.join(name)).unwrap();
        }
        for f in &files {
            std::fs::copy(f, collected.join(f.file_name().unwrap())).unwrap();
        }
        let out = std::process::Command::new(&par2)
            .arg("r")
            .arg("blob.bin.par2")
            .current_dir(&collected)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "par2 r failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let rebuilt = std::fs::read(collected.join("blob.bin.002")).unwrap();
        assert_eq!(rebuilt, lost, "reconstructed part must be byte-identical");
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
        let mut tools = Tools::bare("/bin/true");
        tools.par2 = Some(fake);
        let mut events = Vec::new();
        let err = create(
            &tools,
            &inspect(&payload),
            &dir.path().join("out"),
            15,
            Duration::ZERO,
            &mut |p, l| events.push((p, l)),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("par2 create failed"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }
}
