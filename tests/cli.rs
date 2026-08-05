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
    run_with_env(dir, args, &[])
}

fn run_with_env(dir: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
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
        .envs(extra_env.iter().copied())
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
fn info_save_writes_config_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let device = dir.path().join("fake-device");
    let cfg = dir.path().join("config.toml");
    let out = run(
        dir.path(),
        &[
            "--device",
            device.to_str().unwrap(),
            "--config",
            cfg.to_str().unwrap(),
            "info",
            "--save",
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains(device.to_str().unwrap()), "config:\n{text}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("info: saved device"), "stdout:\n{stdout}");
}

// stdout is piped here, so the interactive picker must never launch: bare
// invocations keep the "nothing to do" bail in scripts and CI.
#[test]
fn bare_invocation_without_tty_bails_with_nothing_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nothing to do"), "stderr:\n{stderr}");
}

#[test]
fn no_tui_without_payloads_bails_with_nothing_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["--no-tui"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nothing to do"), "stderr:\n{stderr}");
}

// keep_iso = false in the config was documented but dead: the staged ISO was
// always kept. It must behave like --discard-iso after a verified burn.
#[test]
fn config_keep_iso_false_discards_iso_after_verification() {
    let dir = tempfile::tempdir().unwrap();
    let payload = dir.path().join("vault.hc");
    std::fs::write(&payload, vec![7u8; 1024 * 1024]).unwrap();
    let device = dir.path().join("fake-device");
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "keep_iso = false\n").unwrap();
    let staging = dir.path().join("staging");
    let iso = staging.join("KEEPCFG").join("KEEPCFG.iso");
    let mut contents = iso.clone().into_os_string();
    contents.push(".contents");

    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut parts = vec![fakebin()];
    parts.extend(std::env::split_paths(&old));
    let out = Command::new(env!("CARGO_BIN_EXE_ovenmitts"))
        .args([
            "--device",
            device.to_str().unwrap(),
            "--config",
            cfg.to_str().unwrap(),
            "--staging",
            staging.to_str().unwrap(),
            "burn",
            "--yes",
            "--label",
            "KEEPCFG",
            payload.to_str().unwrap(),
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("PATH", std::env::join_paths(parts).unwrap())
        .env("OVENMITTS_FAKE_DEVICE", &device)
        .env("OVENMITTS_FAKE_MOUNT", &contents)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !iso.exists(),
        "keep_iso = false must discard the verified ISO"
    );
    assert!(
        staging.join("KEEPCFG").join("checksums.sha256").is_file(),
        "sidecars must survive the discard"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("discarded"), "stdout:\n{stdout}");
}

// A signal while parked at the [Y/n] confirm prompt must abort cleanly.
// Regression test: the prompt used to block the signal-polling thread on a
// raw stdin read (SA_RESTART), so SIGTERM at a prompt hung ovenmitts forever.
#[test]
fn sigterm_at_confirm_prompt_aborts_instead_of_hanging() {
    use std::io::Read as _;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let payload = dir.path().join("vault.hc");
    std::fs::write(&payload, vec![7u8; 1024 * 1024]).unwrap();
    let device = dir.path().join("fake-device");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut parts = vec![fakebin()];
    parts.extend(std::env::split_paths(&old));
    let mut child = Command::new(env!("CARGO_BIN_EXE_ovenmitts"))
        .args([
            "--no-tui",
            "--device",
            device.to_str().unwrap(),
            payload.to_str().unwrap(),
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("PATH", std::env::join_paths(parts).unwrap())
        .env("OVENMITTS_FAKE_DEVICE", &device)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // held open, never written: an attended-but-idle terminal, not an EOF
    let stdin = child.stdin.take().unwrap();

    let mut stdout = child.stdout.take().unwrap();
    let (prompt_tx, prompt_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    seen.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&seen).contains("[Y/n]") {
                        let _ = prompt_tx.send(());
                        // keep draining so the child never blocks on the pipe
                        while matches!(stdout.read(&mut buf), Ok(n) if n > 0) {}
                        break;
                    }
                }
            }
        }
    });
    prompt_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("confirm prompt never appeared");

    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("ovenmitts hung at the confirm prompt after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success(), "signal exit must be nonzero");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("signal received"), "stderr:\n{stderr}");
    drop(stdin);
}

// An oversized single-file payload is not a refusal: `plan` prints the
// multi-disc span table and exits 0.
#[test]
fn plan_prints_span_table_for_oversized_payload() {
    let dir = tempfile::tempdir().unwrap();
    let payload = dir.path().join("vault.hc");
    let f = std::fs::File::create(&payload).unwrap();
    f.set_len(100 * 1024 * 1024).unwrap();
    let device = dir.path().join("fake-device");
    let out = run_with_env(
        dir.path(),
        &[
            "--device",
            device.to_str().unwrap(),
            "plan",
            payload.to_str().unwrap(),
        ],
        &[("OVENMITTS_FAKE_MEDIA", "small")],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("set:"), "stdout:\n{stdout}");
    assert!(stdout.contains("disc 1/"), "stdout:\n{stdout}");
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
