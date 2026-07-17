use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub device: Option<String>,
    pub staging: Option<PathBuf>,
    pub speed: Option<u32>,
    pub redundancy_pct: Option<u32>,
    pub headroom_pct: Option<u32>,
    pub defect_management: Option<bool>,
    pub keep_iso: Option<bool>,
    pub eject_when_done: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub device: String,
    /// true only when --device was passed on the CLI; a non-explicit device
    /// (built-in default or config file) may be swapped by auto-detection.
    pub device_explicit: bool,
    pub staging: PathBuf,
    pub speed: Option<u32>,
    pub redundancy_pct: u32,
    pub headroom_pct: u32,
    pub defect_management: bool,
    pub keep_iso: bool,
    /// Tri-state on purpose: unset means "eject only when an operator is
    /// present" (TUI), which only the runner can decide per request.
    pub eject_when_done: Option<bool>,
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("ovenmitts").join("config.toml")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_staging() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local").join("share"));
    base.join("ovenmitts").join("staging")
}

pub fn load(path: Option<&Path>) -> Result<FileConfig> {
    let path = path.map(PathBuf::from).unwrap_or_else(default_path);
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
}

/// Persist `device` into the config file, creating parent dirs and the file
/// if absent. toml_edit keeps comments, formatting and all other keys; a
/// parse failure propagates instead of clobbering a hand-edited file.
pub fn save_device(path: &Path, device: &str) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading config {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parsing config {}", path.display()))?;
    doc["device"] = toml_edit::value(device);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // write-then-rename: a torn write would brick every later load
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing config {}", path.display()))
}

impl Config {
    pub fn resolve(file: FileConfig) -> Result<Self> {
        let redundancy_pct = file.redundancy_pct.unwrap_or(15);
        let headroom_pct = file.headroom_pct.unwrap_or(5);
        // out-of-range percentages would silently defeat the capacity gate
        anyhow::ensure!(
            (1..=100).contains(&redundancy_pct),
            "config: redundancy_pct must be 1..=100 (got {redundancy_pct})"
        );
        anyhow::ensure!(
            headroom_pct <= 50,
            "config: headroom_pct must be 0..=50 (got {headroom_pct})"
        );
        Ok(Self {
            // a config-file device is a soft preference (tried first, may be
            // swapped by auto-detection); only --device pins, set in main
            device_explicit: false,
            device: file.device.unwrap_or_else(|| "/dev/sr0".into()),
            staging: file.staging.unwrap_or_else(default_staging),
            speed: file.speed,
            redundancy_pct,
            headroom_pct,
            defect_management: file.defect_management.unwrap_or(false),
            keep_iso: file.keep_iso.unwrap_or(true),
            eject_when_done: file.eject_when_done,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults() {
        let c = Config::resolve(FileConfig::default()).unwrap();
        assert_eq!(c.device, "/dev/sr0");
        assert!(!c.device_explicit);
        assert_eq!(c.redundancy_pct, 15);
        assert_eq!(c.headroom_pct, 5);
        assert!(!c.defect_management);
        assert!(c.keep_iso);
        assert_eq!(c.eject_when_done, None);
    }

    #[test]
    fn parse_partial_toml() {
        let f: FileConfig = toml::from_str("device = \"/dev/sr1\"\nredundancy_pct = 20\n").unwrap();
        let c = Config::resolve(f).unwrap();
        assert_eq!(c.device, "/dev/sr1");
        assert!(!c.device_explicit, "config device is a soft preference");
        assert_eq!(c.redundancy_pct, 20);
        assert_eq!(c.headroom_pct, 5);
    }

    #[test]
    fn out_of_range_percentages_rejected() {
        let bad_headroom = FileConfig {
            headroom_pct: Some(150),
            ..FileConfig::default()
        };
        assert!(Config::resolve(bad_headroom).is_err());
        let bad_redundancy = FileConfig {
            redundancy_pct: Some(0),
            ..FileConfig::default()
        };
        assert!(Config::resolve(bad_redundancy).is_err());
    }

    #[test]
    fn unknown_keys_rejected() {
        assert!(toml::from_str::<FileConfig>("nope = 1\n").is_err());
    }

    #[test]
    fn save_device_creates_file_and_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        save_device(&path, "/dev/sr1").unwrap();
        let c = Config::resolve(load(Some(&path)).unwrap()).unwrap();
        assert_eq!(c.device, "/dev/sr1");
        assert!(!c.device_explicit);
    }

    #[test]
    fn save_device_preserves_comments_and_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# my burner\ndevice = \"/dev/sr9\"\nredundancy_pct = 20\n",
        )
        .unwrap();
        save_device(&path, "/dev/sr1").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my burner"));
        assert!(text.contains("redundancy_pct = 20"));
        assert!(text.contains("device = \"/dev/sr1\""));
        assert!(!text.contains("/dev/sr9"));
    }

    #[test]
    fn save_device_errors_on_invalid_toml_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "device = [broken\n").unwrap();
        assert!(save_device(&path, "/dev/sr1").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "device = [broken\n"
        );
    }
}
