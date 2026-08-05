//! `ovenmitts update` — download the latest release, verify it, replace in place.
//!
//! Entirely in-process: no shell, no piping a fetched script into `bash`. The
//! binary is fetched over hardened TLS, its published SHA-256 sidecar is
//! verified against the download before anything is installed, and the running
//! executable is swapped with an atomic same-filesystem rename. Trust roots in
//! TLS to github.com plus the release assets — the same anchor as the first
//! install, and the release pipeline additionally attests provenance.
//!
//! `OVENMITTS_VERSION` pins a specific version (e.g. `OVENMITTS_VERSION=0.1.9`);
//! unset takes whatever `/releases/latest` resolves to. `install.sh` remains
//! the first-install path (it also installs the xorriso/par2 backends).

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::hashing;
use crate::tools::which;

const REPO: &str = "greenseeing/ovenmitts";

/// Release asset name for a CPU architecture (`std::env::consts::ARCH`).
fn asset_for(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("ovenmitts-linux-amd64"),
        "aarch64" => Some("ovenmitts-linux-arm64"),
        _ => None,
    }
}

fn asset_name() -> Result<&'static str> {
    asset_for(std::env::consts::ARCH).with_context(|| {
        format!(
            "no prebuilt binary for this CPU architecture ({})",
            std::env::consts::ARCH
        )
    })
}

/// Base URL for release assets. `OVENMITTS_VERSION` pins an exact tag; unset
/// uses GitHub's `latest` redirect, which sidesteps the releases API entirely
/// (no JSON to parse, no tag string to scrape into a path).
fn release_base() -> Result<String> {
    match std::env::var("OVENMITTS_VERSION") {
        Ok(v) if !v.is_empty() => {
            let ver = v.strip_prefix('v').unwrap_or(&v);
            if !is_semver(ver) {
                bail!("OVENMITTS_VERSION is not a valid version: '{v}'");
            }
            Ok(format!(
                "https://github.com/{REPO}/releases/download/v{ver}"
            ))
        }
        _ => Ok(format!(
            "https://github.com/{REPO}/releases/latest/download"
        )),
    }
}

