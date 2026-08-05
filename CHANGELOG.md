# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Every burn leaves a persistent run record.** Stage transitions, warnings,
  failures and decile progress steps tee to `run.log` in the staging dir
  (append-mode, one dated header per run), and a successful burn writes
  `<label>.report.txt`: the stage summaries plus provenance — ovenmitts
  version, tool paths, and the par2 version banner. `burn-iso` writes both
  next to the ISO. Until now a crash or closed terminal erased the whole
  story of a burn.
- **Stale staging dirs are flagged, never deleted.** Preflight lists earlier
  run dirs older than 30 days (or big enough to crowd out the current plan)
  with a reminder to remove ones already burned and verified. The staging
  free-space check also re-runs right before mastering — the ISO is the big
  late allocation, and space that was free at confirm time may be gone.
- **Inactivity watchdog for streaming tools.** A wedged drive used to hang
  ovenmitts forever (a burn/format/master would wait on xorriso with no
  timeout). Streaming operations now abort if a tool produces no output for
  `stall_timeout_secs` seconds (new config key, default 900 = 15 min; `0`
  disables). Healthy tools emit keepalives every second, so this only trips on
  a genuinely stuck drive; a softer "still working" note appears after 2 min of
  silence. Short one-shot probes (`xorriso -toc`, `dvd+rw-mediainfo`, `par2 -V`,
  `veracrypt --list`, the LBA report) get a fixed 120 s deadline.
- CI: `ci.yml` runs tests, `clippy -D warnings`, `rustfmt --check`, an MSRV
  (1.80) check, and `shellcheck`/`bash -n` on `install.sh` for every push and
  PR; `audit.yml` runs `cargo audit` weekly. Release binaries now carry signed
  build-provenance attestations (`gh attestation verify`).

### Fixed

- **`keep_iso = false` in the config works now.** The key was documented and
  accepted but never read, so the staged ISO was always kept. It now behaves
  exactly like `--discard-iso`: the ISO is removed after a fully verified
  burn (sidecars, parity and logs are kept). Users who set it start getting
  the promised behavior.
- **Recovery-critical files survive a crash.** `checksums.sha256`,
  `MANIFEST.txt`, `RECOVERY.txt`, the LBA map, the ISO sha256 sidecar, the
  mastered ISO and the saved config are now fsynced (file and parent
  directory) instead of relying on the page cache getting flushed eventually.
- **A burn retry no longer destroys the failed attempt's transcript.**
  `<label>.burn.log` appends under a dated `=== burn attempt … ===` header
  per attempt instead of truncating; the log grows across retries.
- **Verify failures say what is actually wrong per file.** A hash mismatch
  ("bad burn — re-burn it") is now reported differently from a read error
  ("failing medium — ddrescue now, see RECOVERY.txt") and from a file missing
  off the disc; previously all three collapsed into one undifferentiated
  failure list.
- **A signal at the `[Y/n]` prompt aborts cleanly instead of hanging.** The
  confirm prompt used to block the thread that polls the shutdown flag, so
  SIGTERM/SIGINT/SIGHUP delivered while ovenmitts waited for an answer was
  silently swallowed (the kernel restarts the interrupted read) and the run
  hung forever. The prompt now keeps polling and runs the same graceful
  tool-terminating shutdown as everywhere else.
- **Signals shut down cleanly instead of orphaning the burn.** SIGTERM/SIGHUP
  (terminal closed, `systemctl stop`) used to kill ovenmitts instantly and
  leave the burning xorriso running; line-mode Ctrl-C did no cleanup. All three
  now run the same graceful sequence the TUI already used for Ctrl-C — abort the
  runner, SIGTERM the tools, wait, then SIGKILL — and report that the disc may
  be partially written. A main-thread panic also terminates running tools now,
  and a TUI draw error mid-burn no longer orphans the tool.
