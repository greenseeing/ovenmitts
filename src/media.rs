use anyhow::{bail, Context, Result};

use crate::plan::{MediaInfo, MediaKind, BDXL_100, BD_R_25, BD_R_50, BD_R_XL_128, DVD_R, SECTOR};
use crate::tools::Tools;

/// Probe the drive: run `xorriso -outdev <dev> -toc -list_formats -list_speeds`
/// and optionally `dvd+rw-mediainfo` for the media ID.
pub fn probe(tools: &Tools, device: &str) -> Result<MediaInfo> {
    let args: Vec<String> = ["-outdev", device, "-toc", "-list_formats", "-list_speeds"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let out = crate::proc::output_deadline(&tools.xorriso, &args, crate::proc::SHORT_OP_DEADLINE)
        .with_context(|| format!("cannot run {}", tools.xorriso.display()))?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut info =
        parse_xorriso_probe(&text).with_context(|| probe_failure_context(device, &text))?;
    if info.media_id.is_none() {
        if let Some(mediainfo) = &tools.mediainfo {
            if let Ok(mo) = crate::proc::output_deadline(
                mediainfo,
                &[device.to_string()],
                crate::proc::SHORT_OP_DEADLINE,
            ) {
                info.media_id = parse_media_id(&String::from_utf8_lossy(&mo.stdout));
            }
        }
    }
    Ok(info)
}

/// Optical drives on this host (/dev/sr*), in numeric order.
pub fn list_drives() -> Vec<String> {
    list_drives_in(std::path::Path::new("/dev"))
}

fn list_drives_in(dev_dir: &std::path::Path) -> Vec<String> {
    let mut drives: Vec<(u32, String)> = std::fs::read_dir(dev_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n: u32 = name.strip_prefix("sr")?.parse().ok()?;
            Some((n, e.path().display().to_string()))
        })
        .collect();
    drives.sort();
    drives.into_iter().map(|(_, path)| path).collect()
}

fn probe_failure_context(device: &str, text: &str) -> String {
    let diag = text
        .lines()
        .rfind(|l| l.contains(" FAILURE : ") || l.contains(" SORRY : "))
        .map(str::trim);
    match diag {
        Some(d) => format!("probing {device}: {d}"),
        None => format!("probing {device}: unrecognized xorriso output"),
    }
}

