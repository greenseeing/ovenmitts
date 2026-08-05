use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex, MutexGuard};

use ovenmitts::config::Config;
use ovenmitts::hashing;
use ovenmitts::plan;
use ovenmitts::runner::{self, Ack, BurnParams, BurnRequest, RunnerCtx, Stage, StageEvent};
use ovenmitts::tools::Tools;

// Fake tools read OVENMITTS_FAKE_* from the process environment, which is
// global: serialize the tests and keep the guard alive for the whole run.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn fakebin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fakebin")
}

struct Harness {
    _env: MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    device: PathBuf,
    staging: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        std::env::set_var("XDG_DATA_HOME", home.join(".local").join("share"));
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut parts = vec![fakebin()];
        parts.extend(std::env::split_paths(&old).filter(|p| *p != fakebin()));
        std::env::set_var("PATH", std::env::join_paths(parts).unwrap());
        let device = dir.path().join("fake-device");
        std::env::set_var("OVENMITTS_FAKE_DEVICE", &device);
        std::env::remove_var("OVENMITTS_FAKE_MOUNT");
        std::env::remove_var("OVENMITTS_FAKE_BURN_FAIL");
        let staging = dir.path().join("staging");
        Self {
            _env: guard,
            dir,
            device,
            staging,
        }
    }

    fn set_mount_for(&self, iso: &Path) {
        let mut contents = iso.as_os_str().to_os_string();
        contents.push(".contents");
        std::env::set_var("OVENMITTS_FAKE_MOUNT", contents);
    }

    fn tools(&self) -> Tools {
        let bin = fakebin();
        Tools {
            xorriso: bin.join("xorriso"),
            par2: Some(bin.join("par2")),
            par2_version: Some("fake par2".into()),
            udisksctl: Some(bin.join("udisksctl")),
            veracrypt: None,
            eject: Some(bin.join("eject")),
            mediainfo: None,
        }
    }

    fn config(&self) -> Config {
        Config {
            device: self.device.display().to_string(),
            device_explicit: true,
            staging: self.staging.clone(),
            speed: Some(4),
            redundancy_pct: 15,
            headroom_pct: 5,
            defect_management: false,
            keep_iso: true,
            eject_when_done: None,
        }
    }

    fn ctx(&self) -> (RunnerCtx, Receiver<StageEvent>, Sender<Ack>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        (
            RunnerCtx {
                cfg: self.config(),
                tools: self.tools(),
                tx,
                ack_rx,
            },
            rx,
            ack_tx,
        )
    }

    fn payload(&self, name: &str, len: usize, seed: u64) -> PathBuf {
        let p = self.dir.path().join(name);
        std::fs::write(&p, pseudo_random(len, seed)).unwrap();
        p
    }

    fn stage_dir(&self, label: &str) -> PathBuf {
        self.staging.join(label)
    }
}

fn pseudo_random(len: usize, mut state: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(len + 8);
    while v.len() < len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.extend_from_slice(&state.to_le_bytes());
    }
    v.truncate(len);
    v
}

fn burn_request(payloads: Vec<PathBuf>, label: &str) -> BurnRequest {
    BurnRequest {
        payloads,
        label: Some(label.into()),
        parity: true,
        dry_run: false,
        assume_yes: true,
        amend: false,
        discard_iso: false,
    }
}

fn drain(ctx: RunnerCtx, rx: Receiver<StageEvent>) -> Vec<StageEvent> {
    drop(ctx);
    rx.try_iter().collect()
}

fn stage_starts(events: &[StageEvent]) -> Vec<Stage> {
    events
        .iter()
        .filter_map(|ev| match ev {
            StageEvent::StageStart(s) => Some(*s),
            _ => None,
        })
        .collect()
}

fn stage_dones(events: &[StageEvent]) -> Vec<Stage> {
    events
        .iter()
        .filter_map(|ev| match ev {
            StageEvent::StageDone { stage, .. } => Some(*stage),
            _ => None,
        })
        .collect()
}

fn sha256_of(path: &Path) -> String {
    hashing::sha256_file(path, &mut |_, _| {}).unwrap()
}

fn par2_files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "par2") {
                found.push(p);
            }
        }
    }
    found
}

