use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};

const CHUNK: usize = 1024 * 1024;

/// Streaming SHA-256 of a file; cb(bytes_done, bytes_total) for progress.
pub fn sha256_file(path: &Path, cb: &mut dyn FnMut(u64, u64)) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let total = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    cb(0, total);
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        cb(done, total);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Write sha256sum-compatible lines: "<hex>  <relpath>\n" (two spaces).
pub fn write_checksums(entries: &[(String, String)], out: &Path) -> Result<()> {
    let mut text = String::new();
    for (hash, rel) in entries {
        text.push_str(hash);
        text.push_str("  ");
        text.push_str(rel);
        text.push('\n');
    }
    std::fs::write(out, text).with_context(|| format!("write {}", out.display()))
}

/// Parse sha256sum-format text into (hex, relpath) pairs.
pub fn parse_checksums(text: &str) -> Result<Vec<(String, String)>> {
    let mut entries = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            continue;
        }
        let Some((hash, rel)) = line.split_once("  ") else {
            bail!("checksums line {}: missing two-space separator", i + 1);
        };
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("checksums line {}: not a sha256 hex digest", i + 1);
        }
        if rel.is_empty() {
            bail!("checksums line {}: empty path", i + 1);
        }
        if !plain_relative(rel) {
            bail!("checksums line {}: not a plain relative path", i + 1);
        }
        entries.push((hash.to_ascii_lowercase(), rel.to_string()));
    }
    Ok(entries)
}

/// Only bare `a/b/c` paths: non-empty, no root, no `..`, no leading `.`.
fn plain_relative(rel: &str) -> bool {
    !rel.is_empty()
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

enum Resolved {
    File(PathBuf),
    Missing,
}

// A verify disc is untrusted input: a checksums file naming host paths, or a
// Rock Ridge symlink escaping the mount, must abort the whole run — per-entry
// pass/fail output would tell a crafted disc which host files exist.
//
// Descend component by component with lstat (no symlink is ever followed). A
// legitimate ovenmitts disc never lists symlinks (they are excluded from
// checksums at burn time), so any symlink on the path is anomalous and rejected
// uniformly — dangling or not. That uniformity is what closes the oracle:
// following a symlink and letting its target's existence decide bail-vs-missing
// would leak whether an attacker-named host path exists. `NotFound` therefore
// only ever means a genuinely-absent, symlink-free path (an ordinary missing
// file), never a probe of anything outside the mount.
fn resolve_under_root(canon_root: &Path, rel: &str) -> Result<Resolved> {
    ensure!(
        plain_relative(rel),
        "checksums entry '{rel}' is not a plain relative path - refusing to verify this disc"
    );
    let mut cur = canon_root.to_path_buf();
    for comp in Path::new(rel).components() {
        let Component::Normal(name) = comp else {
            // plain_relative already guaranteed this; belt and suspenders.
            bail!("checksums entry '{rel}' is not a plain relative path - refusing to verify this disc");
        };
        cur.push(name);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => bail!(
                "checksums entry '{rel}' escapes the disc root (symlink on the disc?) - refusing to verify this disc"
            ),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Resolved::Missing),
            Err(e) => return Err(e).with_context(|| format!("resolve {rel}")),
        }
    }
    // Every component was a real, non-symlink entry pushed onto canon_root, so
    // the result is guaranteed under the root.
    Ok(Resolved::File(cur))
}