- **Tools are never orphaned on error or panic.** Every external process is now
  owned by a guard that kills and reaps it on any early return or unwind, so a
  burning xorriso can't be left running when ovenmitts exits abnormally. The
  guard also unregisters a child before waiting on it, closing a window where a
  force-quit could signal a recycled PID.
- **Verification fails closed when the page cache can't be defeated.** With
  `verify --iso`, if `eject` is unavailable *and* the O_DIRECT read-back also
  fails, ovenmitts previously fell back to a buffered read that could be served
  from cache and still print PASS. It now hard-errors in that case. A buffered
  read-back that *did* follow a physical disc reload is allowed but recorded as
  a caveat (shown in the report, in both line and TUI modes), and each
  VerifyImage summary states whether the cache was defeated by O_DIRECT or by
  the reload.
- **`verify` without `--iso` is labeled advisory.** That mode checks the disc
  against its own on-disc checksums (media-decay detection), not against the
  source; it now says so via a warning and a recorded caveat.
- **`check_media` errors fail the verify.** A `verify` run that couldn't run its
  media check previously reported the stage as "skipped" and still finished
  successfully — false confidence. It now fails the run (matching `check`).
- **Mounted-VeraCrypt-container detection now fails closed.** The preflight
  check that refuses to burn a mounted container previously treated *any*
  `veracrypt --list` failure (a spawn error, an unexpected exit) as "nothing
  mounted" and proceeded — so a transient failure could let a live container be
  archived as silent garbage. It now distinguishes VeraCrypt's normal
  "No volumes mounted" (exit 1) empty case from real failures, and refuses the
  burn when mount state cannot be determined.
- **External tools run under `LC_ALL=C`.** Progress and capacity parsing keyed
  on `.`-decimal numbers and English keywords (`38.2%`, `23.3g`, `MB written`);
  under a non-C locale the tools emit `38,2%` and localized words, silently
  freezing progress bars and — worse — misreading capacity into the fit check.
  Every external command is now spawned through one helper that pins the C
  locale.

### Security

- **Disc verification refuses path-traversal and symlink escapes.** `ovenmitts
  verify` reads `checksums.sha256` off the mounted disc, which is untrusted
  input. Entries whose path is absolute, contains `..`, or resolves (via a Rock
  Ridge symlink on the disc) outside the mount now abort the run instead of
  hashing an arbitrary host file — closing a file-existence oracle. Malicious
  entries fail the whole verify rather than reporting a per-file result.
- **`ovenmitts update` no longer pipes a shell script into bash.** It was
  re-fetching `install.sh` from the mutable `main` branch and running it, so any
  push to `main` was code execution on every updater. Update is now entirely
  in-process: it downloads the release binary for the host arch over hardened
  TLS (`--proto =https --proto-redir =https --tlsv1.2`, no shell), verifies it
  against the published SHA-256 sidecar before installing, and swaps the binary
  with an atomic same-filesystem rename. It never escalates privileges and never
  executes a fetched script. `OVENMITTS_VERSION` still pins a version (now
  validated as `MAJOR.MINOR.PATCH`).
- **`install.sh` hardened.** Staging files are created with `mktemp` (O_EXCL,
  unpredictable, 0600) instead of a predictable `$bindir/.ovenmitts.new.$$`, so
  the download and the root-side `cp` can no longer be redirected through a
  planted symlink. The GitHub release tag is shape-validated before it reaches a
  URL/path, the release-API call fails loudly instead of silently, and the
  upgrade decision is made by hashing the installed binary against the published
  sidecar rather than by executing the on-PATH binary.

### Changed

- Supply-chain hardening for GitHub Actions: workflow actions and the build
  container are pinned by commit SHA / image digest, `permissions` are
  least-privilege (read-only by default, write scoped to the release job), and
  `dependabot.yml` keeps the pins current.

## [0.1.8] - 2026-07-24

No functional changes to ovenmitts itself — this release moves where it lives.

### Changed