const BURN_STAGES: [Stage; 7] = [
    Stage::Preflight,
    Stage::Parity,
    Stage::Checksums,
    Stage::Master,
    Stage::Burn,
    Stage::VerifyImage,
    Stage::VerifyFiles,
];

#[test]
fn burn_failure_reports_cause_and_retry_hint() {
    let h = Harness::new();
    std::env::set_var("OVENMITTS_FAKE_BURN_FAIL", "1");
    let payload = h.payload("vault.hc", 8 * 1024 * 1024, 42);
    let stage_dir = h.stage_dir("PIPE");
    let iso = stage_dir.join("PIPE.iso");
    h.set_mount_for(&iso);

    let (ctx, rx, _ack) = h.ctx();
    let err = runner::run_burn(&ctx, &burn_request(vec![payload], "PIPE")).unwrap_err();
    std::env::remove_var("OVENMITTS_FAKE_BURN_FAIL");
    let events = drain(ctx, rx);

    assert!(format!("{err:#}").contains("libburn indicates failure"));
    let Some(StageEvent::Failed { stage, error }) = events.last() else {
        panic!("expected Failed last, got {:?}", events.last());
    };
    assert_eq!(*stage, Stage::Burn);
    assert!(error.contains("libburn indicates failure"), "{error}");
    assert!(!error.contains("patient"), "{error}");

    let infos: Vec<&String> = events
        .iter()
        .filter_map(|ev| match ev {
            StageEvent::Info(t) => Some(t),
            _ => None,
        })
        .collect();
    assert!(infos
        .iter()
        .any(|t| t.contains("burn transcript") && t.contains("PIPE.burn.log")));
    assert!(infos
        .iter()
        .any(|t| t.contains(&format!("burn-iso {}", iso.display()))));

    let log = std::fs::read_to_string(stage_dir.join("PIPE.burn.log")).unwrap();
    assert!(log.contains("patient"), "transcript must keep keepalives");
    assert!(log.contains("libburn indicates failure with writing"));
}

