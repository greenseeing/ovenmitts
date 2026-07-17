use std::path::PathBuf;

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

#[derive(Debug, Clone)]
pub struct PayloadFile {
    pub path: PathBuf,
    pub size: u64,
    pub looks_like_container: bool,
}

impl PayloadFile {
    pub fn inspect(path: PathBuf) -> anyhow::Result<Self> {
        let meta = std::fs::metadata(&path)
            .map_err(|e| anyhow::anyhow!("cannot read payload {}: {e}", path.display()))?;
        anyhow::ensure!(
            meta.is_file(),
            "payload is not a regular file: {}",
            path.display()
        );
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        // VeraCrypt containers: .hc/.tc, or big extensionless files (ciphertext blobs)
        let looks_like_container =
            matches!(ext, "hc" | "tc") || (ext.is_empty() && meta.len() >= 64 * 1024 * 1024);
        Ok(Self {
            path,
            size: meta.len(),
            looks_like_container,
        })
    }
}

#[derive(Debug, Clone)]
pub struct PlanInput {
    pub payloads: Vec<PayloadFile>,
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
// recovery blocks as possible. Slice must be a multiple of 4.
pub fn slice_bytes_for(size: u64) -> u64 {
    let raw = size.div_ceil(PAR2_MAX_BLOCKS);
    let aligned = raw.div_ceil(4) * 4;
    aligned.max(64 * 1024)
}

pub fn estimate_parity(size: u64, redundancy_pct: u32) -> u64 {
    let slice = slice_bytes_for(size);
    let blocks = size.div_ceil(slice);
    let recovery_blocks = (blocks * redundancy_pct as u64).div_ceil(100);
    // per-recovery-block packet overhead ~68 bytes + index file ~= blocks * 100
    recovery_blocks * slice + recovery_blocks * 68 + blocks * 100 + 1024 * 1024
}

pub fn build_plan(input: &PlanInput, media: &MediaInfo) -> ArchivePlan {
    let payload_bytes: u64 = input.payloads.iter().map(|p| p.size).sum();
    let parity_bytes_est = if input.parity {
        input
            .payloads
            .iter()
            .map(|p| estimate_parity(p.size, input.redundancy_pct))
            .sum()
    } else {
        0
    };
    // ISO structures, Rock Ridge, MD5 tags, manifest/recovery text: generous flat pad
    let overhead_bytes_est = 16 * 1024 * 1024 + input.payloads.len() as u64 * SECTOR;
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
    if input.payloads.iter().any(|p| p.looks_like_container) {
        warnings.push(
            "VeraCrypt payload detected: keep an external volume-header backup \
             (Tools > Backup Volume Header), and create a FRESH container per \
             archive generation - never re-burn diverged copies of one container"
                .into(),
        );
        for p in &input.payloads {
            if p.looks_like_container && p.size % SECTOR != 0 {
                warnings.push(format!(
                    "{}: size is not a multiple of 2048; resize the container to a \
                     whole MiB to keep every ISO extent sector-aligned",
                    p.path.display()
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

    #[test]
    fn slice_targets_block_ceiling() {
        // 93 GiB container -> ~2.9 MiB slices, block count <= 32768
        let size = 100_000_000_000u64;
        let s = slice_bytes_for(size);
        assert_eq!(s % 4, 0);
        assert!(size.div_ceil(s) <= PAR2_MAX_BLOCKS);
        // 20 GiB -> 640 KiB slices
        let s20 = slice_bytes_for(20 * 1024 * 1024 * 1024);
        assert_eq!(s20, 655_360);
    }

    #[test]
    fn slice_floor_for_small_files() {
        assert_eq!(slice_bytes_for(10 * 1024 * 1024), 64 * 1024);
    }

    #[test]
    fn plan_fits_with_headroom() {
        let input = PlanInput {
            payloads: vec![PayloadFile {
                path: "vault.hc".into(),
                size: 19 * 1024 * 1024 * 1024,
                looks_like_container: true,
            }],
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
            payloads: vec![PayloadFile {
                path: "vault.hc".into(),
                size: 23 * 1024 * 1024 * 1024,
                looks_like_container: false,
            }],
            parity: true,
            redundancy_pct: 15,
            headroom_pct: 5,
            defect_management: false,
        };
        assert!(!build_plan(&input, &media(BD_R_25)).fits);
    }

    #[test]
    fn dm_uses_formatted_capacity() {
        let mut m = media(BD_R_25);
        m.formatted_capacity = Some(24_220_008_448); // observed spare-area loss
        let input = PlanInput {
            payloads: vec![PayloadFile {
                path: "v".into(),
                size: 23_000_000_000,
                looks_like_container: false,
            }],
            parity: false,
            redundancy_pct: 15,
            headroom_pct: 0,
            defect_management: true,
        };
        let plan = build_plan(&input, &m);
        assert_eq!(plan.capacity, 24_220_008_448);
    }
}