- ovenmitts is now hosted at [github.com/greenseeing/ovenmitts](https://github.com/greenseeing/ovenmitts);
  Codeberg's July 2026 Terms of Use disallow projects mostly written with
  generative-AI tools, so the repo, releases, and CI moved. Releases are now
  built by GitHub Actions from the same pinned `cargo-zigbuild` image, with the
  same asset names and per-file SHA-256 sums.
- `install.sh` and `ovenmitts update` resolve releases from GitHub. Existing
  installs upgrade normally while the Codeberg repo still answers: its final
  commit carries this installer, which already points at GitHub. An install
  that misses that window just re-runs the one-liner from the README.

## [0.1.7] - 2026-07-19

### Added

- Staging directory is editable on the TUI plan screen: new "Staging" row
  (`e` to edit; empty input resets to the config/CLI default). The path rides
  `BurnParams` through the amend loop, so the staging free-space preflight
  re-checks the typed path as a plan-screen warning and hard-gates on Proceed.
  `~/…` now expands to `$HOME` in typed paths, the config `staging` key, and
  `--staging`. Tests: 4 new (edit commit/empty-reset, amended staging redirects
  the whole pipeline, tilde expansion in canonicalize and config).

- Every burn now reports the exact paths of every file it wrote: `wrote:`
  lines in line mode, and a scrollable (↑↓/j/k) "Files written" section on the
  TUI report screen. Covers parity files, checksums.sha256, MANIFEST.txt,
  RECOVERY.txt, the ISO (omitted when discarded after verification), the LBA
  map, the ISO sha256 sidecar, and the burn transcript; `burn-iso` reports its
  transcript too. Tests: 3 new/3 extended; 234 total pass, clippy/fmt clean.

## [0.1.6] - 2026-07-18

### Fixed

- TUI burn confirmation now guards against type-ahead: Enter typed before the
  confirm prompt renders (shell autorepeat at launch or double-tapped from the
  picker) no longer answers the prompt. Proceed is ignored until the prompt has
  been visible for ≥500 ms; Abort (q/Esc) is never delayed. First render of the
  prompt is tracked (later prompts in the amend loop are unaffected, arming
  happens once). Input queue is flushed at TUI start and picker start, and again
  when the first prompt renders. DESIGN.md TUI section documents the arming
  contract. Tests: 3 new (Enter ignored before/within arm delay; abort works
  unarmed); 226 total pass, clippy/fmt clean.

- Preflight failures before any plan exists (no disc, probe error) no longer
  switch to the Run screen, which would read as "it skipped the plan and went
  straight to burning". The failure now renders on the Plan screen's probing
  view as a wrapped error banner, with a "q quit" footer. Post-plan failures
  and pipeline-thread-disconnect still switch to Run as before. Tests: 1 new
  (preflight failure stays on Plan, shows banner); 227 total pass, clippy/fmt
  clean.

## [0.1.5] - 2026-07-18

### Added

- Payload picker now docks a "Selected" table under the browser once anything
  is selected: one row per payload showing full path (tail-truncated to fit),
  payload bytes, and share of the disc budget (`—` while the drive probe is
  pending). Table is capped at 8 rows with an "… and N more" overflow note.
  
- Picker text no longer clips on narrow windows: media/fit/status lines and
  the key-hint footer now word-wrap via `head_wrap` on constrained widths; the
  footer area grows to fit wrapped lines so the →/← open/up hint stays visible
  (that hint clipping made cross-directory selection undiscoverable). Entry
  rows and table paths remain single-line (required for cursor tracking).
  Tests: 5 new picker tests (table content, probing dash, tail_fit, wrap
  helper, narrow-window render); 223 total pass, clippy/fmt clean.

### Changed

- `tui::head_wrap` promoted to `pub(crate)` for reuse in the picker's
  line-wrapping logic; `kv()` demoted back to private (picker uses `wrapped()`
  wrapper instead).
- DESIGN.md Pick-screen paragraph expanded to document the Selected table
  behavior and narrow-window wrapping constraints.

## [0.1.4] - 2026-07-18

### Added

- Bare `ovenmitts` (no payload paths, no subcommand) now opens an interactive
  payload picker on a TTY before the plan wizard: directory browser starting
  at cwd with checkbox multi-select, fuzzy filtering within the current
  directory (`/` mode), hidden-files toggle (`.`), live disc-fit estimate
  in the header (background drive probe reusing runner's device-resolution
  policy via new `runner::detect_media`; falls back to an assumed blank BD-R
  25 like `plan`). Selection validated via `Payload::inspect` at toggle time;
  ancestor/descendant selections deduped. Enter with nothing selected picks
  the cursor entry. Cancel (q/Esc) prints "nothing selected" and exits 0.
  Non-TTY or `--no-tui` with no payloads keep the "nothing to do" error.
  Tests: picker unit tests incl. TestBackend render test; two non-TTY
  fallback tests in `tests/cli.rs`.

### Fixed

- Verify pipeline: reuse existing mountpoint when GNOME automounts the
  re-inserted disc during raw readback (pre-check + recheck on mount failure
  to handle automount race). Tests: new unit test for mount reuse path;
  non-mountable device path for udisksctl-absent test.

## [0.1.3] - 2026-07-17

### Added

- Payloads can be directories: `ovenmitts ~/extras ~/package.hc` burns the
  whole tree under `/extras` with per-file checksums and ONE par2 recovery
  set per top-level payload (member paths stored relative, so `par2 r -B.`
  works from a disc copy). Symlinks/special files stay on the ISO but are
  excluded from checksums and parity (warned); 0-byte files are checksummed
  but not parity-protected (par2 cannot repair them).

- Every burn tees the full xorriso transcript to `<label>.burn.log` next to
  the staged ISO, so a failed burn stays diagnosable after the run.
- A failed burn prints the transcript path and, in the full pipeline, the
  `ovenmitts burn-iso` retry hint (the staged ISO survives the failure).

### Changed

- `check` on a blank disc now refuses immediately with "medium is blank -
  nothing to check yet" instead of surfacing a cryptic xorriso error (or,
  on 0.1.0, polling 180 s for a readable sector that a blank disc never has).

