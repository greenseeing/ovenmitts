use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};

use crate::plan::{self, Payload};
use crate::tools::Tools;

pub struct MasterInput<'a> {
    pub label: &'a str,
    pub payloads: &'a [Payload],
    pub parity_files: &'a [PathBuf],
    pub checksums: &'a Path,
    pub manifest: &'a Path,
    pub recovery: &'a Path,
    pub out_iso: &'a Path,
}

/// Disc layout:
///   /<payload files>            (root)
///   /parity/<name>.par2 ...
///   /checksums.sha256
///   /MANIFEST.txt
///   /RECOVERY.txt
pub fn build_iso(
    tools: &Tools,
    input: &MasterInput,
    stall: std::time::Duration,
    cb: &mut dyn FnMut(Option<f32>, String),
) -> Result<u64> {
    let args = master_args(input);
    crate::proc::run_streaming(
        &tools.xorriso,
        &args,
        stall,
        &mut |line| match parse_progress_line(line) {
            Some(pct) => cb(Some(pct), strip_update(line).trim().to_string()),
            None => {
                let t = line.trim();
                if !t.is_empty() {
                    cb(None, t.to_string());
                }
            }
        },
    )?;
    let size = std::fs::metadata(input.out_iso)
        .with_context(|| format!("stat mastered ISO {}", input.out_iso.display()))?
        .len();
    ensure!(
        size > 0,
        "xorriso produced an empty ISO at {}",
        input.out_iso.display()
    );
    // xorriso wrote the image; make sure it is on the platter (data AND
    // directory entry) before the burn reads it back or reports it staged.
    crate::fsutil::fsync_existing(input.out_iso)?;
    Ok(size)
}