#[test]
fn burn_pipeline_dir_and_file() {
    let h = Harness::new();
    let extras = h.dir.path().join("extras");
    std::fs::create_dir_all(extras.join("sub")).unwrap();
    std::fs::write(extras.join("a.bin"), pseudo_random(64 * 1024, 7)).unwrap();
    std::fs::write(extras.join("empty.bin"), b"").unwrap();
    std::fs::write(
        extras.join("sub").join("b.bin"),
        pseudo_random(32 * 1024, 8),
    )
    .unwrap();
    std::os::unix::fs::symlink("a.bin", extras.join("link_a")).unwrap();
    let vault = h.payload("vault.hc", 4 * 1024 * 1024, 42);

    let stage_dir = h.stage_dir("MIXED");
    let iso = stage_dir.join("MIXED.iso");
    h.set_mount_for(&iso);

    let (ctx, rx, _ack) = h.ctx();
    runner::run_burn(
        &ctx,
        &burn_request(vec![extras.clone(), vault.clone()], "MIXED"),
    )
    .unwrap();
    let events = drain(ctx, rx);

    assert_eq!(stage_starts(&events), BURN_STAGES);
    assert_eq!(stage_dones(&events), BURN_STAGES);

    let warns: Vec<&String> = events
        .iter()
        .filter_map(|ev| match ev {
            StageEvent::Warn(t) => Some(t),
            _ => None,
        })
        .collect();
    assert!(warns.iter().any(|t| t.contains("link_a")), "{warns:?}");
    assert!(warns.iter().any(|t| t.contains("empty.bin")), "{warns:?}");

    let checksums = std::fs::read_to_string(stage_dir.join("checksums.sha256")).unwrap();
    let entries = hashing::parse_checksums(&checksums).unwrap();
    let rels: Vec<&str> = entries.iter().map(|(_, rel)| rel.as_str()).collect();
    assert_eq!(
        rels,
        vec![
            "extras/a.bin",
            "extras/empty.bin",
            "extras/sub/b.bin",
            "vault.hc",
            "parity/extras.par2",
            "parity/extras.vol000+01.par2",
            "parity/vault.hc.par2",
            "parity/vault.hc.vol000+01.par2",
        ]
    );
    assert_eq!(entries[0].0, sha256_of(&extras.join("a.bin")));
    assert_eq!(entries[2].0, sha256_of(&extras.join("sub").join("b.bin")));
    assert_eq!(entries[3].0, sha256_of(&vault));

    let argv = std::fs::read_to_string(stage_dir.join("parity").join("extras.par2.argv")).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    let parent = h.dir.path().canonicalize().unwrap();
    assert_eq!(PathBuf::from(lines[0]).canonicalize().unwrap(), parent);
    let base = lines
        .iter()
        .find_map(|l| l.strip_prefix("-B"))
        .expect("par2 argv must pin the basepath with -B");
    assert_eq!(PathBuf::from(base).canonicalize().unwrap(), parent);
    assert!(lines.contains(&"extras/a.bin"), "argv: {argv}");
    assert!(lines.contains(&"extras/sub/b.bin"), "argv: {argv}");
    assert!(
        !lines.iter().any(|l| l.contains("empty.bin")),
        "argv: {argv}"
    );
    assert!(!lines.iter().any(|l| l.contains("link_a")), "argv: {argv}");
    assert_eq!(par2_files_under(&stage_dir.join("parity")).len(), 4);

    let recovery = std::fs::read_to_string(stage_dir.join("RECOVERY.txt")).unwrap();
    assert!(
        recovery.lines().any(|l| l.trim() == "cp -r /mnt/extras ."),
        "{recovery}"
    );
    assert!(
        recovery
            .lines()
            .any(|l| l.trim() == "par2 r -B. /mnt/parity/extras.par2"),
        "{recovery}"
    );
    assert!(
        recovery
            .lines()
            .any(|l| l.trim() == "par2 r -B. /mnt/parity/vault.hc.par2 vault.hc"),
        "{recovery}"
    );
    assert!(
        !recovery.contains("veracrypt --text --mount-options ro /mnt/extras"),
        "{recovery}"
    );
    assert!(
        recovery.contains("veracrypt --text --mount-options ro /mnt/vault.hc"),
        "{recovery}"
    );

    let manifest = std::fs::read_to_string(stage_dir.join("MANIFEST.txt")).unwrap();
    assert!(manifest.contains("extras/"), "{manifest}");
    assert!(manifest.contains("files: 3"), "{manifest}");
}

#[test]
fn run_plan_accepts_directory() {
    let h = Harness::new();
    let extras = h.dir.path().join("extras");
    std::fs::create_dir_all(extras.join("sub")).unwrap();
    std::fs::write(extras.join("a.bin"), pseudo_random(100_000, 3)).unwrap();
    std::fs::write(extras.join("sub").join("b.bin"), pseudo_random(50_000, 4)).unwrap();
    let vault = h.payload("vault.hc", 200_000, 5);

    let (ctx, rx, _ack) = h.ctx();
    runner::run_plan(&ctx, &[extras, vault], Some("bd25")).unwrap();
    let events = drain(ctx, rx);
    let payload_bytes = events
        .iter()
        .find_map(|ev| match ev {
            StageEvent::Plan { plan, .. } => Some(plan.payload_bytes),
            _ => None,
        })
        .expect("no Plan event");
    assert_eq!(payload_bytes, 350_000);
    assert!(matches!(events.last(), Some(StageEvent::Finished { .. })));
}