/// Pure parser for combined xorriso -toc -list_formats -list_speeds output.
/// Extracts profile, blank/appendable status, formatted state, free bytes,
/// formatted capacity (from format descriptors), write speeds.
pub fn parse_xorriso_probe(out: &str) -> Result<MediaInfo> {
    let mut profile: Option<String> = None;
    let mut unrecognizable = false;
    let mut status: Option<String> = None;
    let mut product: Option<String> = None;
    let mut blocks_free: Option<u64> = None;
    let mut summary_free: Option<u64> = None;
    let mut format_status: Option<String> = None;
    let mut format_capacity: Option<u64> = None;
    let mut idx0_capacity: Option<u64> = None;
    let mut has_spare_area = false;
    let mut listed_speeds: Vec<f32> = Vec::new();
    let mut bound_speeds: Vec<f32> = Vec::new();

    for raw in out.lines() {
        let line = raw.trim_end();
        if let Some(v) = line.strip_prefix("Media current: ") {
            let v = v.trim();
            if v == "is not recognizable" {
                unrecognizable = true;
            } else if profile.is_none() {
                profile = Some(v.to_string());
            }
        } else if let Some(v) = line.strip_prefix("Media status : ") {
            if status.is_none() {
                status = Some(v.trim().to_string());
            }
        } else if let Some(v) = line.strip_prefix("Media product: ") {
            let v = v.trim();
            if product.is_none() && !v.is_empty() {
                product = Some(v.to_string());
            }
        } else if let Some(v) = line.strip_prefix("Media blocks : ") {
            if blocks_free.is_none() {
                blocks_free = parse_media_blocks_writable(v);
            }
        } else if let Some(v) = line.strip_prefix("Media summary: ") {
            if summary_free.is_none() {
                summary_free = parse_summary_free(v);
            }
        } else if let Some(v) = line.strip_prefix("Format status: ") {
            if format_status.is_none() {
                let v = v.trim();
                format_status = Some(v.to_string());
                format_capacity = parse_format_capacity(v);
            }
        } else if let Some(v) = line.strip_prefix("BD Spare Area:") {
            has_spare_area = has_spare_area
                || v.split_whitespace()
                    .any(|t| t.chars().all(|c| c.is_ascii_digit()) && t.parse::<u64>() != Ok(0));
        } else if line.starts_with("Format idx") {
            if idx0_capacity.is_none() {
                if let Some((0x00, sectors)) = parse_format_descriptor(line) {
                    idx0_capacity = Some(sectors * SECTOR);
                }
            }
        } else if line.starts_with("Write speed") {
            if let Some(x) = parse_speed_factor(line) {
                if line.starts_with("Write speed  :") {
                    listed_speeds.push(x);
                } else {
                    bound_speeds.push(x);
                }
            }
        }
    }

    let status = status.unwrap_or_default();
    if status.contains("is not present") {
        bail!("no medium in drive");
    }
    if profile.is_none() && unrecognizable {
        bail!("medium is not recognizable");
    }
    let profile = profile.context("no 'Media current:' line in xorriso output")?;

    let blank = status.contains("is blank");
    let pow = status.contains("POW formatted") || profile.contains("Pseudo Overwrite");
    let free_bytes = if pow {
        0
    } else {
        blocks_free.or(summary_free).unwrap_or(0)
    };

    let fmt = format_status.as_deref().unwrap_or("");
    let formatted = pow
        || has_spare_area
        || fmt.starts_with("formatted,")
        || (blank && fmt.starts_with("written,"));
    // Unformatted media: predict what `-format as_needed` will leave (descriptor
    // 00h is the default full format, the observed 768 MiB spare loss on BD-R 25).
    let formatted_capacity = if fmt.starts_with("formatted,") || fmt.starts_with("written,") {
        format_capacity
    } else if fmt.starts_with("unformatted,") {
        idx0_capacity
    } else {
        None
    };

    let mut speeds = if listed_speeds.is_empty() {
        bound_speeds
    } else {
        listed_speeds
    };
    speeds.sort_by(f32::total_cmp);
    speeds.dedup_by(|a, b| (*a - *b).abs() < 0.05);

    let kind = detect_kind(&profile, free_bytes);
    Ok(MediaInfo {
        kind,
        profile,
        blank,
        formatted,
        free_bytes,
        formatted_capacity,
        speeds,
        media_id: product,
    })
}

// "0 readable , 12219392 writable , 12219392 overall"; POW/ROM media say
// "unused" instead of "writable" and must not count as free.
fn parse_media_blocks_writable(v: &str) -> Option<u64> {
    for part in v.split(',') {
        let mut words = part.split_whitespace();
        let (Some(n), Some(label)) = (words.next(), words.next()) else {
            continue;
        };
        if label == "writable" {
            return n.parse::<u64>().ok().map(|b| b * SECTOR);
        }
    }
    None
}

// "0 sessions, 0 data blocks, 0 data, 23.3g free"
fn parse_summary_free(v: &str) -> Option<u64> {
    let seg = v.rsplit(',').next()?.trim();
    parse_scaled(seg.strip_suffix("free")?.trim())
}

// xorriso Sfile_scale shorthand: binary factors, suffix b/k/m/g/t/p, bare "0"
fn parse_scaled(s: &str) -> Option<u64> {
    let s = s.trim();
    let last = s.chars().next_back()?;
    let (num, factor) = if last.is_ascii_digit() {
        (s, 1.0)
    } else {
        let f: f64 = match last.to_ascii_lowercase() {
            'b' => 1.0,
            'k' => 1024.0,
            'm' => 1024.0 * 1024.0,
            'g' => 1024.0f64.powi(3),
            't' => 1024.0f64.powi(4),
            'p' => 1024.0f64.powi(5),
            _ => return None,
        };
        (&s[..s.len() - 1], f)
    };
    let v = num.trim().parse::<f64>().ok()?;
    if v < 0.0 {
        return None;
    }
    Some((v * factor) as u64)
}

// "formatted, with 23610.0 MiB" / "written, with ..." -> bytes, sector-aligned
// down ("%.1f MiB" is rounded, never trust the tail)
fn parse_format_capacity(v: &str) -> Option<u64> {
    let rest = v.split_once("with ")?.1;
    let mib = rest
        .trim()
        .strip_suffix("MiB")?
        .trim()
        .parse::<f64>()
        .ok()?;
    if mib < 0.0 {
        return None;
    }
    let bytes = (mib * 1024.0 * 1024.0) as u64;
    Some(bytes - bytes % SECTOR)
}