### Fixed

- `--staging` is a global flag like `--device`: it now works with the
  default TUI invocation (`ovenmitts <payloads> --staging <dir>`) and every
  subcommand, not just `burn`.
- Burn, format and master failures now report xorriso's diagnostic lines
  (`FAILURE`/`FATAL`/`SORRY`/`aborting`) instead of the raw stderr tail,
  where "Thank you for being patient" keepalives could bury or evict the
  actual cause.

## [0.1.1] - 2026-07-17

### Added

- `ovenmitts update`: upgrade to the latest release by re-running the
  published installer (checksum-verified; `OVENMITTS_VERSION` pins).
- `ovenmitts info --save` and `ovenmitts check --save` write the
  auto-detected device to the config file (created if absent; comments,
  formatting and other keys survive).

### Changed

- Line-mode output now uses one severity vocabulary: `info:` for
  diagnostics, `warning:` for warnings, `error: [stage]` for stage failures
  (previously `[stage] FAILED:`), and a stage failure prints exactly one
  error line instead of two. The `info` subcommand's listing stays bare.
- A `device` set in the config file is now a soft preference: it is tried
  first, but auto-detection still scans `/dev/sr*` when it holds no readable
  medium. Only `--device` pins the drive hard.
- `check` and `verify` fail fast when no drive has a readable medium instead
  of polling up to 180 s for one.

### Fixed

- `check` and `verify` now auto-detect the drive like every other subcommand
  (previously they used the configured/default device verbatim, so a machine
  whose burner is not `/dev/sr0` always errored without `--device`).