#[test]
fn burn_pipeline_end_to_end() {
    let h = Harness::new();
    let payload = h.payload("vault.hc", 8 * 1024 * 1024, 42);
    let stage_dir = h.stage_dir("PIPE");
    let iso = stage_dir.join("PIPE.iso");
    h.set_mount_for(&iso);

    let (ctx, rx, _ack) = h.ctx();
    runner::run_burn(&ctx, &burn_request(vec![payload.clone()], "PIPE")).unwrap();
    let events = drain(ctx, rx);

    assert_eq!(stage_starts(&events), BURN_STAGES);
    assert_eq!(stage_dones(&events), BURN_STAGES);
    let Some(StageEvent::Finished { report }) = events.last() else {
        panic!("expected Finished last, got {:?}", events.last());
    };
    assert_eq!(
        report.stages.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        BURN_STAGES
    );

    let index = stage_dir.join("parity").join("vault.hc.par2");
    let volume = stage_dir.join("parity").join("vault.hc.vol000+01.par2");
    assert!(index.is_file(), "missing {}", index.display());
    assert!(volume.is_file(), "missing {}", volume.display());

    let argv =
        std::fs::read_to_string(stage_dir.join("parity").join("vault.hc.par2.argv")).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    assert_eq!(
        PathBuf::from(lines[0]).canonicalize().unwrap(),
        payload.parent().unwrap().canonicalize().unwrap()
    );
    let slice = plan::slice_bytes_for(8 * 1024 * 1024, 1);
    assert_eq!(slice, 65536);
    assert!(lines.contains(&"create"));
    let base = lines
        .iter()
        .find_map(|l| l.strip_prefix("-B"))
        .expect("par2 argv must pin the basepath with -B");
    assert_eq!(
        PathBuf::from(base).canonicalize().unwrap(),
        payload.parent().unwrap().canonicalize().unwrap()
    );
    assert!(lines.contains(&"-r15"));
    assert!(lines.contains(&"-n1"));
    assert!(
        lines.contains(&format!("-s{slice}").as_str()),
        "argv: {argv}"
    );
    assert!(lines.iter().any(|l| l.starts_with("-m")));
    assert_eq!(lines.last(), Some(&"vault.hc"));

    let checksums = std::fs::read_to_string(stage_dir.join("checksums.sha256")).unwrap();
    let entries = hashing::parse_checksums(&checksums).unwrap();
    let rels: Vec<&str> = entries.iter().map(|(_, rel)| rel.as_str()).collect();
    assert_eq!(
        rels,
        vec![
            "vault.hc",
            "parity/vault.hc.par2",
            "parity/vault.hc.vol000+01.par2"
        ]
    );
    assert_eq!(entries[0].0, sha256_of(&payload));
    assert_eq!(entries[1].0, sha256_of(&index));

    let manifest = std::fs::read_to_string(stage_dir.join("MANIFEST.txt")).unwrap();
    assert!(manifest.contains("vault.hc"));
    assert!(manifest.contains("volume label: PIPE"));
    let recovery = std::fs::read_to_string(stage_dir.join("RECOVERY.txt")).unwrap();
    assert!(recovery.contains("vault.hc"));
    let lba = std::fs::read_to_string(stage_dir.join("PIPE.lba.txt")).unwrap();
    assert!(lba.contains("File data lba:"));

    let iso_sha = sha256_of(&iso);
    let device_sha = sha256_of(&h.device);
    assert_eq!(
        device_sha, iso_sha,
        "fake burn must copy the ISO bit-for-bit"
    );
    let cdrecord =
        std::fs::read_to_string(format!("{}.cdrecord_argv", h.device.display())).unwrap();
    assert!(
        cdrecord.lines().any(|l| l == "speed=4"),
        "config speed must reach cdrecord: {cdrecord}"
    );
    assert_eq!(report.iso_sha256.as_deref(), Some(iso_sha.as_str()));
    assert_eq!(report.iso_path.as_deref(), Some(iso.as_path()));
    assert_eq!(report.iso_bytes, std::fs::metadata(&iso).unwrap().len());
    let sidecar = std::fs::read_to_string(stage_dir.join("PIPE.iso.sha256")).unwrap();
    let (recorded, _) = hashing::parse_checksums(&sidecar).unwrap().remove(0);
    assert_eq!(recorded, device_sha);

    assert!(!report.reminders.is_empty());
    assert!(report.reminders.iter().any(|r| r.contains("burn-iso")));
    assert!(report.reminders.iter().any(|r| r.contains("off-disc")));
    assert!(report.reminders.iter().any(|r| r.contains("VeraCrypt")));

    let expected_written = vec![
        stage_dir.join("parity").join("vault.hc.par2"),
        stage_dir.join("parity").join("vault.hc.vol000+01.par2"),
        stage_dir.join("checksums.sha256"),
        stage_dir.join("MANIFEST.txt"),
        stage_dir.join("RECOVERY.txt"),
        iso.clone(),
        stage_dir.join("PIPE.lba.txt"),
        stage_dir.join("PIPE.iso.sha256"),
        stage_dir.join("PIPE.burn.log"),
    ];
    assert_eq!(report.written_files, expected_written);
    for f in &report.written_files {
        assert!(f.is_file(), "reported file missing: {}", f.display());
    }

    assert!(events.iter().any(|ev| matches!(
        ev,
        StageEvent::Progress { stage: Stage::Master, pct: Some(p), .. } if (*p - 50.0).abs() < 0.01
    )));
    assert!(events.iter().any(|ev| matches!(
        ev,
        StageEvent::Progress { stage: Stage::Burn, pct: Some(p), .. } if *p == 100.0
    )));
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, StageEvent::Failed { .. })));
}