/// Hash every listed file under root and compare; returns (relpath, ok) per entry.
pub fn verify_checksums(
    root: &Path,
    entries: &[(String, String)],
    cb: &mut dyn FnMut(u64, u64),
) -> Result<Vec<(String, bool)>> {
    let canon_root = root
        .canonicalize()
        .with_context(|| format!("resolve {}", root.display()))?;
    let resolved = entries
        .iter()
        .map(|(_, rel)| resolve_under_root(&canon_root, rel))
        .collect::<Result<Vec<_>>>()?;
    let sizes: Vec<u64> = resolved
        .iter()
        .map(|r| match r {
            Resolved::File(p) => std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
            Resolved::Missing => 0,
        })
        .collect();
    let total: u64 = sizes.iter().sum();
    let mut base = 0u64;
    cb(0, total);
    let mut results = Vec::with_capacity(entries.len());
    for (((expected, rel), size), res) in entries.iter().zip(&sizes).zip(&resolved) {
        let ok = match res {
            Resolved::Missing => false,
            Resolved::File(path) => {
                match sha256_file(path, &mut |done, _| cb(base + done.min(*size), total)) {
                    Ok(actual) => actual == expected.to_ascii_lowercase(),
                    Err(_) => false,
                }
            }
        };
        base += size;
        cb(base, total);
        results.push((rel.clone(), ok));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn tmp(content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, content).unwrap();
        (dir, p)
    }

    #[test]
    fn hashes_known_vectors() {
        let (_d, p) = tmp(b"");
        assert_eq!(sha256_file(&p, &mut |_, _| {}).unwrap(), EMPTY);
        let (_d, p) = tmp(b"abc");
        assert_eq!(sha256_file(&p, &mut |_, _| {}).unwrap(), ABC);
    }

    #[test]
    fn progress_is_chunked_and_complete() {
        let size = 2 * 1024 * 1024 + 3;
        let (_d, p) = tmp(&vec![7u8; size]);
        let mut calls = Vec::new();
        sha256_file(&p, &mut |done, total| calls.push((done, total))).unwrap();
        assert_eq!(calls.first().copied(), Some((0, size as u64)));
        assert_eq!(calls.last().copied(), Some((size as u64, size as u64)));
        assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0));
        assert!(calls.len() >= 4);
    }

    #[test]
    fn hashing_missing_file_errors() {
        assert!(sha256_file(Path::new("/nonexistent/x"), &mut |_, _| {}).is_err());
    }

    #[test]
    fn write_parse_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("checksums.sha256");
        let entries = vec![
            (ABC.to_string(), "vault.hc".to_string()),
            (EMPTY.to_string(), "parity/vault.hc.par2".to_string()),
        ];
        write_checksums(&entries, &out).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.ends_with('\n'));
        assert!(text.contains(&format!("{ABC}  vault.hc\n")));
        assert_eq!(parse_checksums(&text).unwrap(), entries);
    }

    #[test]
    fn parse_tolerates_newline_variations() {
        let body = format!("{ABC}  a.bin");
        assert_eq!(parse_checksums(&body).unwrap().len(), 1);
        assert_eq!(parse_checksums(&format!("{body}\n")).unwrap().len(), 1);
        assert_eq!(parse_checksums(&format!("{body}\r\n")).unwrap().len(), 1);
        assert_eq!(parse_checksums(&format!("{body}\n\n")).unwrap().len(), 1);
        let upper = format!("{}  a.bin", ABC.to_uppercase());
        assert_eq!(parse_checksums(&upper).unwrap()[0].0, ABC);
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse_checksums("nonsense").is_err());
        assert!(parse_checksums(&format!("{}  short-hex", &ABC[..63])).is_err());
        assert!(parse_checksums(&format!("{}zz  bad-chars", &ABC[..62])).is_err());
        assert!(parse_checksums(&format!("{ABC} single-space")).is_err());
        assert!(parse_checksums(&format!("{ABC}  ")).is_err());
    }

    #[test]
    fn parse_rejects_traversal_paths() {
        for rel in ["/etc/hostname", "../x", "a/../../x", "./x", ".."] {
            let line = format!("{ABC}  {rel}");
            assert!(parse_checksums(&line).is_err(), "accepted {rel:?}");
        }
        assert!(parse_checksums(&format!("{ABC}  parity/a.par2")).is_ok());
    }

    #[test]
    fn verify_bails_on_traversal_before_any_result() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![(ABC.to_string(), "../outside".to_string())];
        assert!(verify_checksums(dir.path(), &entries, &mut |_, _| {}).is_err());
    }

    #[test]
    fn verify_bails_on_symlink_escape() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"abc").unwrap();
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("link"))
            .unwrap();
        let entries = vec![(ABC.to_string(), "link".to_string())];
        let err = verify_checksums(root.path(), &entries, &mut |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("escapes the disc root"), "{err:#}");
    }

    #[test]
    fn dangling_symlink_bails_like_existing_one_no_oracle() {
        // A symlink to a nonexistent path must bail exactly like one to an
        // existing path — otherwise bail-vs-missing leaks host-path existence.
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/no/such/path/xyz-12345", root.path().join("probe")).unwrap();
        let entries = vec![(ABC.to_string(), "probe".to_string())];
        let err = verify_checksums(root.path(), &entries, &mut |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("escapes the disc root"), "{err:#}");
    }

    #[test]
    fn intermediate_symlink_dir_bails() {
        // A symlinked *directory* component must not be descended into either.
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join("d")).unwrap();
        std::fs::write(outside.path().join("d/f"), b"abc").unwrap();
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("d"), root.path().join("d")).unwrap();
        let entries = vec![(ABC.to_string(), "d/f".to_string())];
        let err = verify_checksums(root.path(), &entries, &mut |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("escapes the disc root"), "{err:#}");
    }

    #[test]
    fn verify_accepts_nested_real_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("parity")).unwrap();
        std::fs::write(root.path().join("parity/a.par2"), b"abc").unwrap();
        let entries = vec![(ABC.to_string(), "parity/a.par2".to_string())];
        let res = verify_checksums(root.path(), &entries, &mut |_, _| {}).unwrap();
        assert_eq!(res, vec![("parity/a.par2".to_string(), true)]);
    }

    #[test]
    fn verify_rejects_even_in_root_symlinks() {
        // A real ovenmitts disc never lists a symlink in checksums.sha256, so
        // any symlink is anomalous and rejected uniformly - even one that
        // resolves inside the mount - to keep the no-oracle guarantee simple.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("real"), b"abc").unwrap();
        std::os::unix::fs::symlink("real", root.path().join("alias")).unwrap();
        let entries = vec![(ABC.to_string(), "alias".to_string())];
        let err = verify_checksums(root.path(), &entries, &mut |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("escapes the disc root"), "{err:#}");
    }

    #[test]
    fn verify_reports_ok_bad_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("good"), b"abc").unwrap();
        std::fs::write(dir.path().join("bad"), b"xyz").unwrap();
        let entries = vec![
            (ABC.to_string(), "good".to_string()),
            (ABC.to_string(), "bad".to_string()),
            (ABC.to_string(), "missing".to_string()),
        ];
        let mut last = (0, 0);
        let res = verify_checksums(dir.path(), &entries, &mut |d, t| last = (d, t)).unwrap();
        assert_eq!(
            res,
            vec![
                ("good".to_string(), true),
                ("bad".to_string(), false),
                ("missing".to_string(), false),
            ]
        );
        assert_eq!(last, (6, 6));
    }
}
