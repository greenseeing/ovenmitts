//! Sector-level ECC: augment the mastered ISO in place with a dvdisaster
//! RS02 layer (speed47 fork). The ECC lives past the ISO 9660 filesystem on
//! the same disc, so it survives filesystem-metadata damage that file-level
//! par2 cannot, and it costs nothing: it is sized to fill disc space that
//! would otherwise burn empty. The volume descriptors are untouched -
//! readers ignore the appended data, and the truncation self-check's
//! "actual >= declared" rule stays valid.

use std::path::Path;
use std::time::Duration;

use anyhow::{ensure, Context, Result};

const SECTOR: u64 = 2048;
/// Below this ECC share of the image, augmentation is not worth a layer
/// (dvdisaster's own presets start well above; a sliver protects nothing).
const MIN_MARGIN_PCT: u64 = 5;
/// Staging space the augmentation must leave untouched (reports, logs).
const STAGING_RESERVE: u64 = 16 * 1024 * 1024;

/// Pure: the augmentation target in sectors - the disc budget, capped by
/// what the staging filesystem can actually absorb (the image grows in
/// staging; a small disc payload on a 100 GB medium must not ENOSPC the
/// staging disk). None when the affordable margin is under MIN_MARGIN_PCT
/// of the image.
pub fn augment_target(iso_bytes: u64, budget_bytes: u64, staging_free: u64) -> Option<u64> {
    let iso_sectors = iso_bytes.div_ceil(SECTOR);
    let affordable = iso_sectors + staging_free.saturating_sub(STAGING_RESERVE) / SECTOR;
    let target_sectors = (budget_bytes / SECTOR).min(affordable);
    let margin = target_sectors.saturating_sub(iso_sectors);
    (margin * 100 >= iso_sectors * MIN_MARGIN_PCT && margin > 0).then_some(target_sectors)
}

/// Pure: the dvdisaster argv (verified against the speed47 fork's manpage:
/// `-i <image> -mRS02 -n <total sectors> -c` augments in place).
pub fn augment_args(iso: &Path, target_sectors: u64) -> Vec<String> {
    vec![
        "-i".into(),
        iso.display().to_string(),
        "-mRS02".into(),
        "-n".into(),
        target_sectors.to_string(),
        "-c".into(),
    ]
}

/// Run the augmentation; returns the image's new size. Fails closed: a
/// dvdisaster error leaves the image in an unknown state, and burning it
/// would waste a disc on dead ECC. Progress lines are forwarded verbatim
/// (the fork's progress format is undocumented - they double as watchdog
/// keepalives, so `--no-progress` must never be passed).
pub fn augment(
    bin: &Path,
    iso: &Path,
    target_sectors: u64,
    stall: Duration,
    on_line: &mut dyn FnMut(&str),
) -> Result<u64> {
    let args = augment_args(iso, target_sectors);
    crate::proc::run_streaming(bin, &args, stall, on_line)
        .context("dvdisaster RS02 augmentation failed - the staged image may be half-augmented; re-master or set ecc = false")?;
    let after = std::fs::metadata(iso)
        .with_context(|| format!("stat augmented {}", iso.display()))?
        .len();
    ensure!(
        after <= target_sectors * SECTOR,
        "dvdisaster grew {} past the requested {target_sectors} sectors ({after} bytes) - it no longer fits the disc budget",
        iso.display()
    );
    // the ECC layer must be on the platter like every other artifact
    crate::fsutil::fsync_existing(iso)?;
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn args_match_the_forks_manpage_contract() {
        assert_eq!(
            augment_args(Path::new("/stage/v.iso"), 11_729_216),
            vec!["-i", "/stage/v.iso", "-mRS02", "-n", "11729216", "-c"]
        );
    }

    const ROOMY: u64 = 1 << 40; // staging with a TiB free

    #[test]
    fn target_fills_the_budget_or_declines_slivers() {
        // 8 MiB image, 25 GB budget: plenty of margin, target = budget floor
        let budget = 23_774_035_968u64; // BD-R 25 budget after 5% headroom
        assert_eq!(
            augment_target(8 * 1024 * 1024, budget, ROOMY),
            Some(budget / 2048)
        );
        // image already fills the budget: no room for a layer
        assert_eq!(augment_target(budget, budget, ROOMY), None);
        // margin under 5% of the image: a sliver protects nothing
        let iso = 20_000_000 * 2048u64;
        let budget = (20_000_000 + 500_000) * 2048u64; // 2.5% margin
        assert_eq!(augment_target(iso, budget, ROOMY), None);
        let budget = (20_000_000 + 1_500_000) * 2048u64; // 7.5% margin
        assert_eq!(augment_target(iso, budget, ROOMY), Some(21_500_000));
    }

    #[test]
    fn target_never_outgrows_the_staging_filesystem() {
        // 2 GiB image, 100 GB disc budget, but staging has only 10 GiB free:
        // the image grows in staging, so the target caps at what fits there
        let iso = 2u64 << 30;
        let budget = 95u64 << 30;
        let staging_free = 10u64 << 30;
        let target = augment_target(iso, budget, staging_free).unwrap();
        let growth = target * 2048 - iso;
        assert!(
            growth <= staging_free,
            "growth {growth} > free {staging_free}"
        );
        assert!(target < budget / 2048, "budget target must be capped");
        // staging too tight for a meaningful layer: skip, never ENOSPC
        assert_eq!(augment_target(iso, budget, 32 * 1024 * 1024), None);
    }

    #[test]
    fn augment_runs_tool_and_reports_new_size() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("v.iso");
        std::fs::write(&iso, vec![0u8; 4096]).unwrap();
        let fake = dir.path().join("dvdisaster");
        // appends an ECC-ish tail and records argv, like the real tool
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {argv}\n\
                 printf 'ecc-tail' >> \"$2\"\nprintf 'augmenting image\\n'\n",
                argv = dir.path().join("argv.txt").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut lines = Vec::new();
        let after = augment(&fake, &iso, 1000, Duration::ZERO, &mut |l| {
            lines.push(l.to_string())
        })
        .unwrap();
        assert_eq!(after, 4096 + "ecc-tail".len() as u64);
        assert!(lines.iter().any(|l| l.contains("augmenting")));
        let argv = std::fs::read_to_string(dir.path().join("argv.txt")).unwrap();
        assert_eq!(argv.lines().collect::<Vec<_>>(), augment_args(&iso, 1000));
    }

    #[test]
    fn augment_failure_is_loud_and_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("v.iso");
        std::fs::write(&iso, vec![0u8; 4096]).unwrap();
        let fake = dir.path().join("dvdisaster");
        std::fs::write(&fake, "#!/bin/sh\necho 'out of memory' >&2\nexit 3\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = augment(&fake, &iso, 1000, Duration::ZERO, &mut |_| {}).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("RS02 augmentation failed"), "{msg}");
        assert!(msg.contains("out of memory"), "{msg}");
        assert!(msg.contains("ecc = false"), "{msg}");
    }
}