#[test]
fn burn_pipeline_multi_payload() {
    let h = Harness::new();
    let vault = h.payload("vault.hc", 8 * 1024 * 1024, 1);
    let notes = h.payload("notes.bin", 3 * 1024 * 1024, 2);
    let stage_dir = h.stage_dir("MULTI");
    let iso = stage_dir.join("MULTI.iso");
    h.set_mount_for(&iso);

    let (ctx, rx, _ack) = h.ctx();
    runner::run_burn(
        &ctx,
        &burn_request(vec![vault.clone(), notes.clone()], "MULTI"),
    )
    .unwrap();
    let events = drain(ctx, rx);

    assert_eq!(stage_starts(&events), BURN_STAGES);
    let Some(StageEvent::Finished { report }) = events.last() else {
        panic!("expected Finished last, got {:?}", events.last());
    };

    let checksums = std::fs::read_to_string(stage_dir.join("checksums.sha256")).unwrap();
    let entries = hashing::parse_checksums(&checksums).unwrap();
    let rels: Vec<&str> = entries.iter().map(|(_, rel)| rel.as_str()).collect();
    assert_eq!(
        rels,
        vec![
            "vault.hc",
            "notes.bin",
            "parity/vault.hc.par2",
            "parity/vault.hc.vol000+01.par2",
            "parity/notes.bin.par2",
            "parity/notes.bin.vol000+01.par2",
        ]
    );
    assert_eq!(entries[0].0, sha256_of(&vault));
    assert_eq!(entries[1].0, sha256_of(&notes));

    let manifest = std::fs::read_to_string(stage_dir.join("MANIFEST.txt")).unwrap();
    assert!(manifest.contains("vault.hc"));
    assert!(manifest.contains("notes.bin"));

    assert_eq!(par2_files_under(&stage_dir.join("parity")).len(), 4);
    assert_eq!(report.iso_bytes, std::fs::metadata(&iso).unwrap().len());
    assert_eq!(
        report.iso_sha256.as_deref(),
        Some(sha256_of(&h.device).as_str())
    );
}

#[test]
fn amended_params_flow_through_whole_pipeline() {
    let h = Harness::new();
    let payload = h.payload("vault.hc", 8 * 1024 * 1024, 77);
    let stage_dir = h.stage_dir("AMENDED");
    let iso = stage_dir.join("AMENDED.iso");
    h.set_mount_for(&iso);

    let (ctx, rx, ack_tx) = h.ctx();
    ack_tx
        .send(Ack::Amend(BurnParams {
            label: "amended".into(),
            speed: Some(8),
            redundancy_pct: 30,
            parity: true,
            defect_management: false,
            staging: h.staging.clone(),
        }))
        .unwrap();
    ack_tx.send(Ack::Proceed).unwrap();
    let mut req = burn_request(vec![payload], "ORIG");
    req.assume_yes = false;
    req.amend = true;
    runner::run_burn(&ctx, &req).unwrap();
    let events = drain(ctx, rx);

    assert_eq!(stage_starts(&events), BURN_STAGES);
    let plans: Vec<&BurnParams> = events
        .iter()
        .filter_map(|ev| match ev {
            StageEvent::Plan { params, .. } => Some(params),
            _ => None,
        })
        .collect();
    assert_eq!(plans.len(), 2, "amend must re-plan through the runner");
    assert_eq!(plans[0].label, "ORIG");
    assert_eq!(plans[1].label, "AMENDED");
    assert_eq!(plans[1].redundancy_pct, 30);

    assert!(iso.is_file());
    assert!(
        !h.stage_dir("ORIG").exists(),
        "the pre-amend label must never be staged"
    );
    let argv =
        std::fs::read_to_string(stage_dir.join("parity").join("vault.hc.par2.argv")).unwrap();
    assert!(argv.lines().any(|l| l == "-r30"), "par2 argv: {argv}");
    let cdrecord =
        std::fs::read_to_string(format!("{}.cdrecord_argv", h.device.display())).unwrap();
    assert!(
        cdrecord.lines().any(|l| l == "speed=8"),
        "cdrecord argv: {cdrecord}"
    );
}