/// `MAJOR.MINOR.PATCH`, digits only — the value lands in a URL and a path.
fn is_semver(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// curl argv (no shell): fail on HTTP errors, HTTPS only end to end, cap the
/// transfer, write to `dest`. Returns false when curl reports failure.
fn curl_args(url: &str, dest: &Path) -> Vec<String> {
    vec![
        "--fail".into(),
        "--silent".into(),
        "--show-error".into(),
        "--location".into(),
        "--proto".into(),
        "=https".into(),
        "--proto-redir".into(),
        "=https".into(),
        "--tlsv1.2".into(),
        "--max-time".into(),
        "300".into(),
        "--output".into(),
        dest.display().to_string(),
        url.into(),
    ]
}

fn curl(curl_bin: &Path, url: &str, dest: &Path) -> Result<()> {
    let status = Command::new(curl_bin)
        .args(curl_args(url, dest))
        .status()
        .with_context(|| format!("could not launch curl to fetch {url}"))?;
    if !status.success() {
        bail!("download failed: {url}");
    }
    Ok(())
}

/// Expected SHA-256 for `asset`, read from its published `.sha256` sidecar.
fn fetch_expected_sha(curl_bin: &Path, base: &str, asset: &str, scratch: &Path) -> Result<String> {
    let sidecar = scratch.join(format!("{asset}.sha256"));
    curl(curl_bin, &format!("{base}/{asset}.sha256"), &sidecar)
        .context("fetching the published checksum")?;
    let text = std::fs::read_to_string(&sidecar).context("reading the checksum sidecar")?;
    let entries = hashing::parse_checksums(&text)?;
    let (sha, _) = entries
        .into_iter()
        .next()
        .context("checksum sidecar is empty")?;
    Ok(sha)
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    AlreadyCurrent,
    Updated,
}

pub fn run() -> Result<()> {
    let Some(curl_bin) = which("curl") else {
        bail!("curl is required to update; install it and try again");
    };
    let asset = asset_name()?;
    let base = release_base()?;
    let current_exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("could not locate the running ovenmitts binary")?;

    match update_binary(&curl_bin, &base, asset, &current_exe)? {
        Outcome::AlreadyCurrent => println!("ovenmitts is already up to date"),
        Outcome::Updated => println!("Updated ovenmitts at {}", current_exe.display()),
    }
    Ok(())
}

/// The whole update, parameterized on the target so it is testable without
/// touching the real running binary. Downloads nothing when already current;
/// on mismatch or download failure, leaves `current_exe` untouched.
fn update_binary(curl_bin: &Path, base: &str, asset: &str, current_exe: &Path) -> Result<Outcome> {
    let bindir = current_exe
        .parent()
        .context("the running binary has no parent directory")?
        .to_path_buf();

    // Fetch the sidecar first: comparing it to the running binary decides
    // whether an update is needed without downloading (or executing) anything.
    let expected = fetch_expected_sha(curl_bin, base, asset, &bindir)?;
    let _ = std::fs::remove_file(bindir.join(format!("{asset}.sha256")));

    let current_sha =
        hashing::sha256_file(current_exe, &mut |_, _| {}).context("hashing the current binary")?;
    if current_sha == expected {
        return Ok(Outcome::AlreadyCurrent);
    }

    if !is_writable_dir(&bindir) {
        bail!(
            "{} is not writable - re-run the installer instead:\n  \
             curl -fsSL https://raw.githubusercontent.com/{REPO}/main/install.sh | bash",
            bindir.display()
        );
    }

    let staged = bindir.join(format!(".ovenmitts.new.{}", std::process::id()));
    println!("Downloading {asset} ...");
    if let Err(e) = curl(curl_bin, &format!("{base}/{asset}"), &staged) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }

    let got = match hashing::sha256_file(&staged, &mut |_, _| {}) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            return Err(e).context("hashing the downloaded binary");
        }
    };
    if got != expected {
        let _ = std::fs::remove_file(&staged);
        bail!("checksum mismatch - refusing to install (expected {expected}, got {got})");
    }

    if let Err(e) = set_executable(&staged).and_then(|()| {
        std::fs::rename(&staged, current_exe)
            .with_context(|| format!("replacing {}", current_exe.display()))
    }) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    Ok(Outcome::Updated)
}