- TUI report screen no longer lists the second-copy reminder twice (the
  runner's report already carries it; the TUI appended its own copy).

- TUI run screen: long warning/info lines in the event log now word-wrap
  (with the newest lines kept visible) instead of being clipped at the right
  edge; the failure banner wraps up to 5 rows.

## [0.1.0] - 2026-07-17

### Added

- Full archival pipeline: preflight → parity → checksums → master → burn →
  verify, driven by xorriso end-to-end (`-as mkisofs` / `-as cdrecord`);
  growisofs is never invoked.
- ISO 9660 level 3 + Rock Ridge + embedded MD5 session/file tags; multi-extent
  support for payloads beyond 4 GiB.
- par2 parity with computed slice size targeting the 32768-block PAR2 ceiling
  (never the default 2000 blocks); one recovery set per payload.
- Cache-proof two-stage verification: tray eject/reload with readiness poll,
  exact-size O_DIRECT read-back hashed against the staged ISO (buffered
  fallback), then read-only mount and per-file SHA-256 verification.
- Self-documenting discs: `RECOVERY.txt` (ddrescue → par2 repair walkthrough),
  `MANIFEST.txt` (burn parameters, sizes, hashes, date), `checksums.sha256`,
  and on-disc parity; staging keeps off-disc copies plus the file→LBA map.
  Both text files credit only the standard tools a future reader needs
  (xorriso, par2, ddrescue).
- Optional drive-level defect management (`--defect-management`):
  `-format as_needed` with capacity re-read from `-list_formats` before
  burning; stream recording at full speed and capacity remains the default.
- Capacity preflight with configurable headroom (default 5%) and exact
  unformatted capacities for BD-R 25/50, BDXL 100/128, DVD±R; staging
  free-space shortfalls surface as warnings before the confirm prompt.
- ratatui TUI (plan → confirm → live stage progress → report) when run bare on
  a TTY; identical plain-line output with `--no-tui` or when piped.
- Interactive plan screen: label, burn speed, redundancy, parity on/off, and
  defect management are editable in the TUI (↑↓ select, ←→ adjust/cycle,
  Space toggle, `e` inline edit); every change re-plans live through the
  runner, and a non-fitting plan keeps the screen open with confirmation
  disabled until it fits.
- Drive auto-detection: when no device is configured and the default
  `/dev/sr0` cannot be probed, `burn`, `burn-iso`, `info`, and `plan` scan
  `/dev/sr*` — exactly one drive with media is selected (announced in the
  output); several drives with media refuse with a list.
- Subcommands: `burn`, `burn-iso` (bit-identical second copy), `verify`,
  `check` (source-free periodic health check via embedded MD5 tags), `info`,
  `plan` (capacity math without a disc).
- VeraCrypt hygiene: refuses mounted containers at preflight, warns on
  non-sector-aligned container sizes, reminds about external header backups
  and fresh containers per archive generation.
- TOML config at `~/.config/ovenmitts/config.toml` with CLI-flag override.
- `eject_when_done` config key: force (`true`) or suppress (`false`) the disc
  eject after a fully verified archive; unset, the disc ejects only in the
  TUI — an operator is present — while unattended line-mode runs leave it
  loaded so the tray isn't left open.

[Unreleased]: https://github.com/greenseeing/ovenmitts/compare/v0.1.8...HEAD
[0.1.8]: https://github.com/greenseeing/ovenmitts/releases/tag/v0.1.8
[0.1.7]: https://github.com/greenseeing/ovenmitts/releases/tag/v0.1.7
[0.1.6]: https://github.com/greenseeing/ovenmitts/releases/tag/v0.1.6
[0.1.5]: https://github.com/greenseeing/ovenmitts/releases/tag/v0.1.5
[0.1.4]: https://github.com/greenseeing/ovenmitts/releases/tag/v0.1.4
[0.1.3]: https://github.com/greenseeing/ovenmitts/releases/tag/v0.1.3
[0.1.1]: https://github.com/greenseeing/ovenmitts/releases/tag/v0.1.1
[0.1.0]: https://github.com/greenseeing/ovenmitts/releases/tag/v0.1.0