#[test]
fn amended_staging_redirects_stage_dir() {
    let h = Harness::new();
    let payload = h.payload("vault.hc", 8 * 1024 * 1024, 99);
    let alt = h.dir.path().join("alt-staging");
    let stage_dir = alt.join("MOVED");
    let iso = stage_dir.join("MOVED.iso");
    h.set_mount_for(&iso);

    let (ctx, rx, ack_tx) = h.ctx();
    ack_tx
        .send(Ack::Amend(BurnParams {
            label: "MOVED".into(),
            speed: Some(4),
            redundancy_pct: 15,
            parity: true,
            defect_management: false,
            staging: alt.clone(),
        }))
        .unwrap();
    ack_tx.send(Ack::Proceed).unwrap();
    let mut req = burn_request(vec![payload], "MOVED");
    req.assume_yes = false;
    req.amend = true;
    runner::run_burn(&ctx, &req).unwrap();
    let events = drain(ctx, rx);

    assert!(matches!(events.last(), Some(StageEvent::Finished { .. })));
    assert!(iso.is_file(), "ISO must land in the amended staging dir");
    assert!(
        !h.stage_dir("MOVED").exists(),
        "the config staging dir must stay untouched"
    );
}

#[test]
fn discard_iso_drops_iso_from_written_files_but_keeps_sidecars() {
    let h = Harness::new();
    let payload = h.payload("vault.hc", 8 * 1024 * 1024, 7);
    let stage_dir = h.stage_dir("TOSS");
    let iso = stage_dir.join("TOSS.iso");
    h.set_mount_for(&iso);

    let (ctx, rx, _ack) = h.ctx();
    let mut req = burn_request(vec![payload], "TOSS");
    req.discard_iso = true;
    runner::run_burn(&ctx, &req).unwrap();
    let events = drain(ctx, rx);

    let Some(StageEvent::Finished { report }) = events.last() else {
        panic!("expected Finished last, got {:?}", events.last());
    };
    assert!(!iso.exists(), "discard_iso must remove the staged ISO");
    assert_eq!(report.iso_path, None);
    assert!(
        !report.written_files.contains(&iso),
        "a discarded ISO must not be reported as written"
    );
    assert!(report
        .written_files
        .contains(&stage_dir.join("TOSS.iso.sha256")));
    assert!(report
        .written_files
        .contains(&stage_dir.join("TOSS.burn.log")));
    for f in &report.written_files {
        assert!(f.is_file(), "reported file missing: {}", f.display());
    }
}

#[test]
fn preflight_rejects_oversized_payload_before_parity() {
    let h = Harness::new();
    let big = h.dir.path().join("big.hc");
    let f = std::fs::File::create(&big).unwrap();
    f.set_len(26 * 1024 * 1024 * 1024).unwrap();

    let (ctx, rx, _ack) = h.ctx();
    let err = runner::run_burn(&ctx, &burn_request(vec![big], "BIG")).unwrap_err();
    let events = drain(ctx, rx);

    let msg = format!("{err:#}");
    assert!(msg.contains("does not fit"), "{msg}");
    assert!(msg.contains("bytes"), "{msg}");
    assert_eq!(stage_starts(&events), vec![Stage::Preflight]);
    assert!(matches!(
        events.last(),
        Some(StageEvent::Failed {
            stage: Stage::Preflight,
            ..
        })
    ));
    assert!(
        par2_files_under(&h.staging).is_empty(),
        "no parity work before preflight passes"
    );
    assert!(!h.stage_dir("BIG").exists());
}