fn is_writable_dir(dir: &Path) -> bool {
    // A probe create is the honest test: perms + mount read-only-ness + ACLs.
    let probe = dir.join(format!(".ovenmitts.wtest.{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // OVENMITTS_VERSION is process-global; serialize the tests that mutate it.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn arch_maps_to_release_asset() {
        assert_eq!(asset_for("x86_64"), Some("ovenmitts-linux-amd64"));
        assert_eq!(asset_for("aarch64"), Some("ovenmitts-linux-arm64"));
        assert_eq!(asset_for("riscv64"), None);
    }

    #[test]
    fn semver_validation() {
        assert!(is_semver("0.1.9"));
        assert!(is_semver("10.20.30"));
        assert!(!is_semver("0.1"));
        assert!(!is_semver("0.1.9.1"));
        assert!(!is_semver("v0.1.9"));
        assert!(!is_semver("0.1.x"));
        assert!(!is_semver("0..9"));
        assert!(!is_semver(""));
    }

    #[test]
    fn latest_base_needs_no_api() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("OVENMITTS_VERSION");
        let base = release_base().unwrap();
        assert_eq!(
            base,
            "https://github.com/greenseeing/ovenmitts/releases/latest/download"
        );
        assert!(!base.contains("api.github.com"));
    }

    #[test]
    fn pinned_version_builds_tag_url() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("OVENMITTS_VERSION", "v1.2.3");
        let base = release_base().unwrap();
        std::env::remove_var("OVENMITTS_VERSION");
        assert_eq!(
            base,
            "https://github.com/greenseeing/ovenmitts/releases/download/v1.2.3"
        );
    }

    #[test]
    fn pinned_bad_version_rejected() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("OVENMITTS_VERSION", "1.2.x");
        let err = release_base().unwrap_err();
        std::env::remove_var("OVENMITTS_VERSION");
        assert!(err.to_string().contains("not a valid version"), "{err}");
    }

    #[test]
    fn curl_argv_is_https_only_and_shell_free() {
        let args = curl_args("https://example/x", Path::new("/tmp/x"));
        // proto locks both the initial request and every redirect to https.
        assert!(args.windows(2).any(|w| w == ["--proto", "=https"]));
        assert!(args.windows(2).any(|w| w == ["--proto-redir", "=https"]));
        assert!(args.contains(&"--fail".to_string()));
        assert!(args.contains(&"--tlsv1.2".to_string()));
        assert!(!args.iter().any(|a| a.contains("bash")));
    }

    // A fake curl that serves files from `serve_dir`, matching the real curl
    // argv (`... --output <dest> <url>`): copies serve_dir/<basename(url)> to
    // dest, exiting non-zero when the source is absent (mirrors `--fail`).
    fn fake_curl(dir: &Path, serve_dir: &Path) -> PathBuf {
        let p = dir.join("curl");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nset -eu\ndest=\"\"; url=\"\"; prev=\"\"\nfor a in \"$@\"; do\n  \
                 if [ \"$prev\" = \"--output\" ]; then dest=\"$a\"; fi\n  \
                 case \"$a\" in --*) ;; =https) ;; *) url=\"$a\" ;; esac\n  prev=\"$a\"\ndone\n\
                 base=\"${{url##*/}}\"\nsrc=\"{serve}/$base\"\n\
                 [ -f \"$src\" ] || {{ echo \"fake curl: no $src\" >&2; exit 22; }}\n\
                 cp \"$src\" \"$dest\"\n",
                serve = serve_dir.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn sha_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        let d = h.finalize();
        let mut s = String::new();
        use std::fmt::Write as _;
        for b in d {
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        serve: PathBuf,
        curl: PathBuf,
        bindir: PathBuf,
        exe: PathBuf,
    }

    // Publishes a "release" of `new_bytes` for the current arch and installs
    // `installed_bytes` as the running binary.
    fn setup(installed_bytes: &[u8], new_bytes: &[u8]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let serve = dir.path().join("serve");
        let bindir = dir.path().join("bin");
        std::fs::create_dir_all(&serve).unwrap();
        std::fs::create_dir_all(&bindir).unwrap();
        let asset = asset_name().unwrap();
        std::fs::write(serve.join(asset), new_bytes).unwrap();
        std::fs::write(
            serve.join(format!("{asset}.sha256")),
            format!("{}  {asset}\n", sha_of(new_bytes)),
        )
        .unwrap();
        let exe = bindir.join("ovenmitts");
        std::fs::write(&exe, installed_bytes).unwrap();
        let curl = fake_curl(dir.path(), &serve);
        Fixture {
            _dir: dir,
            serve,
            curl,
            bindir,
            exe,
        }
    }

    #[test]
    fn updates_and_verifies_atomically() {
        let f = setup(b"old-binary", b"new-binary-v2");
        let base = format!("file://{}", f.serve.display());
        let asset = asset_name().unwrap();
        let outcome = update_binary(&f.curl, &base, asset, &f.exe).unwrap();
        assert_eq!(outcome, Outcome::Updated);
        assert_eq!(std::fs::read(&f.exe).unwrap(), b"new-binary-v2");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&f.exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // no staging leftovers
        let leftovers: Vec<_> = std::fs::read_dir(&f.bindir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".ovenmitts.new")
            })
            .collect();
        assert!(leftovers.is_empty(), "staging file left behind");
    }

    #[test]
    fn already_current_downloads_nothing() {
        let f = setup(b"same-bytes", b"same-bytes");
        let base = format!("file://{}", f.serve.display());
        let asset = asset_name().unwrap();
        assert_eq!(
            update_binary(&f.curl, &base, asset, &f.exe).unwrap(),
            Outcome::AlreadyCurrent
        );
        assert_eq!(std::fs::read(&f.exe).unwrap(), b"same-bytes");
    }

    #[test]
    fn checksum_mismatch_leaves_binary_untouched() {
        let f = setup(b"old-binary", b"new-binary");
        // Corrupt the published binary so it no longer matches its sidecar.
        std::fs::write(f.serve.join(asset_name().unwrap()), b"tampered").unwrap();
        let base = format!("file://{}", f.serve.display());
        let err = update_binary(&f.curl, &base, asset_name().unwrap(), &f.exe).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        assert_eq!(std::fs::read(&f.exe).unwrap(), b"old-binary");
    }
}
