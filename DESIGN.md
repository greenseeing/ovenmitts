# ovenmitts — design

Archival burns of large files and whole directories (VeraCrypt containers,
project trees) to BD-R / M-DISC on Linux.
Single static binary. Pipeline: preflight → parity → checksums → master → burn → verify.

Every design decision below is backed by the verified research in
`mdisc-archival-claims_2026-07-16/findings_mdisc-archival-claims.md`. Do not
re-litigate them in code.

## Non-negotiable decisions (from research)

1. **xorriso end-to-end.** Master with `-as mkisofs`, burn with `-as cdrecord`.
   growisofs is never invoked (unmaintained since 2008; its POW-formatting
   default renders media unusable to xorriso).
2. **ISO 9660 level 3 + Rock Ridge + `--md5`.** Multi-extent handles files > 4 GiB;
   `--md5` embeds session/file checksums so `xorriso -check_media` can verify the
   disc decades later without the source ISO.
3. **Defect management OFF by default** (stream recording, full speed, full
   capacity). `--defect-management` formats first (`-format as_needed`) and then
   capacity MUST be re-read from `-list_formats` (observed spare loss: 768 MiB,
   not the man-page 256 MiB).
4. **par2 block count is computed, never defaulted.** Default 2000 blocks →
   ~46 MiB slices on 93 GiB → ~300 scattered sector errors defeat 15% parity.
   We target ~2 MiB slices, clamped to the PAR2 ceiling of 32768 blocks.
5. **Verification is two-stage and cache-proof.** Eject/reload + readiness poll,
   then read exactly `iso_size` bytes from the device (O_DIRECT when possible,
   2048-aligned) and compare SHA-256 against the staged ISO; then mount ro and
   verify every file against `checksums.sha256` (hashed natively, streaming).
6. **Never fill to the rim.** Default headroom 5% (failures concentrate at the
   outer region / last layer). Parity + checksums + LBA manifest are kept
   off-disc in staging too.
7. **Capacity preflight** uses unformatted "Media summary" free blocks for
   stream recording, formatted capacity from `-list_formats` when DM is on.
8. **VeraCrypt hygiene**: refuse to archive a container that is currently
   mounted (veracrypt --text --list); when payload looks like a container
   (.hc/.tc or extensionless big file), print reminders: external header
   backup, fresh container per generation (Volume Clones warning).
9. **Self-documenting discs** (darbrrb lesson): every disc carries RECOVERY.txt
   with exact restore/repair commands, and MANIFEST.txt with parameters,
   hashes, date. On-disc files never name ovenmitts — they credit only the
   standard tools (xorriso, par2, ddrescue) a future reader needs.

## Exact external commands

```
probe:    xorriso -outdev <dev> -toc -list_formats -list_speeds
format:   xorriso -outdev <dev> -format as_needed        (only with --defect-management)
master:   xorriso -as mkisofs -iso-level 3 -rock --md5 -V <LABEL> -o <iso> -graft-points <grafts>...
lba map:  xorriso -indev <iso> -find / -exec report_lba --
burn:     xorriso -as cdrecord -v dev=<dev> speed=<n> fs=64m blank=as_needed -eject <iso>
check:    xorriso -md5 on -indev <dev> -check_media --
          (-md5 defaults to off; without it check runs skip the session tags)
media id: dvd+rw-mediainfo <dev>                         (optional, if installed)
mount:    udisksctl mount -b <dev>   /  udisksctl unmount -b <dev>
parity:   par2 create -B<root_parent> -r<pct> -n1 -s<slice_bytes> -m<mem_mb> <out.par2> <rel_operands>...
          (one set per top-level payload; a directory's member files are the
           operands, relative to the payload root's parent, so the set stores
           disc-layout paths and `par2 r -B.` repairs a disc copy; cwd = that
           parent and -B pins the basepath there because par2 otherwise bases
           on the staged .par2's dir and skips the sources; 0-byte members are
           excluded - par2 cannot repair them; no -q - par2 emits its percent
           stream only at default verbosity; prefer `par2cmdline-turbo` binary
           name `par2` or `par2turbo` if found)
```

