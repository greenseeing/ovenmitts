# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `check` on a blank disc now refuses immediately with "medium is blank -
  nothing to check yet" instead of surfacing a cryptic xorriso error (or,
  on 0.1.0, polling 180 s for a readable sector that a blank disc never has).

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

[0.1.1]: https://codeberg.org/greenseer/ovenmitts/releases/tag/v0.1.1
[0.1.0]: https://codeberg.org/greenseer/ovenmitts/releases/tag/v0.1.0