// "Format idx 0 : 00h , 11826176s , 23098.0 MiB" -> (type code, sectors)
fn parse_format_descriptor(line: &str) -> Option<(u8, u64)> {
    let rhs = line.split_once(':')?.1;
    let mut parts = rhs.split(',');
    let code = u8::from_str_radix(parts.next()?.trim().strip_suffix('h')?, 16).ok()?;
    let sectors = parts
        .next()?
        .trim()
        .strip_suffix('s')?
        .parse::<u64>()
        .ok()?;
    Some((code, sectors))
}

// "Write speed  :  17984k ,  4.0xB" -> 4.0
fn parse_speed_factor(line: &str) -> Option<f32> {
    let factor = line.split_once(':')?.1.split(',').nth(1)?.trim();
    let x = factor.find('x')?;
    factor[..x].trim().parse::<f32>().ok().filter(|v| *v > 0.0)
}

/// Pure parser: manufacturer/media code from dvd+rw-mediainfo output.
pub fn parse_media_id(out: &str) -> Option<String> {
    for line in out.lines() {
        if let Some(v) = line.trim_start().strip_prefix("Media ID:") {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Classify media from profile string + nominal size.
pub fn detect_kind(profile: &str, free_bytes: u64) -> MediaKind {
    let p = profile.trim();
    if p.starts_with("BD-ROM") || p.starts_with("DVD-ROM") || p.starts_with("CD-") {
        return MediaKind::Unknown;
    }
    if p.starts_with("BD-R") {
        const LAYERS: [(MediaKind, u64); 4] = [
            (MediaKind::BdR25, BD_R_25),
            (MediaKind::BdR50, BD_R_50),
            (MediaKind::Bdxl100, BDXL_100),
            (MediaKind::BdRXl128, BD_R_XL_128),
        ];
        // blank media: free matches nominal; DM-formatted blanks lose ~3%
        for (kind, nominal) in LAYERS {
            if free_bytes.abs_diff(nominal) <= nominal / 10 {
                return kind;
            }
        }
        return MediaKind::Unknown;
    }
    if p.starts_with("DVD-RW") || p.starts_with("DVD-RAM") || p.starts_with("DVD-R/DL") {
        return MediaKind::Unknown;
    }
    if p.starts_with("DVD-R") {
        return MediaKind::DvdR;
    }
    if p.starts_with("DVD+RW") || p.starts_with("DVD+R/DL") {
        return MediaKind::Unknown;
    }
    if p.starts_with("DVD+R") {
        return MediaKind::DvdPlusR;
    }
    MediaKind::Unknown
}

/// Map a `--media` CLI hint (bd25, bd50, bd100, bd128, dvdr) to a synthetic
/// blank MediaInfo for `ovenmitts plan` without a disc.
pub fn synthetic(hint: &str) -> Result<MediaInfo> {
    let (kind, free_bytes, profile) = match hint.to_ascii_lowercase().as_str() {
        "bd25" => (MediaKind::BdR25, BD_R_25, "BD-R sequential recording"),
        "bd50" => (MediaKind::BdR50, BD_R_50, "BD-R sequential recording"),
        "bd100" => (MediaKind::Bdxl100, BDXL_100, "BD-R sequential recording"),
        "bd128" => (
            MediaKind::BdRXl128,
            BD_R_XL_128,
            "BD-R sequential recording",
        ),
        "dvdr" => (MediaKind::DvdR, DVD_R, "DVD-R sequential recording"),
        _ => bail!("unknown media hint '{hint}' (expected bd25, bd50, bd100, bd128, dvdr)"),
    };
    Ok(MediaInfo {
        kind,
        profile: profile.to_string(),
        blank: true,
        formatted: false,
        free_bytes,
        formatted_capacity: None,
        speeds: Vec::new(),
        media_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOC_BDR_BLANK: &str = include_str!("../tests/fixtures/xorriso_toc_bdr_blank_mdisc.txt");
    const TOC_BDR_POW: &str = include_str!("../tests/fixtures/xorriso_toc_bdr_pow.txt");
    const TOC_DVDRW_CLOSED: &str = include_str!("../tests/fixtures/xorriso_toc_dvdrw_closed.txt");
    const FORMATS_BDR_BLANK: &str =
        include_str!("../tests/fixtures/xorriso_list_formats_bdr_blank.txt");
    const FORMATS_BDRE: &str =
        include_str!("../tests/fixtures/xorriso_list_formats_bdre_formatted.txt");
    const PROBE_BDXL: &str = include_str!("../tests/fixtures/xorriso_probe_bdxl_blank.txt");
    const PROBE_BDR_FULL: &str = include_str!("../tests/fixtures/xorriso_probe_bdr_blank_full.txt");
    const MEDIAINFO_POW: &str = include_str!("../tests/fixtures/dvd_rw_mediainfo_bdr_pow.txt");

    #[test]
    fn blank_mdisc_toc_without_list_formats() {
        let m = parse_xorriso_probe(TOC_BDR_BLANK).unwrap();
        assert_eq!(m.profile, "BD-R sequential recording");
        assert!(m.blank);
        assert!(!m.formatted);
        assert_eq!(m.free_bytes, 12_219_392 * 2048);
        assert_eq!(m.free_bytes, BD_R_25);
        assert_eq!(m.kind, MediaKind::BdR25);
        assert_eq!(m.formatted_capacity, None);
        assert!(m.speeds.is_empty());
        assert_eq!(
            m.media_id.as_deref(),
            Some("MILLEN/MR1/0 , Millenniata Inc.")
        );
    }

    #[test]
    fn full_probe_blank_bdr() {
        let m = parse_xorriso_probe(PROBE_BDR_FULL).unwrap();
        assert!(m.blank);
        assert!(!m.formatted);
        assert_eq!(m.free_bytes, BD_R_25);
        assert_eq!(m.kind, MediaKind::BdR25);
        // prediction from Format idx 0 (00h): the observed 768 MiB spare loss
        assert_eq!(m.formatted_capacity, Some(24_220_008_448));
        assert_eq!(m.speeds, vec![2.0, 4.0]);
    }

    #[test]
    fn pow_bdr_is_unusable() {
        let m = parse_xorriso_probe(TOC_BDR_POW).unwrap();
        assert!(!m.blank);
        assert!(m.formatted);
        assert_eq!(m.free_bytes, 0);
        assert_eq!(m.kind, MediaKind::Unknown);
        assert_eq!(
            m.media_id.as_deref(),
            Some("VERBAT/IMk/0 , Mitsubishi Kagaku Media Co.")
        );
    }

    #[test]
    fn closed_dvdrw_has_no_free_space() {
        let m = parse_xorriso_probe(TOC_DVDRW_CLOSED).unwrap();
        assert_eq!(m.profile, "DVD-RW sequential recording");
        assert!(!m.blank);
        assert!(!m.formatted);
        assert_eq!(m.free_bytes, 0);
        assert_eq!(m.kind, MediaKind::Unknown);
        assert_eq!(m.formatted_capacity, None);
    }

    #[test]
    fn list_formats_blank_bdr_predicts_dm_capacity() {
        let m = parse_xorriso_probe(FORMATS_BDR_BLANK).unwrap();
        assert!(m.blank);
        assert!(!m.formatted);
        // no "Media blocks" line here: falls back to rounded summary shorthand
        assert_eq!(m.free_bytes, (23.3f64 * 1024f64.powi(3)) as u64);
        assert_eq!(m.kind, MediaKind::BdR25);
        assert_eq!(m.formatted_capacity, Some(11_826_176 * 2048));
    }

    #[test]
    fn formatted_bdre_reports_actual_capacity() {
        let m = parse_xorriso_probe(FORMATS_BDRE).unwrap();
        assert!(!m.blank);
        assert!(m.formatted);
        assert_eq!(m.formatted_capacity, Some(24_756_879_360)); // 23610.0 MiB
        assert_eq!(m.free_bytes, (22.4f64 * 1024f64.powi(3)) as u64);
        assert_eq!(m.kind, MediaKind::BdR25);
    }

    #[test]
    fn blank_bdxl_probe() {
        let m = parse_xorriso_probe(PROBE_BDXL).unwrap();
        assert!(m.blank);
        assert_eq!(m.kind, MediaKind::Bdxl100);
        assert_eq!(m.formatted_capacity, Some(47_305_728 * 2048));
        // only "Write speed L/H" bounds present: used as fallback
        assert_eq!(m.speeds, vec![2.0, 4.0]);
    }

    #[test]
    fn no_media_lines_is_an_error() {
        assert!(parse_xorriso_probe("xorriso : FAILURE : Cannot acquire drive").is_err());
        let empty_tray = "Media current: is not recognizable\nMedia status : is not present\n";
        let err = parse_xorriso_probe(empty_tray).unwrap_err();
        assert!(err.to_string().contains("no medium"));
    }

    #[test]
    fn media_id_from_mediainfo() {
        assert_eq!(parse_media_id(MEDIAINFO_POW).as_deref(), Some("TDKBLD/RBB"));
        assert_eq!(parse_media_id("INQUIRY: [X][Y][Z]\n"), None);
    }

    #[test]
    fn detect_kind_by_profile_and_size() {
        let bd = "BD-R sequential recording";
        assert_eq!(detect_kind(bd, BD_R_25), MediaKind::BdR25);
        assert_eq!(detect_kind(bd, 24_220_008_448), MediaKind::BdR25); // DM-formatted
        assert_eq!(detect_kind(bd, BD_R_50), MediaKind::BdR50);
        assert_eq!(detect_kind(bd, BDXL_100), MediaKind::Bdxl100);
        assert_eq!(detect_kind(bd, BD_R_XL_128), MediaKind::BdRXl128);
        assert_eq!(detect_kind(bd, 0), MediaKind::Unknown);
        assert_eq!(detect_kind("BD-RE", BD_R_25), MediaKind::BdR25);
        assert_eq!(detect_kind("BD-ROM", BD_R_25), MediaKind::Unknown);
        assert_eq!(
            detect_kind("DVD-R sequential recording", 4_707_319_808),
            MediaKind::DvdR
        );
        assert_eq!(
            detect_kind("DVD-RW sequential recording", 0),
            MediaKind::Unknown
        );
        assert_eq!(detect_kind("DVD-RAM", 0), MediaKind::Unknown);
        assert_eq!(detect_kind("DVD+R", 4_700_372_992), MediaKind::DvdPlusR);
        assert_eq!(detect_kind("DVD+R/DL", 8_547_991_552), MediaKind::Unknown);
        assert_eq!(detect_kind("DVD+RW", 0), MediaKind::Unknown);
        assert_eq!(detect_kind("CD-R", 0), MediaKind::Unknown);
    }

    #[test]
    fn synthetic_hints() {
        let m = synthetic("bd25").unwrap();
        assert_eq!(m.kind, MediaKind::BdR25);
        assert_eq!(m.free_bytes, BD_R_25);
        assert!(m.blank);
        assert_eq!(m.formatted_capacity, None);
        assert_eq!(synthetic("BD100").unwrap().kind, MediaKind::Bdxl100);
        assert_eq!(synthetic("bd50").unwrap().free_bytes, BD_R_50);
        assert_eq!(synthetic("bd128").unwrap().free_bytes, BD_R_XL_128);
        assert_eq!(synthetic("dvdr").unwrap().kind, MediaKind::DvdR);
        assert!(synthetic("cdr").is_err());
    }

    #[test]
    fn scaled_shorthand() {
        assert_eq!(
            parse_scaled("23.3g"),
            Some((23.3f64 * 1024f64.powi(3)) as u64)
        );
        assert_eq!(parse_scaled("4489m"), Some(4489 * 1024 * 1024));
        assert_eq!(parse_scaled("0"), Some(0));
        assert_eq!(parse_scaled("12k"), Some(12_288));
        assert_eq!(parse_scaled("nonsense"), None);
    }

    #[test]
    fn speed_line_variants() {
        assert_eq!(
            parse_speed_factor("Write speed  :  17984k ,  4.0xB"),
            Some(4.0)
        );
        assert_eq!(
            parse_speed_factor("Write speed L:   8992k ,  2.0xB"),
            Some(2.0)
        );
        assert_eq!(
            parse_speed_factor("Write speed  :   706k , 4.0xC"),
            Some(4.0)
        );
        assert_eq!(parse_speed_factor("Write speed  : garbage"), None);
    }

    #[test]
    fn list_drives_finds_sr_devices_in_numeric_order() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["sr10", "sr2", "sr0", "sda", "loop0", "srx", "sr"] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        let drives = list_drives_in(dir.path());
        let names: Vec<&str> = drives
            .iter()
            .map(|d| d.rsplit('/').next().unwrap())
            .collect();
        assert_eq!(names, ["sr0", "sr2", "sr10"]);
        assert!(list_drives_in(std::path::Path::new("/nonexistent")).is_empty());
    }
}