#[test]
fn run_verify_passes_against_burned_device() {
    let h = Harness::new();
    let payload = h.payload("vault.hc", 8 * 1024 * 1024, 9);
    let stage_dir = h.stage_dir("VER");
    let iso = stage_dir.join("VER.iso");
    h.set_mount_for(&iso);

    let (ctx, rx, _ack) = h.ctx();
    runner::run_burn(&ctx, &burn_request(vec![payload], "VER")).unwrap();
    drain(ctx, rx);

    let (ctx, rx, _ack) = h.ctx();
    runner::run_verify(&ctx, Some(&iso)).unwrap();
    let events = drain(ctx, rx);

    let expected = vec![Stage::VerifyImage, Stage::VerifyFiles, Stage::CheckMedia];
    assert_eq!(stage_starts(&events), expected);
    assert_eq!(stage_dones(&events), expected);
    let Some(StageEvent::Finished { report }) = events.last() else {
        panic!("expected Finished last, got {:?}", events.last());
    };
    assert_eq!(report.iso_sha256.as_deref(), Some(sha256_of(&iso).as_str()));
    assert_eq!(report.iso_bytes, std::fs::metadata(&iso).unwrap().len());
    assert!(events
        .iter()
        .any(|ev| matches!(ev, StageEvent::Info(t) if t.contains("recorded sha256"))));
    assert!(events.iter().any(|ev| matches!(
        ev,
        StageEvent::StageDone { stage: Stage::CheckMedia, summary } if summary.contains("clean")
    )));
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, StageEvent::Failed { .. })));
}

#[test]
fn run_verify_without_iso_is_labeled_advisory() {
    let h = Harness::new();
    let payload = h.payload("vault.hc", 8 * 1024 * 1024, 21);
    let stage_dir = h.stage_dir("ADVISORY");
    let iso = stage_dir.join("ADVISORY.iso");
    h.set_mount_for(&iso);

    let (ctx, rx, _ack) = h.ctx();
    runner::run_burn(&ctx, &burn_request(vec![payload], "ADVISORY")).unwrap();
    drain(ctx, rx);

    // No --iso: checks the disc against its own on-disc checksums only. Must
    // finish, but must record a caveat that this is not source verification.
    let (ctx, rx, _ack) = h.ctx();
    runner::run_verify(&ctx, None).unwrap();
    let events = drain(ctx, rx);

    let Some(StageEvent::Finished { report }) = events.last() else {
        panic!("expected Finished last, got {:?}", events.last());
    };
    assert!(
        report.degradations.iter().any(|d| d.contains("no --iso")),
        "expected an advisory caveat, got {:?}",
        report.degradations
    );
    // The image read-back stage must not run without an ISO to compare against.
    assert!(!stage_starts(&events).contains(&Stage::VerifyImage));
    assert!(events
        .iter()
        .any(|ev| matches!(ev, StageEvent::Warn(t) if t.contains("no --iso"))));
}

#[test]
fn run_verify_refuses_disc_with_traversal_checksums() {
    let h = Harness::new();
    let payload = h.payload("vault.hc", 8 * 1024 * 1024, 13);
    let stage_dir = h.stage_dir("EVIL");
    let iso = stage_dir.join("EVIL.iso");
    h.set_mount_for(&iso);

    let (ctx, rx, _ack) = h.ctx();
    runner::run_burn(&ctx, &burn_request(vec![payload], "EVIL")).unwrap();
    drain(ctx, rx);

    // A crafted disc names a host path outside the mount. The verifier must
    // abort before emitting any per-file result (no host-file existence oracle).
    let mount = PathBuf::from(format!("{}.contents", iso.display()));
    std::fs::write(
        mount.join("checksums.sha256"),
        format!("{}  ../../../../etc/hostname\n", "0".repeat(64)),
    )
    .unwrap();

    let (ctx, rx, _ack) = h.ctx();
    let err = runner::run_verify(&ctx, Some(&iso)).unwrap_err();
    let events = drain(ctx, rx);

    assert!(
        format!("{err:#}").contains("plain relative path"),
        "{err:#}"
    );
    assert!(
        !stage_dones(&events).contains(&Stage::VerifyFiles),
        "must not complete file verification on a tampered disc"
    );
    assert!(matches!(
        events.last(),
        Some(StageEvent::Failed {
            stage: Stage::VerifyFiles,
            ..
        })
    ));
}
