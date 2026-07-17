use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fakebin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fakebin")
}

// Per-child env only (no global set_var): tempdir HOME/XDG so the user's
// real config can never leak into a test run.
fn run(dir: &Path, args: &[&str]) -> Output {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut parts = vec![fakebin()];
    parts.extend(std::env::split_paths(&old));
    Command::new(env!("CARGO_BIN_EXE_ovenmitts"))
        .args(args)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("PATH", std::env::join_paths(parts).unwrap())
        .env("OVENMITTS_FAKE_DEVICE", dir.join("fake-device"))
        .output()
        .unwrap()
}

fn error_lines(stderr: &str) -> Vec<&str> {
    stderr.lines().filter(|l| l.starts_with("error:")).collect()
}

#[test]
fn stage_failure_prints_exactly_one_error_line() {
    let dir = tempfile::tempdir().unwrap();
    let device = dir.path().join("fake-device");
    let missing = dir.path().join("missing.iso");
    let out = run(
        dir.path(),
        &[
            "--device",
            device.to_str().unwrap(),
            "verify",
            "--iso",
            missing.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let errors = error_lines(&stderr);
    assert_eq!(errors.len(), 1, "stderr:\n{stderr}");
    assert!(errors[0].contains("[verify image]"), "stderr:\n{stderr}");
}

#[test]
fn info_kv_lines_stay_bare() {
    let dir = tempfile::tempdir().unwrap();
    let device = dir.path().join("fake-device");
    let out = run(dir.path(), &["--device", device.to_str().unwrap(), "info"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l.starts_with("device : ")),
        "stdout:\n{stdout}"
    );
    assert!(!stdout.contains("info: device"), "stdout:\n{stdout}");
}

#[test]
fn config_error_prints_top_level_error_line() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "nope = 1\n").unwrap();
    let out = run(dir.path(), &["--config", cfg.to_str().unwrap(), "info"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let errors = error_lines(&stderr);
    assert_eq!(errors.len(), 1, "stderr:\n{stderr}");
    assert!(errors[0].contains("parsing config"), "stderr:\n{stderr}");
}