## Payload model

A top-level payload is a regular file or a directory; the disc root is flat,
so top-level names must be unique. A directory keeps its name at the root
(`/extras/**`) and expands at preflight into member files (deterministic
sorted walk, regular files only): each member is checksummed by its relative
disc path and the payload gets ONE par2 set covering all members. Symlinks
and special files stay on the ISO via Rock Ridge but are excluded from
checksums and parity (warned) — the walk never follows a symlink. 0-byte
members are checksummed and grafted but excluded from parity operands.
Refused at preflight: empty directory payloads, non-UTF-8 file names, names
starting with `-` (par2 would parse a flag), more than 32768 non-empty
members, or member paths past the argv budget (tell the user to tar the
directory). Slice sizing budgets one block of tail waste per member file so
a multi-file set stays under the 32768-block ceiling.

Implementers: verify flag syntax against the online man pages (mankier.com/1/xorriso,
/1/xorrisofs, /1/xorrecord, manpages.debian.org par2) before finalizing; the
exact progress-line formats must be handled from real samples documented there.

## Capacities (exact, unformatted)

| media | bytes |
|---|---|
| BD-R 25 SL | 25_025_314_816 |
| BD-R 50 DL | 50_050_629_632 |
| BDXL 100 TL | 100_103_356_416 |
| BD-R XL 128 QL | 128_001_769_472 |
| DVD-R 4.7 | 4_707_319_808 |
| DVD+R 4.7 | 4_700_372_992 |

## Module map (flat, house style: zipline/pwshark)

```
src/
  main.rs      entry; dispatch CLI subcommand or TUI
  lib.rs       pub mod declarations
  cli.rs       clap v4 derive definitions (done in scaffold)
  config.rs    TOML config ~/.config/ovenmitts/config.toml, CLI-override merge (done)
  plan.rs      core domain types + pure planning math (done in scaffold)
  tools.rs     external binary discovery + version probe (done)
  media.rs     parse xorriso -toc/-list_formats/-list_speeds + dvd+rw-mediainfo → MediaInfo
  hashing.rs   streaming SHA-256; write/parse/verify checksums.sha256
  parity.rs    par2 slice-size computation + create/verify invocation
  master.rs    ISO build, MANIFEST.txt, RECOVERY.txt, LBA map extraction
  burn.rs      burn + optional DM format; parse xorriso progress → BurnProgress events
  verify.rs    readiness poll, O_DIRECT exact-size read-back hash, mount + file verify, check_media
  runner.rs    pipeline orchestration; emits StageEvent over mpsc; used by CLI and TUI
  tui.rs       ratatui 0.30 live pipeline view (interactive plan → stage progress → report)
  picker.rs    interactive payload picker for bare `ovenmitts` (browse, checkbox, fuzzy filter)
```

Signatures in the scaffold are the contract. Fill bodies; do not change public
signatures unless compilation genuinely requires it — if so, note it in the
final report. `todo!()` bodies mark what to implement.

## Events

`runner.rs` drives stages on a worker thread and emits `StageEvent` on
`std::sync::mpsc::Sender`. CLI mode prints events as lines; TUI renders them.
Subprocess stdout/stderr are pumped by reader threads; progress lines are
parsed in burn.rs/master.rs and forwarded as `StageEvent::Progress`.
`Out` carries a command's primary output (the `info` listing) and renders
bare; `Info`/`Warn`/`Failed` are diagnostics and carry severity.

Confirmation is a loop, not a gate: `NeedAck` grants the UI exactly one reply —
`Proceed`, `Abort`, or `Amend(BurnParams)`. On `Amend` the runner re-plans
(pure math; media stays probed once), re-emits `Plan` carrying the canonical
params it now holds, and asks again. Line mode never sends `Amend`
(`BurnRequest.amend = false` keeps the pre-prompt fit bail). The invariant on
both sides: at most one ack per `NeedAck`, and everything the pipeline consumes
(parity `-r`, label, speed, format gate) comes from the loop's final params.

