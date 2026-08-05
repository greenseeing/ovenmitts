//! Durable writes for the files a future recovery depends on. A checksums
//! file or RECOVERY.txt that evaporates in a crash after the burn defeats the
//! point of an archival tool, so these writes fsync the file *and* its parent
//! directory (the entry itself is directory data).

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};

/// Write `contents` to `path` (0600 - checksums, manifests and reports
/// describe private archive contents), fsync the file, then fsync the parent
/// directory so the new entry survives a crash.
pub fn write_durable(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    // mode() applies only at creation; tighten pre-existing files too
    f.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", path.display()))?;
    f.write_all(contents.as_ref())
        .with_context(|| format!("write {}", path.display()))?;
    f.sync_all()
        .with_context(|| format!("fsync {}", path.display()))?;
    fsync_dir(path.parent().unwrap_or(Path::new(".")))
}

/// fsync a directory: makes creates/renames inside it durable.
pub fn fsync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("fsync dir {}", dir.display()))
}

/// Make a file written by an external tool durable: fsync its data and its
/// parent directory entry (write_durable for files we didn't write ourselves).
pub fn fsync_existing(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("fsync {}", path.display()))?;
    fsync_dir(path.parent().unwrap_or(Path::new(".")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_durable_creates_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        write_durable(&p, "first").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "first");
        write_durable(&p, "second, shorter or longer").unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "second, shorter or longer"
        );
    }

    #[test]
    fn write_durable_is_owner_only_even_on_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        write_durable(&p, "x").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // a pre-existing world-readable file must be tightened, not kept
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_durable(&p, "y").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn write_durable_errors_on_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("no-such-dir").join("f.txt");
        let err = write_durable(&p, "x").unwrap_err();
        assert!(err.to_string().contains("write"), "{err:#}");
    }

    #[test]
    fn fsync_dir_errors_on_missing_dir() {
        assert!(fsync_dir(Path::new("/no/such/dir/xyz")).is_err());
    }

    #[test]
    fn fsync_existing_syncs_file_and_errors_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tool-output.iso");
        std::fs::write(&p, b"external tool wrote this").unwrap();
        fsync_existing(&p).unwrap();
        assert!(fsync_existing(&dir.path().join("absent")).is_err());
    }
}
