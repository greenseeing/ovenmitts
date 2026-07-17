use std::path::{Path, PathBuf};

pub const BD_R_25: u64 = 25_025_314_816;
pub const BD_R_50: u64 = 50_050_629_632;
pub const BDXL_100: u64 = 100_103_356_416;
pub const BD_R_XL_128: u64 = 128_001_769_472;
pub const DVD_R: u64 = 4_707_319_808;
pub const DVD_PLUS_R: u64 = 4_700_372_992;

pub const PAR2_MAX_BLOCKS: u64 = 32_768;
pub const SECTOR: u64 = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    BdR25,
    BdR50,
    Bdxl100,
    BdRXl128,
    DvdR,
    DvdPlusR,
    Unknown,
}

impl MediaKind {
    pub fn label(&self) -> &'static str {
        match self {
            MediaKind::BdR25 => "BD-R 25 GB (single layer)",
            MediaKind::BdR50 => "BD-R 50 GB (dual layer)",
            MediaKind::Bdxl100 => "BDXL 100 GB (triple layer)",
            MediaKind::BdRXl128 => "BD-R XL 128 GB (quad layer)",
            MediaKind::DvdR => "DVD-R 4.7 GB",
            MediaKind::DvdPlusR => "DVD+R 4.7 GB",
            MediaKind::Unknown => "unknown media",
        }
    }

    pub fn nominal_bytes(&self) -> Option<u64> {
        match self {
            MediaKind::BdR25 => Some(BD_R_25),
            MediaKind::BdR50 => Some(BD_R_50),
            MediaKind::Bdxl100 => Some(BDXL_100),
            MediaKind::BdRXl128 => Some(BD_R_XL_128),
            MediaKind::DvdR => Some(DVD_R),
            MediaKind::DvdPlusR => Some(DVD_PLUS_R),
            MediaKind::Unknown => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub kind: MediaKind,
    pub profile: String,
    pub blank: bool,
    pub formatted: bool,
    pub free_bytes: u64,
    pub formatted_capacity: Option<u64>,
    pub speeds: Vec<f32>,
    pub media_id: Option<String>,
}

pub const PAR2_MAX_FILES: u64 = 32_768;
// Linux argv budget is ~2 MB; leave headroom for the fixed par2 flags
const PAR2_OPERAND_BYTES_MAX: usize = 1_500_000;

#[derive(Debug, Clone)]
pub struct PayloadMember {
    pub abs: PathBuf,
    /// Disc path and par2 operand: "vault.hc" or "extras/sub/a.bin".
    pub rel: String,
    pub size: u64,
    pub container: bool,
}

#[derive(Debug, Clone)]
pub struct Payload {
    /// Top-level disc name (the disc root is flat).
    pub name: String,
    pub root: PathBuf,
    pub is_dir: bool,
    /// Regular files only, deterministic walk order; exactly one entry
    /// (rel == name) for a file payload.
    pub files: Vec<PayloadMember>,
    /// Directory count incl. the root for dir payloads (ISO overhead math).
    pub dirs: usize,
    pub total_size: u64,
}

impl Payload {
    pub fn inspect(path: PathBuf) -> anyhow::Result<(Self, Vec<String>)> {
        let root = std::fs::canonicalize(&path)
            .map_err(|e| anyhow::anyhow!("cannot read payload {}: {e}", path.display()))?;
        let name = match root.file_name() {
            None => anyhow::bail!("payload has no usable name: {}", path.display()),
            Some(n) => n
                .to_str()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "payload name is not valid UTF-8 - rename it: {}",
                        root.display()
                    )
                })?
                .to_string(),
        };
        anyhow::ensure!(
            !name.starts_with('-'),
            "payload name starts with '-' (par2 would read it as a flag) - rename it: {name}"
        );
        let meta = std::fs::metadata(&root)
            .map_err(|e| anyhow::anyhow!("cannot read payload {}: {e}", root.display()))?;
        let mut warnings = Vec::new();
        let (files, dirs) = if meta.is_dir() {
            let mut files = Vec::new();
            let mut dirs = 1usize;
            walk_dir(&root, &name, &mut files, &mut dirs, &mut warnings)?;
            anyhow::ensure!(
                !files.is_empty(),
                "payload directory has no files: {}",
                root.display()
            );
            (files, dirs)
        } else if meta.is_file() {
            let member = PayloadMember {
                abs: root.clone(),
                rel: name.clone(),
                size: meta.len(),
                container: container_heuristic(&root, meta.len()),
            };
            (vec![member], 0)
        } else {
            anyhow::bail!(
                "payload is not a regular file or directory: {}",
                root.display()
            );
        };
        ensure_par2_limits(&name, &files)?;
        for m in files.iter().filter(|m| m.size == 0) {
            warnings.push(format!(
                "{}: empty file - archived and checksummed, but excluded from \
                 parity (par2 cannot repair 0-byte files)",
                m.rel
            ));
        }
        let total_size = files.iter().map(|m| m.size).sum();
        Ok((
            Self {
                name,
                is_dir: meta.is_dir(),
                files,
                dirs,
                total_size,
                root,
            },
            warnings,
        ))
    }

    pub fn looks_like_container(&self) -> bool {
        self.files.iter().any(|m| m.container)
    }

    /// par2 source operands, relative to the payload root's parent (the
    /// basepath). 0-byte files are excluded: par2 cannot repair them.
    pub fn parity_operands(&self) -> Vec<&str> {
        self.files
            .iter()
            .filter(|m| m.size > 0)
            .map(|m| m.rel.as_str())
            .collect()
    }

    pub fn slice_bytes(&self) -> u64 {
        slice_bytes_for(self.total_size, self.parity_operands().len() as u64)
    }

    /// True block count of the recovery set: every member's tail slice
    /// rounds up, so this is not simply total_size / slice.
    pub fn parity_blocks(&self) -> u64 {
        let slice = self.slice_bytes();
        self.files.iter().map(|m| m.size.div_ceil(slice)).sum()
    }
}