## Error handling / style

- anyhow::Result everywhere; `bail!`/`context()`; exit codes via main.
- No logging framework (house style): user-facing lines via events, that's it.
- No comments unless the WHY is non-obvious. No multi-line docstrings.
- Tests: unit tests inline `#[cfg(test)]`; integration tests in `tests/` with
  fake `xorriso`/`par2` shell scripts on PATH (fixtures under `tests/fakebin/`).
  Fixture texts for parser tests live in `tests/fixtures/`.
- Never require root; burning needs group `cdrom` membership (document it).
  O_DIRECT read-back falls back to buffered reads (with a warned event) when
  O_DIRECT fails (e.g. tmpfs in tests).

## TUI

`ovenmitts <paths>...` with no subcommand on a TTY → TUI; bare `ovenmitts`
(no paths) first opens the payload picker. Screens:
0. Pick (bare invocation only): directory browser starting at cwd, checkbox
   multi-select of files/dirs, fuzzy filter within the current directory.
   Keys: ↑↓/jk move, Space toggle, →/←(hl, Backspace) descend/ascend, `/`
   filter mode (Enter keeps, Esc clears — never cancels), `.` hidden toggle,
   Enter confirm (empty selection picks the cursor entry), q/Esc cancel →
   "nothing selected", exit 0. Toggling runs Payload::inspect, so invalid
   payloads refuse with the preflight message right in the status line;
   selecting a dir absorbs selected descendants, selecting under a selected
   ancestor refuses (the disc root stays flat; the runner still dedupes).
   The drive is probed in the background (same auto-select policy as the
   runner); the header shows a live fit estimate from build_plan — no disc
   falls back to an assumed blank BD-R 25, like `plan`. A Selected table
   (full path tail-truncated · payload bytes · % of the disc budget, `—`
   while probing) docks under the browser once anything is selected, capped
   at 8 rows + "… and N more". Media/fit/status lines and the key hints
   word-wrap via head_wrap on narrow windows; only entry rows and table
   paths truncate (they must stay one row for cursor math).
1. Plan: media probe result, payload table, editable parameters (label, speed,
   redundancy, parity, defect management), fit bar, warnings. Keys: ↑↓/jk
   select row, ←→/hl adjust or cycle, Space toggle, `e` inline edit for
   label/speed (Enter commits, Esc cancels), Enter burn (disabled while the
   plan does not fit), q/Esc abort. Every edit goes through the runner's amend
   loop; the screen only ever shows runner-computed plans. Confirm arming:
   type-ahead is flushed at TUI start and when the first prompt renders, and
   Proceed is ignored until that prompt has been on screen ≥500 ms — a burn
   confirm must be a deliberate keypress on a prompt the user has seen
   (double-tapped/held Enter from the picker or shell must never answer it).
   Abort (q/Esc) is never delayed.
2. Run: per-stage progress gauges (parity/checksums/master/burn/verify1/verify2),
   scrolling event log, elapsed time. Parameters are locked once the run starts.
3. Report: per-stage result, hashes, next-step reminders (second copy,
   off-disc parity, header backup). Key: q quit.

Non-TTY or `--no-tui` → plain line output (same events).

## Drive selection

A `--device` from the CLI is always used as-is. The built-in default
`/dev/sr0` and a config-file `device` are soft preferences that may be
swapped: if the preferred device cannot be probed
(no disc, or no such drive), every drive-touching subcommand
(`burn`/`burn-iso`/`verify`/`check`/`info`/`plan`) probes every `/dev/sr*` —
one drive with media is auto-selected (announced in the event log), more
than one refuses with the list (`plan` propagates that refusal instead of
falling back to synthetic media). Probing needs exclusive access, so
`verify`/`check` unmount the configured device before resolving; a mounted
disc sitting in a *different* drive is invisible to the scan — pass
`--device` for that case. `check`/`verify` fail fast when no drive has a
readable medium instead of polling for one.