/// Pure: build the xorriso mastering argv (testable without xorriso).
pub fn master_args(input: &MasterInput) -> Vec<String> {
    let mut args: Vec<String> = [
        "-as",
        "mkisofs",
        "-iso-level",
        "3",
        "-rock",
        "--md5",
        "-V",
        input.label,
        "-o",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(input.out_iso.display().to_string());
    args.push("-graft-points".into());
    for p in input.payloads {
        // a directory source grafts its whole tree under /<name>
        args.push(graft(&format!("/{}", p.name), &p.root));
    }
    for f in input.parity_files {
        args.push(graft(&format!("/parity/{}", file_name_of(f)), f));
    }
    args.push(graft("/checksums.sha256", input.checksums));
    args.push(graft("/MANIFEST.txt", input.manifest));
    args.push(graft("/RECOVERY.txt", input.recovery));
    args
}

// xorrisofs splits pathspecs at the first unescaped '='; only the ISO side
// needs \= and \\ escapes, the disk side is taken verbatim after the split.
fn graft(target: &str, source: &Path) -> String {
    let escaped = target.replace('\\', "\\\\").replace('=', "\\=");
    format!("{escaped}={}", source.display())
}

pub(crate) fn file_name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn strip_update(line: &str) -> &str {
    line.trim_start()
        .strip_prefix("xorriso : UPDATE :")
        .unwrap_or(line)
}

/// Pure: parse mkisofs-emulation progress lines ("  12.34% done, ...").
pub fn parse_progress_line(line: &str) -> Option<f32> {
    let rest = strip_update(line).trim_start();
    let idx = rest.find('%')?;
    let (num, tail) = rest.split_at(idx);
    if !tail.starts_with("% done") {
        return None;
    }
    let pct: f32 = num.trim().parse().ok()?;
    // genuine mkisofs estimates can slightly overshoot 100%
    (0.0..=200.0).contains(&pct).then_some(pct.min(100.0))
}

pub struct ManifestEntry {
    pub name: String,
    pub bytes: u64,
    pub files: usize,
    pub is_dir: bool,
    /// Some for file payloads; dir payloads carry per-file hashes in
    /// checksums.sha256 instead.
    pub sha256: Option<String>,
    /// Some when parity ran: (slice bytes, block count summed per member —
    /// every file's tail slice rounds up, so this is not bytes/slice).
    pub par2: Option<(u64, u64)>,
}

/// MANIFEST.txt: date, label, parameters (redundancy, slice size, defect
/// management), one row per top-level payload. Credits only standard tools —
/// on-disc files never name ovenmitts.
pub fn write_manifest(
    out: &Path,
    label: &str,
    payloads: &[ManifestEntry],
    redundancy_pct: Option<u32>,
    defect_management: bool,
    ecc: bool,
) -> Result<()> {
    use std::fmt::Write as _;
    let mut t = String::new();
    let _ = writeln!(t, "MANIFEST");
    let _ = writeln!(
        t,
        "created: {}",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );
    let _ = writeln!(t, "volume label: {label}");
    let _ = writeln!(t);
    let _ = writeln!(t, "parameters:");
    let _ = writeln!(
        t,
        "  mastered and burned: xorriso (ISO 9660 level 3, Rock Ridge, embedded MD5 tags)"
    );
    match redundancy_pct {
        Some(pct) => {
            let _ = writeln!(t, "  parity: par2, {pct}% redundancy, one set per payload");
        }
        None => {
            let _ = writeln!(t, "  parity: none");
        }
    }
    let _ = writeln!(
        t,
        "  defect management: {}",
        if defect_management {
            "on (drive-formatted, spare areas)"
        } else {
            "off (stream recording)"
        }
    );
    let _ = writeln!(
        t,
        "  sector ecc: {}",
        if ecc {
            "dvdisaster RS02, embedded when free capacity allowed (self-identifying)"
        } else {
            "none"
        }
    );
    let _ = writeln!(t);
    let _ = writeln!(t, "payloads:");
    for e in payloads {
        let _ = writeln!(t, "  {}{}", e.name, if e.is_dir { "/" } else { "" });
        let _ = writeln!(t, "    bytes: {} ({})", e.bytes, plan::human_bytes(e.bytes));
        if e.is_dir {
            let _ = writeln!(t, "    files: {}", e.files);
        }
        match &e.sha256 {
            Some(sha) => {
                let _ = writeln!(t, "    sha256: {sha}");
            }
            None => {
                let _ = writeln!(t, "    sha256: per-file hashes in checksums.sha256");
            }
        }
        if let Some((slice, blocks)) = e.par2 {
            let _ = writeln!(t, "    par2 slice: {slice} bytes ({blocks} blocks)");
        }
    }
    crate::fsutil::write_durable(out, t)
}

/// RECOVERY.txt: self-documenting restore instructions (darbrrb lesson) -
/// exact commands for: mount + copy, ddrescue a failing disc, par2repair the
/// payload, regenerate the file->LBA map from disc or ISO, VeraCrypt header
/// restore pointers.
pub fn write_recovery(out: &Path, label: &str, payloads: &[Payload], ecc: bool) -> Result<()> {
    use std::fmt::Write as _;
    let mut t = String::new();
    let mut step = 0u32;
    let mut num = move || {
        step += 1;
        step
    };
    let _ = writeln!(t, "RECOVERY — disc \"{label}\"");
    let _ = writeln!(
        t,
        "Mastered and burned with xorriso; recovery needs only standard tools"
    );
    if ecc {
        let _ = writeln!(t, "(ddrescue, dvdisaster, par2, xorriso).");
    } else {
        let _ = writeln!(t, "(ddrescue, par2, xorriso).");
    }
    let _ = writeln!(t, "Commands assume Linux with this disc in /dev/sr0.");
    let _ = writeln!(t);
    let _ = writeln!(t, "{}. Disc reads normally: mount and copy", num());
    let _ = writeln!(t, "   mount -o ro /dev/sr0 /mnt");
    for p in payloads {
        let flag = if p.is_dir { "-r " } else { "" };
        let _ = writeln!(t, "   cp {flag}/mnt/{} .", p.name);
    }
    let _ = writeln!(t, "   sha256sum -c --ignore-missing /mnt/checksums.sha256");
    let _ = writeln!(
        t,
        "   Run in the directory holding the copies; --ignore-missing skips the"
    );
    let _ = writeln!(t, "   parity/ entries you did not copy.");
    let _ = writeln!(t);
    let _ = writeln!(
        t,
        "{}. Disc has read errors: image everything readable first (GNU ddrescue)",
        num()
    );
    let _ = writeln!(t, "   ddrescue /dev/sr0 recovered.iso rescue.map");
    let _ = writeln!(
        t,
        "   ddrescue -r3 /dev/sr0 recovered.iso rescue.map   # extra retry passes"
    );
    let _ = writeln!(
        t,
        "   Unreadable sectors stay zero-filled and the image keeps its full"
    );
    let _ = writeln!(t, "   length - exactly what the repair steps below expect.");
    if ecc {
        let _ = writeln!(t);
        let _ = writeln!(
            t,
            "{}. Repair sector damage with the embedded ECC (dvdisaster RS02)",
            num()
        );
        let _ = writeln!(
            t,
            "   The image carries a dvdisaster RS02 layer when free disc capacity"
        );
        let _ = writeln!(
            t,
            "   allowed at mastering time; it self-identifies. Run this BEFORE the"
        );
        let _ = writeln!(t, "   par2 step - it repairs raw sectors in the image:");
        let _ = writeln!(t, "   dvdisaster -i recovered.iso -t     # assess");
        let _ = writeln!(t, "   dvdisaster -i recovered.iso -f     # repair in place");
    }
    let _ = writeln!(t);
    let _ = writeln!(t, "{}. Extract files from the rescued image", num());
    let _ = writeln!(t, "   mount -o loop,ro recovered.iso /mnt");
    let _ = writeln!(t, "   or without root:");
    for p in payloads {
        let _ = writeln!(
            t,
            "   xorriso -osirrox on -indev recovered.iso -extract /{} {}",
            p.name, p.name
        );
    }
    let _ = writeln!(t);
    let _ = writeln!(
        t,
        "{}. Repair a damaged payload with the parity on this disc",
        num()
    );
    let _ = writeln!(
        t,
        "   With the disc or image mounted at /mnt and the damaged copy in the"
    );
    let _ = writeln!(t, "   current directory:");
    for p in payloads {
        // dir sets store member rel paths; a trailing operand must be a file
        if p.is_dir {
            let _ = writeln!(t, "   par2 r -B. /mnt/parity/{}.par2", p.name);
        } else {
            let _ = writeln!(t, "   par2 r -B. /mnt/parity/{}.par2 {}", p.name, p.name);
        }
    }
    let _ = writeln!(
        t,
        "   -B. makes par2 repair the copy here; without it par2 uses the"
    );
    let _ = writeln!(
        t,
        "   read-only /mnt/parity/ as its base and ignores the damaged copy."
    );
    let _ = writeln!(t);
    let _ = writeln!(
        t,
        "{}. Map damaged sectors to files (regenerate the file->LBA table)",
        num()
    );
    let _ = writeln!(t, "   xorriso -indev /dev/sr0 -find / -exec report_lba --");
    let _ = writeln!(
        t,
        "   Works against the rescued image too: xorriso -indev recovered.iso ..."
    );
    let containers: Vec<&str> = payloads
        .iter()
        .flat_map(|p| p.files.iter())
        .filter(|m| m.container)
        .map(|m| m.rel.as_str())
        .collect();
    if !containers.is_empty() {
        let _ = writeln!(t);
        let _ = writeln!(t, "{}. VeraCrypt containers", num());
        let _ = writeln!(
            t,
            "   Containers mount read-only straight from the mounted disc:"
        );
        for rel in containers {
            let _ = writeln!(t, "   veracrypt --text --mount-options ro /mnt/{rel}");
        }
        let _ = writeln!(
            t,
            "   If the volume header is damaged, restore it from your EXTERNAL header"
        );
        let _ = writeln!(
            t,
            "   backup: VeraCrypt > Tools > Restore Volume Header. The embedded backup"
        );
        let _ = writeln!(
            t,
            "   header at the end of the container is the first fallback."
        );
    }
    let _ = writeln!(t);
    let _ = writeln!(
        t,
        "Prevention: re-verify this disc every 6-12 months (no consensus interval"
    );
    let _ = writeln!(
        t,
        "exists; the Digital Preservation Coalition's 6-month fixity baseline is"
    );
    let _ = writeln!(
        t,
        "the reference). Decay caught while the parity above is still readable is"
    );
    let _ = writeln!(t, "decay these files can repair.");
    crate::fsutil::write_durable(out, t)
}

/// ISO 9660 truncation self-check (isolyzer-inspired, native): the Primary
/// Volume Descriptor at sector 16 declares the volume size; a staged image
/// shorter than its own declaration is truncated (torn copy, full disk,
/// interrupted master) and must never reach a disc. Bytes past the declared
/// size are legitimate - sector run-out or appended ECC data.
pub fn check_iso_truncation(iso: &Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(iso).with_context(|| format!("open {}", iso.display()))?;
    let actual = f
        .metadata()
        .with_context(|| format!("stat {}", iso.display()))?
        .len();
    let mut pvd = [0u8; 2048];
    if f.seek(SeekFrom::Start(32768))
        .and_then(|_| f.read_exact(&mut pvd))
        .is_err()
    {
        bail!(
            "{}: too short ({actual} bytes) to hold an ISO 9660 volume \
             descriptor - truncated image? re-master before burning",
            iso.display()
        );
    }
    let expected = pvd_declared_bytes(&pvd).with_context(|| iso.display().to_string())?;
    ensure!(
        actual >= expected,
        "ISO TRUNCATED - DO NOT BURN: {} is {actual} bytes but its own volume \
         descriptor declares {expected}; re-master the image",
        iso.display()
    );
    Ok(())
}

/// Pure: declared volume bytes from the PVD sector (both-endian fields; the
/// little-endian halves at offsets 80 and 128 suffice on every platform).
fn pvd_declared_bytes(pvd: &[u8; 2048]) -> Result<u64> {
    ensure!(
        pvd[0] == 1 && &pvd[1..6] == b"CD001",
        "no ISO 9660 Primary Volume Descriptor at sector 16"
    );
    let blocks = u32::from_le_bytes(pvd[80..84].try_into().unwrap()) as u64;
    let block_size = u16::from_le_bytes(pvd[128..130].try_into().unwrap()) as u64;
    ensure!(
        blocks > 0 && block_size > 0,
        "volume descriptor declares a zero-size volume"
    );
    Ok(blocks * block_size)
}

/// After mastering: `xorriso -indev <iso> -find / -exec report_lba --`,
/// saved next to the ISO (kept OFF-disc; RECOVERY.txt explains regeneration).
pub fn report_lba(tools: &Tools, iso: &Path, out: &Path) -> Result<()> {
    let mut args: Vec<String> = vec!["-indev".into(), iso.display().to_string()];
    args.extend(["-find", "/", "-exec", "report_lba", "--"].map(String::from));
    let output =
        crate::proc::output_deadline(&tools.xorriso, &args, crate::proc::SHORT_OP_DEADLINE)?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = err.lines().rev().take(8).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!(
            "xorriso report_lba failed ({}):\n{}",
            output.status,
            tail.join("\n")
        );
    }
    crate::fsutil::write_durable(out, &output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Progress-line samples: mankier.com/1/xorrisofs, real run at
    // blog.linux-ng.de/2025/01/02/build-unattended-windows-iso/ (xorriso 1.5.6)
    const MKISOFS_LOG: &str = include_str!("../tests/fixtures/xorriso_mkisofs_progress.txt");

    fn payload(path: &str, size: u64) -> Payload {
        let name = Path::new(path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        Payload {
            root: path.into(),
            is_dir: false,
            files: vec![plan::PayloadMember {
                abs: path.into(),
                rel: name.clone(),
                size,
                container: true,
            }],
            dirs: 0,
            total_size: size,
            name,
        }
    }

    fn dir_payload(path: &str, members: &[(&str, u64, bool)]) -> Payload {
        let name = Path::new(path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        Payload {
            root: path.into(),
            is_dir: true,
            files: members
                .iter()
                .map(|(rel, size, container)| plan::PayloadMember {
                    abs: format!("{path}/{rel}").into(),
                    rel: rel.to_string(),
                    size: *size,
                    container: *container,
                })
                .collect(),
            dirs: 1,
            total_size: members.iter().map(|(_, s, _)| s).sum(),
            name,
        }
    }

    fn sample_input<'a>(payloads: &'a [Payload], parity: &'a [PathBuf]) -> MasterInput<'a> {
        MasterInput {
            label: "VAULT_20260717",
            payloads,
            parity_files: parity,
            checksums: Path::new("/stage/checksums.sha256"),
            manifest: Path::new("/stage/MANIFEST.txt"),
            recovery: Path::new("/stage/RECOVERY.txt"),
            out_iso: Path::new("/stage/VAULT_20260717.iso"),
        }
    }

    #[test]
    fn master_args_match_design_contract() {
        let payloads = vec![payload("/home/u/vault.hc", 100)];
        let parity = vec![
            PathBuf::from("/stage/parity/vault.hc.par2"),
            PathBuf::from("/stage/parity/vault.hc.vol000+91.par2"),
        ];
        let args = master_args(&sample_input(&payloads, &parity));
        assert_eq!(
            args,
            vec![
                "-as",
                "mkisofs",
                "-iso-level",
                "3",
                "-rock",
                "--md5",
                "-V",
                "VAULT_20260717",
                "-o",
                "/stage/VAULT_20260717.iso",
                "-graft-points",
                "/vault.hc=/home/u/vault.hc",
                "/parity/vault.hc.par2=/stage/parity/vault.hc.par2",
                "/parity/vault.hc.vol000+91.par2=/stage/parity/vault.hc.vol000+91.par2",
                "/checksums.sha256=/stage/checksums.sha256",
                "/MANIFEST.txt=/stage/MANIFEST.txt",
                "/RECOVERY.txt=/stage/RECOVERY.txt",
            ]
        );
    }

    #[test]
    fn master_args_graft_directory_roots() {
        let payloads = vec![dir_payload(
            "/home/u/extras",
            &[("extras/a.bin", 10, false)],
        )];
        let args = master_args(&sample_input(&payloads, &[]));
        assert!(
            args.contains(&"/extras=/home/u/extras".to_string()),
            "{args:?}"
        );
    }

    #[test]
    fn graft_escapes_iso_side_only() {
        assert_eq!(graft("/a=b", Path::new("/tmp/x=y")), "/a\\=b=/tmp/x=y");
        assert_eq!(graft("/a\\b", Path::new("/t")), "/a\\\\b=/t");
    }

    #[test]
    fn progress_lines_from_capture() {
        let got: Vec<f32> = MKISOFS_LOG
            .lines()
            .filter_map(parse_progress_line)
            .collect();
        assert_eq!(got, vec![0.52, 26.11, 52.22, 99.05]);
        for line in MKISOFS_LOG.lines().filter(|l| !l.contains("% done")) {
            assert_eq!(parse_progress_line(line), None, "{line}");
        }
    }

    #[test]
    fn progress_accepts_bare_mkisofs_format() {
        assert_eq!(
            parse_progress_line(" 12.34% done, estimate finish Tue Jul 15 12:00:00 2014"),
            Some(12.34)
        );
        assert_eq!(parse_progress_line("100.59% done"), Some(100.0));
    }

    #[test]
    fn progress_rejects_other_percent_lines() {
        assert_eq!(
            parse_progress_line("xorriso : UPDATE : Writing:    20208s   38.2%   fifo  52%"),
            None
        );
        assert_eq!(
            parse_progress_line("xorriso : UPDATE : Blanking  ( 1.0% done in 2 seconds )"),
            None
        );
        assert_eq!(parse_progress_line("850.0% done"), None);
        assert_eq!(parse_progress_line(""), None);
    }

    #[test]
    fn manifest_records_parameters_and_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("MANIFEST.txt");
        let payloads = vec![ManifestEntry {
            name: "vault.hc".into(),
            bytes: 20 * 1024 * 1024 * 1024,
            files: 1,
            is_dir: false,
            sha256: Some("ab".repeat(32)),
            par2: Some((655_360, 32_768)),
        }];
        write_manifest(&out, "VAULT_20260717", &payloads, Some(15), false, false).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(
            !text.contains("ovenmitts"),
            "on-disc files carry no tool branding: {text}"
        );
        assert!(text.contains("mastered and burned: xorriso"), "{text}");
        assert!(text.contains("volume label: VAULT_20260717"));
        assert!(text.contains("par2, 15% redundancy"));
        assert!(text.contains("defect management: off (stream recording)"));
        assert!(text.contains("vault.hc"));
        assert!(text.contains("bytes: 21474836480 (20.00 GiB)"));
        assert!(text.contains(&format!("sha256: {}", "ab".repeat(32))));
        assert!(text.contains("par2 slice: 655360 bytes"));
        assert!(text.contains("created: 20"));
        assert!(text.contains(" UTC"));
    }

    #[test]
    fn manifest_without_parity_and_with_dm() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("MANIFEST.txt");
        let entry = ManifestEntry {
            name: "v".into(),
            bytes: 10,
            files: 1,
            is_dir: false,
            sha256: Some("cd".repeat(32)),
            par2: None,
        };
        write_manifest(&out, "L", &[entry], None, true, false).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("parity: none"));
        assert!(text.contains("defect management: on"));
        assert!(!text.contains("par2 slice"));
    }

    #[test]
    fn manifest_dir_entry_points_at_checksums() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("MANIFEST.txt");
        let entry = ManifestEntry {
            name: "extras".into(),
            bytes: 3_000_000,
            files: 42,
            is_dir: true,
            sha256: None,
            par2: Some((65_536, 47)),
        };
        write_manifest(&out, "L", &[entry], Some(15), false, true).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("  extras/\n"), "{text}");
        assert!(text.contains("files: 42"), "{text}");
        assert!(
            text.contains("sha256: per-file hashes in checksums.sha256"),
            "{text}"
        );
        assert!(
            text.contains("par2 slice: 65536 bytes (47 blocks)"),
            "{text}"
        );
    }

    #[test]
    fn recovery_contains_exact_commands_per_payload() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("RECOVERY.txt");
        let payloads = vec![payload("/h/vault.hc", 10), payload("/h/notes.hc", 5)];
        write_recovery(&out, "VAULT_20260717", &payloads, true).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("disc \"VAULT_20260717\""));
        assert!(text.contains("mount -o ro /dev/sr0 /mnt"));
        assert!(text.contains("sha256sum -c --ignore-missing /mnt/checksums.sha256"));
        assert!(text.contains("ddrescue /dev/sr0 recovered.iso rescue.map"));
        assert!(text.contains("mount -o loop,ro recovered.iso /mnt"));
        for n in ["vault.hc", "notes.hc"] {
            assert!(text.contains(&format!("cp /mnt/{n} .")));
            assert!(text.contains(&format!(
                "xorriso -osirrox on -indev recovered.iso -extract /{n} {n}"
            )));
            assert!(text.contains(&format!("par2 r -B. /mnt/parity/{n}.par2 {n}")));
            assert!(text.contains(&format!("veracrypt --text --mount-options ro /mnt/{n}")));
        }
        assert!(text.contains("xorriso -indev /dev/sr0 -find / -exec report_lba --"));
        assert!(text.contains("Restore Volume Header"));
        assert!(text.contains("every 6-12 months"), "{text}");
        assert!(text.contains("fixity"), "{text}");
        assert!(
            !text.contains("ovenmitts"),
            "on-disc files carry no tool branding: {text}"
        );
        assert!(text.contains("Mastered and burned with xorriso"), "{text}");
    }

    #[test]
    fn recovery_dir_payload_commands() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("RECOVERY.txt");
        let payloads = vec![
            dir_payload(
                "/h/extras",
                &[
                    ("extras/a.bin", 10, false),
                    ("extras/sub/inner.hc", 20, true),
                ],
            ),
            payload("/h/vault.hc", 10),
        ];
        write_recovery(&out, "L", &payloads, false).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("cp -r /mnt/extras ."), "{text}");
        assert!(
            text.lines()
                .any(|l| l.trim() == "par2 r -B. /mnt/parity/extras.par2"),
            "{text}"
        );
        assert!(
            text.contains("veracrypt --text --mount-options ro /mnt/extras/sub/inner.hc"),
            "{text}"
        );
        assert!(
            !text.contains("veracrypt --text --mount-options ro /mnt/extras\n"),
            "{text}"
        );
    }

    #[test]
    fn recovery_omits_veracrypt_section_without_containers() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("RECOVERY.txt");
        let payloads = vec![dir_payload("/h/extras", &[("extras/a.bin", 10, false)])];
        write_recovery(&out, "L", &payloads, false).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(!text.contains("veracrypt"), "{text}");
        assert!(!text.contains("VeraCrypt containers"), "{text}");
    }

    fn fake_tools(script: &str, dir: &Path) -> Tools {
        use std::os::unix::fs::PermissionsExt;
        let fake = dir.join("xorriso");
        std::fs::write(&fake, script).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        Tools::bare(fake)
    }

    #[test]
    fn build_iso_streams_progress_and_returns_size() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("out.iso");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > {argv}\n\
             printf 'xorriso : UPDATE :  0.52%% done\\n' >&2\n\
             printf 'xorriso : UPDATE : 52.22%% done, estimate finish Thu Jan 02 15:33:35 2025\\n' >&2\n\
             printf 'ISO image produced: 10424 sectors\\n' >&2\n\
             printf 'iso-bytes-here' > {iso}\n",
            argv = dir.path().join("argv.txt").display(),
            iso = iso.display(),
        );
        let tools = fake_tools(&script, dir.path());
        let payloads = vec![payload("/home/u/vault.hc", 100)];
        let input = MasterInput {
            label: "L",
            payloads: &payloads,
            parity_files: &[],
            checksums: Path::new("/s/checksums.sha256"),
            manifest: Path::new("/s/MANIFEST.txt"),
            recovery: Path::new("/s/RECOVERY.txt"),
            out_iso: &iso,
        };
        let mut events = Vec::new();
        let size = build_iso(&tools, &input, std::time::Duration::ZERO, &mut |p, l| {
            events.push((p, l))
        })
        .unwrap();
        assert_eq!(size, "iso-bytes-here".len() as u64);
        assert!(events.iter().any(|(p, _)| *p == Some(0.52)));
        assert!(events.iter().any(|(p, _)| *p == Some(52.22)));
        assert!(events
            .iter()
            .any(|(p, l)| p.is_none() && l.contains("ISO image produced")));
        let argv = std::fs::read_to_string(dir.path().join("argv.txt")).unwrap();
        let lines: Vec<&str> = argv.lines().collect();
        assert_eq!(&lines[..2], &["-as", "mkisofs"]);
        assert!(lines.contains(&"-graft-points"));
        assert!(lines.contains(&"/vault.hc=/home/u/vault.hc"));
    }

    #[test]
    fn build_iso_surfaces_stderr_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let script = "#!/bin/sh\n\
                      echo 'xorriso : FAILURE : Cannot find source' >&2\n\
                      exit 32\n";
        let tools = fake_tools(script, dir.path());
        let payloads = vec![payload("/x", 1)];
        let input = MasterInput {
            label: "L",
            payloads: &payloads,
            parity_files: &[],
            checksums: Path::new("/c"),
            manifest: Path::new("/m"),
            recovery: Path::new("/r"),
            out_iso: &dir.path().join("never.iso"),
        };
        let err = build_iso(&tools, &input, std::time::Duration::ZERO, &mut |_, _| {}).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Cannot find source"), "{msg}");
    }

    // Minimal valid ISO 9660 shell: 16 zero sectors, a PVD declaring the
    // exact block count, then `data_sectors` of payload.
    fn synthetic_iso(data_sectors: u32) -> Vec<u8> {
        let total_blocks = 16 + 1 + data_sectors;
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
        v.extend(std::iter::repeat(7u8).take(data_sectors as usize * 2048));
        v
    }

    #[test]
    fn truncation_check_accepts_intact_and_padded_images() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("ok.iso");
        std::fs::write(&iso, synthetic_iso(4)).unwrap();
        check_iso_truncation(&iso).unwrap();
        // run-out or appended ECC beyond the declared size is legitimate
        let mut padded = synthetic_iso(4);
        padded.extend_from_slice(&[0u8; 4096]);
        std::fs::write(&iso, padded).unwrap();
        check_iso_truncation(&iso).unwrap();
    }

    #[test]
    fn truncation_check_flags_short_image() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("cut.iso");
        let mut bytes = synthetic_iso(4);
        bytes.truncate(bytes.len() - 2048);
        std::fs::write(&iso, bytes).unwrap();
        let err = check_iso_truncation(&iso).unwrap_err();
        assert!(err.to_string().contains("TRUNCATED"), "{err:#}");
        assert!(err.to_string().contains("DO NOT BURN"), "{err:#}");
    }

    #[test]
    fn truncation_check_flags_missing_or_garbage_pvd() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("no-pvd.iso");
        // long enough to read sector 16, but no CD001 signature
        std::fs::write(&iso, vec![0u8; 40 * 2048]).unwrap();
        let err = check_iso_truncation(&iso).unwrap_err();
        assert!(
            format!("{err:#}").contains("Primary Volume Descriptor"),
            "{err:#}"
        );
        // shorter than the descriptor offset entirely
        std::fs::write(&iso, b"tiny").unwrap();
        let err = check_iso_truncation(&iso).unwrap_err();
        assert!(err.to_string().contains("too short"), "{err:#}");
    }

    #[test]
    fn report_lba_writes_stdout_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > {argv}\n\
             printf 'Report layout: xt , Startlba ,   Blocks , Filesize , ISO image path\\n'\n\
             printf \"File data lba:  0 ,       32 ,    10240 , 20971520 , '/vault.hc'\\n\"\n",
            argv = dir.path().join("argv.txt").display(),
        );
        let tools = fake_tools(&script, dir.path());
        let out = dir.path().join("lba.txt");
        report_lba(&tools, Path::new("/stage/x.iso"), &out).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("File data lba:"));
        assert!(text.contains("'/vault.hc'"));
        let argv = std::fs::read_to_string(dir.path().join("argv.txt")).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            vec![
                "-indev",
                "/stage/x.iso",
                "-find",
                "/",
                "-exec",
                "report_lba",
                "--"
            ]
        );
    }

    #[test]
    fn report_lba_fails_with_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let script = "#!/bin/sh\necho 'no such image' >&2\nexit 2\n";
        let tools = fake_tools(script, dir.path());
        let err = report_lba(&tools, Path::new("/x.iso"), &dir.path().join("o")).unwrap_err();
        assert!(err.to_string().contains("no such image"));
    }
}