pub(crate) fn container_heuristic(path: &Path, size: u64) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    // VeraCrypt containers: .hc/.tc, or big extensionless files (ciphertext blobs)
    matches!(ext, "hc" | "tc") || (ext.is_empty() && size >= 64 * 1024 * 1024)
}

fn walk_dir(
    dir: &Path,
    rel_prefix: &str,
    files: &mut Vec<PayloadMember>,
    dirs: &mut usize,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("cannot read payload directory {}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("cannot read payload directory {}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        // file_type does not follow symlinks: a link is a link, not its target
        let ft = entry
            .file_type()
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        let fname = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => anyhow::bail!(
                "file name is not valid UTF-8 - rename it: {}",
                path.display()
            ),
        };
        let rel = format!("{rel_prefix}/{fname}");
        if ft.is_dir() {
            *dirs += 1;
            walk_dir(&path, &rel, files, dirs, warnings)?;
        } else if ft.is_file() {
            let size = entry
                .metadata()
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?
                .len();
            files.push(PayloadMember {
                container: container_heuristic(&path, size),
                abs: path,
                rel,
                size,
            });
        } else {
            warnings.push(format!(
                "{}: not a regular file - kept on the ISO as-is but excluded \
                 from checksums and parity",
                path.display()
            ));
        }
    }
    Ok(())
}

fn ensure_par2_limits(name: &str, files: &[PayloadMember]) -> anyhow::Result<()> {
    let nonempty: Vec<&PayloadMember> = files.iter().filter(|m| m.size > 0).collect();
    anyhow::ensure!(
        nonempty.len() as u64 <= PAR2_MAX_FILES,
        "payload {name} has {} files - one par2 set holds at most {PAR2_MAX_FILES}; \
         tar the directory first",
        nonempty.len()
    );
    let operand_bytes: usize = nonempty.iter().map(|m| m.rel.len() + 1).sum();
    anyhow::ensure!(
        operand_bytes <= PAR2_OPERAND_BYTES_MAX,
        "payload {name}: file paths exceed the par2 command-line budget; \
         tar the directory first"
    );
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PlanInput {
    pub payloads: Vec<Payload>,
    pub parity: bool,
    pub redundancy_pct: u32,
    pub headroom_pct: u32,
    pub defect_management: bool,
}

#[derive(Debug, Clone)]
pub struct ArchivePlan {
    pub payload_bytes: u64,
    pub parity_bytes_est: u64,
    pub overhead_bytes_est: u64,
    pub total_bytes_est: u64,
    pub capacity: u64,
    pub budget: u64,
    pub fits: bool,
    pub warnings: Vec<String>,
}

// Slice sizing is the recoverability lever (research finding #4): target the
// PAR2 32768-block ceiling so scattered sector damage consumes as few
// recovery blocks as possible. Slice must be a multiple of 4. Every file's
// tail slice can waste up to one block, so the budget shrinks by one block
// per extra file in the set.
pub fn slice_bytes_for(total: u64, nonempty_files: u64) -> u64 {
    let budget = PAR2_MAX_BLOCKS
        .saturating_sub(nonempty_files.saturating_sub(1))
        .max(1);
    let raw = total.div_ceil(budget);
    let aligned = raw.div_ceil(4) * 4;
    aligned.max(64 * 1024)
}

pub fn estimate_parity(payload: &Payload, redundancy_pct: u32) -> u64 {
    let slice = payload.slice_bytes();
    let blocks: u64 = payload.files.iter().map(|m| m.size.div_ceil(slice)).sum();
    let recovery_blocks = (blocks * redundancy_pct as u64).div_ceil(100);
    // per-recovery-block packet overhead ~68 bytes + index file ~= blocks * 100
    // + per-file FileDesc/IFSC packets ~512 bytes
    recovery_blocks * slice
        + recovery_blocks * 68
        + blocks * 100
        + payload.files.len() as u64 * 512
        + 1024 * 1024
}

pub fn build_plan(input: &PlanInput, media: &MediaInfo) -> ArchivePlan {
    let payload_bytes: u64 = input.payloads.iter().map(|p| p.total_size).sum();
    let parity_bytes_est = if input.parity {
        input
            .payloads
            .iter()
            .map(|p| estimate_parity(p, input.redundancy_pct))
            .sum()
    } else {
        0
    };
    // ISO structures, Rock Ridge, MD5 tags, manifest/recovery text: generous flat pad
    let overhead_bytes_est = 16 * 1024 * 1024
        + input
            .payloads
            .iter()
            .map(|p| (p.files.len() + p.dirs) as u64)
            .sum::<u64>()
            * SECTOR;
    let total_bytes_est = payload_bytes + parity_bytes_est + overhead_bytes_est;

    let capacity = if input.defect_management {
        media.formatted_capacity.unwrap_or(media.free_bytes)
    } else {
        media.free_bytes
    };
    let budget = capacity.saturating_sub(capacity.saturating_mul(input.headroom_pct as u64) / 100);
    let fits = total_bytes_est <= budget;

    let mut warnings = Vec::new();
    if matches!(
        media.kind,
        MediaKind::Bdxl100 | MediaKind::BdRXl128 | MediaKind::BdR50
    ) {
        warnings.push(
            "multi-layer media: 25 GB single-layer discs burn and age more reliably \
             and are readable in every BD drive tier"
                .into(),
        );
    }
    if matches!(media.kind, MediaKind::DvdR | MediaKind::DvdPlusR) {
        warnings.push(
            "DVD±R uses organic dye (5-15 year realistic lifespan in heat/humidity); \
             treat as expiring media, re-verify yearly"
                .into(),
        );
    }
    if input.payloads.iter().any(|p| p.looks_like_container()) {
        warnings.push(
            "VeraCrypt payload detected: keep an external volume-header backup \
             (Tools > Backup Volume Header), and create a FRESH container per \
             archive generation - never re-burn diverged copies of one container"
                .into(),
        );
        for m in input.payloads.iter().flat_map(|p| p.files.iter()) {
            if m.container && m.size % SECTOR != 0 {
                warnings.push(format!(
                    "{}: size is not a multiple of 2048; resize the container to a \
                     whole MiB to keep every ISO extent sector-aligned",
                    m.abs.display()
                ));
            }
        }
    }
    if input.parity && input.redundancy_pct < 10 {
        warnings.push("parity below 10% leaves little margin for rim/cluster damage".into());
    }

    ArchivePlan {
        payload_bytes,
        parity_bytes_est,
        overhead_bytes_est,
        total_bytes_est,
        capacity,
        budget,
        fits,
        warnings,
    }
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(free: u64) -> MediaInfo {
        MediaInfo {
            kind: MediaKind::BdR25,
            profile: "BD-R".into(),
            blank: true,
            formatted: false,
            free_bytes: free,
            formatted_capacity: None,
            speeds: vec![],
            media_id: None,
        }
    }

    fn file_payload(name: &str, size: u64, container: bool) -> Payload {
        Payload {
            name: name.into(),
            root: name.into(),
            is_dir: false,
            files: vec![PayloadMember {
                abs: name.into(),
                rel: name.into(),
                size,
                container,
            }],
            dirs: 0,
            total_size: size,
        }
    }

    #[test]
    fn slice_targets_block_ceiling() {
        // 93 GiB container -> ~2.9 MiB slices, block count <= 32768
        let size = 100_000_000_000u64;
        let s = slice_bytes_for(size, 1);
        assert_eq!(s % 4, 0);
        assert!(size.div_ceil(s) <= PAR2_MAX_BLOCKS);
        // 20 GiB -> 640 KiB slices
        let s20 = slice_bytes_for(20 * 1024 * 1024 * 1024, 1);
        assert_eq!(s20, 655_360);
    }

    #[test]
    fn slice_floor_for_small_files() {
        assert_eq!(slice_bytes_for(10 * 1024 * 1024, 1), 64 * 1024);
    }

    #[test]
    fn slice_budget_accounts_for_file_count() {
        // n files of equal size: per-file tail waste must keep the set under
        // the 32768-block ceiling
        for n in [2u64, 100, 10_000, 32_768] {
            let each = 1024 * 1024;
            let total = n * each;
            let slice = slice_bytes_for(total, n);
            let blocks: u64 = (0..n).map(|_| each.div_ceil(slice)).sum();
            assert!(blocks <= PAR2_MAX_BLOCKS, "n={n} blocks={blocks}");
        }
        // single file is bit-identical to the historical sizing
        assert_eq!(slice_bytes_for(0, 0), 64 * 1024);
    }

    #[test]
    fn inspect_file_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.hc");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let (p, warns) = Payload::inspect(path).unwrap();
        assert!(!p.is_dir);
        assert_eq!(p.name, "vault.hc");
        assert_eq!(p.dirs, 0);
        assert_eq!(p.total_size, 4096);
        assert_eq!(p.files.len(), 1);
        assert_eq!(p.files[0].rel, "vault.hc");
        assert!(p.files[0].container);
        assert!(p.looks_like_container());
        assert_eq!(p.parity_operands(), vec!["vault.hc"]);
        assert!(warns.is_empty());
    }

    #[test]
    fn inspect_dir_walks_sorted_and_warns() {
        let dir = tempfile::tempdir().unwrap();
        let extras = dir.path().join("extras");
        std::fs::create_dir_all(extras.join("sub")).unwrap();
        std::fs::write(extras.join("b.bin"), b"bb").unwrap();
        std::fs::write(extras.join("a.bin"), b"aaaa").unwrap();
        std::fs::write(extras.join("empty.bin"), b"").unwrap();
        std::fs::write(extras.join("sub").join("c.bin"), b"c").unwrap();
        std::os::unix::fs::symlink("a.bin", extras.join("link_a")).unwrap();

        let (p, warns) = Payload::inspect(extras).unwrap();
        assert!(p.is_dir);
        assert_eq!(p.name, "extras");
        assert_eq!(p.dirs, 2);
        assert_eq!(p.total_size, 7);
        let rels: Vec<&str> = p.files.iter().map(|m| m.rel.as_str()).collect();
        assert_eq!(
            rels,
            vec![
                "extras/a.bin",
                "extras/b.bin",
                "extras/empty.bin",
                "extras/sub/c.bin"
            ]
        );
        assert_eq!(
            p.parity_operands(),
            vec!["extras/a.bin", "extras/b.bin", "extras/sub/c.bin"]
        );
        assert!(warns.iter().any(|w| w.contains("link_a")), "{warns:?}");
        assert!(
            warns.iter().any(|w| w.contains("extras/empty.bin")),
            "{warns:?}"
        );
    }

    #[test]
    fn inspect_follows_top_level_symlink_and_names_after_target() {
        let dir = tempfile::tempdir().unwrap();
        let extras = dir.path().join("extras");
        std::fs::create_dir(&extras).unwrap();
        std::fs::write(extras.join("a.bin"), b"x").unwrap();
        let link = dir.path().join("link-to-extras");
        std::os::unix::fs::symlink(&extras, &link).unwrap();
        // the CLI path itself follows (explicit user intent); disc name comes
        // from the canonical target
        let (p, _) = Payload::inspect(link).unwrap();
        assert!(p.is_dir);
        assert_eq!(p.name, "extras");
        assert_eq!(p.files[0].rel, "extras/a.bin");
    }

    #[test]
    fn inspect_skips_symlinked_subdir_without_recursing() {
        let dir = tempfile::tempdir().unwrap();
        let extras = dir.path().join("extras");
        std::fs::create_dir(&extras).unwrap();
        std::fs::write(extras.join("a.bin"), b"x").unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.bin"), b"s").unwrap();
        std::os::unix::fs::symlink(&outside, extras.join("escape")).unwrap();

        let (p, warns) = Payload::inspect(extras).unwrap();
        let rels: Vec<&str> = p.files.iter().map(|m| m.rel.as_str()).collect();
        assert_eq!(rels, vec!["extras/a.bin"]);
        assert!(warns.iter().any(|w| w.contains("escape")), "{warns:?}");
    }

    #[test]
    fn inspect_rejects_leading_dash_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("-flag.bin");
        std::fs::write(&path, b"x").unwrap();
        let err = Payload::inspect(path).unwrap_err();
        assert!(err.to_string().contains("starts with '-'"), "{err}");
    }

    #[test]
    fn parity_blocks_sums_per_member_ceilings() {
        let member = |rel: &str, size: u64| PayloadMember {
            abs: rel.into(),
            rel: rel.into(),
            size,
            container: false,
        };
        let p = Payload {
            name: "d".into(),
            root: "d".into(),
            is_dir: true,
            files: vec![member("d/a", 1), member("d/b", 1), member("d/c", 65_537)],
            dirs: 1,
            total_size: 65_539,
        };
        // slice 64 KiB: two 1-byte tails + one 2-block file = 4, not
        // total/slice = 2
        assert_eq!(p.slice_bytes(), 64 * 1024);
        assert_eq!(p.parity_blocks(), 4);
    }

    #[test]
    fn inspect_rejects_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let err = Payload::inspect(empty).unwrap_err();
        assert!(err.to_string().contains("no files"), "{err}");
    }

    #[test]
    fn inspect_rejects_non_utf8_member_name() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let extras = dir.path().join("extras");
        std::fs::create_dir(&extras).unwrap();
        std::fs::write(extras.join(OsStr::from_bytes(b"bad\xFF.bin")), b"x").unwrap();
        let err = Payload::inspect(extras).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn par2_limits_reject_pathological_payloads() {
        let member = |rel: &str| PayloadMember {
            abs: rel.into(),
            rel: rel.into(),
            size: 1,
            container: false,
        };
        let too_many: Vec<PayloadMember> = (0..=PAR2_MAX_FILES)
            .map(|i| member(&format!("d/{i}")))
            .collect();
        let err = ensure_par2_limits("d", &too_many).unwrap_err();
        assert!(err.to_string().contains("at most 32768"), "{err}");

        let long_paths: Vec<PayloadMember> = (0..8000)
            .map(|i| member(&format!("d/{}/{i}.bin", "x".repeat(200))))
            .collect();
        let err = ensure_par2_limits("d", &long_paths).unwrap_err();
        assert!(err.to_string().contains("command-line budget"), "{err}");
    }

    #[test]
    fn plan_fits_with_headroom() {
        let input = PlanInput {
            payloads: vec![file_payload("vault.hc", 19 * 1024 * 1024 * 1024, true)],
            parity: true,
            redundancy_pct: 15,
            headroom_pct: 5,
            defect_management: false,
        };
        let plan = build_plan(&input, &media(BD_R_25));
        assert!(
            plan.fits,
            "19 GiB + 15% parity must fit a BD-R 25 with 5% headroom"
        );
        assert!(plan.total_bytes_est > plan.payload_bytes);
        assert!(!plan.warnings.is_empty()); // container hygiene warning
    }

    #[test]
    fn plan_rejects_overfull() {
        let input = PlanInput {
            payloads: vec![file_payload("vault.hc", 23 * 1024 * 1024 * 1024, false)],
            parity: true,
            redundancy_pct: 15,
            headroom_pct: 5,
            defect_management: false,
        };
        assert!(!build_plan(&input, &media(BD_R_25)).fits);
    }

    #[test]
    fn plan_sums_dir_members_and_counts_overhead() {
        let dir_payload = Payload {
            name: "extras".into(),
            root: "extras".into(),
            is_dir: true,
            files: vec![
                PayloadMember {
                    abs: "extras/a".into(),
                    rel: "extras/a".into(),
                    size: 1000,
                    container: false,
                },
                PayloadMember {
                    abs: "extras/sub/b".into(),
                    rel: "extras/sub/b".into(),
                    size: 500,
                    container: false,
                },
            ],
            dirs: 2,
            total_size: 1500,
        };
        let input = PlanInput {
            payloads: vec![dir_payload, file_payload("v.hc", 2000, false)],
            parity: false,
            redundancy_pct: 15,
            headroom_pct: 0,
            defect_management: false,
        };
        let plan = build_plan(&input, &media(BD_R_25));
        assert_eq!(plan.payload_bytes, 3500);
        // 16 MiB pad + (2 files + 2 dirs) + (1 file + 0 dirs) sectors
        assert_eq!(plan.overhead_bytes_est, 16 * 1024 * 1024 + 5 * SECTOR);
    }

    #[test]
    fn dm_uses_formatted_capacity() {
        let mut m = media(BD_R_25);
        m.formatted_capacity = Some(24_220_008_448); // observed spare-area loss
        let input = PlanInput {
            payloads: vec![file_payload("v", 23_000_000_000, false)],
            parity: false,
            redundancy_pct: 15,
            headroom_pct: 0,
            defect_management: true,
        };
        let plan = build_plan(&input, &m);
        assert_eq!(plan.capacity, 24_220_008_448);
    }
}
