# ovenmitts

Archival burns of large files and whole directories (VeraCrypt containers,
project trees) to BD-R / M-DISC on Linux. One binary drives the whole
pipeline — parity, checksums, mastering, burning, cache-proof verification —
and every disc it writes documents its own recovery.

## Why

Nothing maintained does this job. Brasero cannot master files past 4 GiB−1 (its
mkisofs backend stays at ISO interchange level 1/2) and is effectively
unmaintained; growisofs has had no upstream release since 2008, carries unfixed
BD-R defects, and POW-formats blank BD-R by default — rendering the disc
unusable to other tools; cdw, the last ncurses burn frontend, closed in 2023.
ovenmitts uses xorriso end-to-end (ISO 9660 level 3 multi-extent removes the
4 GiB limit) and adds the two things a 20-year disc actually needs: par2 parity
with a correctly computed block count, and verification that provably reads the
disc, not the page cache. Every design decision is backed by the verified
research in [`mdisc-archival-claims_2026-07-16/`](mdisc-archival-claims_2026-07-16/findings_mdisc-archival-claims.md).

## Install

```sh
curl -fsSL https://codeberg.org/greenseer/ovenmitts/raw/branch/main/install.sh | bash
```

The installer detects `apt`/`dnf`/`zypper`/`pacman`, installs the burn
backends (xorriso required; par2, udisks2, eject recommended), and drops a
single `ovenmitts` binary in place (verifying its published SHA-256 checksum
first). **To upgrade later, run `ovenmitts update`** — it re-runs this
installer, which does nothing when the installed version already matches the
latest release and the backends are present. To pin a version, set
`OVENMITTS_VERSION=0.1.3` before either command.

Or build from source:

```sh
git clone https://codeberg.org/greenseer/ovenmitts
cd ovenmitts
cargo build --release
# binary at target/release/ovenmitts
```

Burning as non-root needs membership in the `cdrom` group (log out and back in
after):

```sh
sudo usermod -aG cdrom $USER
```

### Runtime dependencies

| Tool | Debian package | Role |
|------|----------------|------|
| `xorriso` | `xorriso` | **required** — masters the ISO and burns the disc |
| `par2` | `par2` (or [par2cmdline-turbo](https://github.com/animetosho/par2cmdline-turbo), which also installs as `par2`) | recommended — parity; without it only `--no-parity` burns work |
| `udisksctl` | `udisks2` | recommended — rootless read-only mount for file verification |
| `eject` | `eject` | recommended — tray eject/reload between burn and verify |
| `dvd+rw-mediainfo` | `dvd+rw-tools` | optional — manufacturer media ID in `info` output |
| `veracrypt` | [veracrypt.fr](https://veracrypt.fr) | optional — lets preflight refuse a currently-mounted container |

## Quickstart

```sh
# TUI: interactive plan (edit label/speed/redundancy/parity live) → Enter
# burns → live pipeline → report
ovenmitts ~/archive/vault.hc

# No paths at all: pick payloads interactively first — browse from the
# current directory, Space selects, / fuzzy-filters, live fit estimate
# against the inserted disc, Enter hands the selection to the same plan TUI
ovenmitts

# Directories burn too — the whole tree lands under /extras on the disc
ovenmitts ~/extras ~/archive/vault.hc

# Same pipeline, plain lines (also automatic when stdout is not a TTY)
ovenmitts --no-tui ~/archive/vault.hc

# Scripted burn: 20% parity, custom label, no confirmation prompt
ovenmitts burn ~/archive/vault.hc --label VAULT_2026Q3 --redundancy 20 -y

# Capacity math against the inserted disc, burn nothing
ovenmitts burn ~/archive/vault.hc --dry-run

# Drive-level defect management (formats spare areas first; see Notes)
ovenmitts burn ~/archive/vault.hc --defect-management --speed 4

# Bit-identical second copy from the staged ISO
ovenmitts burn-iso ~/.local/share/ovenmitts/staging/VAULT_2026Q3/VAULT_2026Q3.iso -y

# Verify a disc byte-for-byte against its staged ISO, then every file on it
ovenmitts verify --iso ~/.local/share/ovenmitts/staging/VAULT_2026Q3/VAULT_2026Q3.iso

# Verify without the ISO: mounted file checksums + embedded MD5 tags
ovenmitts verify

# Periodic health check — needs nothing but the disc (embedded MD5 session tags)
ovenmitts check

# What is in the drive: type, capacity, formatted state, speeds, media ID
ovenmitts info

# Same, and remember the detected drive in the config file
ovenmitts info --save

# Will it fit? No disc needed
ovenmitts plan ~/archive/vault.hc --media bd25

# Upgrade ovenmitts to the latest release
ovenmitts update
```

Global flags: `--device <dev>`, `--config <path>`, `--staging <dir>`,
`--no-tui`. A `--device`
from the CLI is always used as-is; the config-file `device` (or the `/dev/sr0`
default) is a soft preference — it is tried first and, if it cannot be probed
(no disc — or no such drive), every subcommand scans the other `/dev/sr*` drives —
exactly one with a disc is auto-selected (announced in the output); more than
one aborts with the list so you can pick with `--device`. A mounted disc in a
drive other than the configured one is invisible to the scan — pass
`--device` for that case.

`burn` flags: `--label` (A–Z 0–9 `_`, max 32 chars; default
`ARCHIVE_YYYYMMDD`), `--speed`, `--redundancy` (par2 percent),
`--defect-management`, `--no-parity`, `--dry-run`, `-y`/`--yes`,
`--discard-iso` (delete the staged ISO after a successful verify; default
keeps it for copy 2).

`burn-iso` flags: `--speed`, `-y`/`--yes`.
`check`/`info` flags: `--save` (write the auto-detected device to the config
file; comments and other keys survive).
`plan` flags: `--media` (`bd25` | `bd50` | `bd100` | `bd128` | `dvdr`; default
`bd25` when no disc can be probed), `--redundancy`.

## The plan screen

The TUI's plan screen is where a burn is shaped. Five parameters are editable
in place; every change round-trips through the pipeline runner, which redoes
the capacity math and sends back the plan it now holds — the parity estimate,
totals, fit gauge, and warnings on screen are always the runner's own numbers,
never a UI approximation. What you confirm is exactly what burns.

| Key | Action |
|-----|--------|
| `↑` `↓` (or `k` `j`) | select a parameter row |
| `←` `→` (or `h` `l`) | step redundancy ±1 · cycle speed through the drive's probed speeds · toggle parity / defect management |
| `Space` | toggle parity / defect management |
| `e` | edit label or speed inline (type, `Enter` commits, `Esc` cancels) |
| `Enter` | burn — disabled while the plan does not fit |
| `q` / `Esc` | abort (while editing, `Esc` only cancels the edit) |

Values are validated by the runner: labels are uppercased to `A–Z 0–9 _`
(max 32), redundancy clamps to 1–100, and a speed the drive didn't report is
allowed with a warning (drives negotiate). If the payload doesn't fit, the
screen shows **DOES NOT FIT** and stays editable — lowering redundancy or
disabling parity often fixes it; `Enter` unlocks the moment it fits. Edits
are per-run: nothing is written back to the config file.

One operator note: after the burn stage the tray ejects on purpose (see the
pipeline table). **Push it back in** — verification resumes automatically once
the drive reports ready. After the final verify passes, the TUI ejects the
disc again so you can label and store it; unattended line-mode runs leave it
loaded instead (no tray hanging open for hours). The `eject_when_done` config
key forces either behavior.

## Configuration

`~/.config/ovenmitts/config.toml` (or `$XDG_CONFIG_HOME/ovenmitts/config.toml`;
`ovenmitts info --save` writes the auto-detected device here for you —
override with `--config`). Every key is optional, unknown keys are rejected,
CLI flags win over the file:

```toml
# device = "/dev/sr1"      # preferred burner, tried first; auto-detection
                           # still scans /dev/sr* when it has no disc
                           # (only --device pins the drive hard)
staging = "/home/you/.local/share/ovenmitts/staging"
                           # parity + ISO workspace
                           # (default $XDG_DATA_HOME/ovenmitts/staging)
speed = 4                  # burn speed factor; unset = drive decides (default)
redundancy_pct = 15        # par2 redundancy percent (default 15)
headroom_pct = 5           # never fill the disc past 100−headroom % (default 5)
defect_management = false  # format spare areas before burning (default false)
keep_iso = true            # keep the staged ISO after a verified burn (default true)
# eject_when_done = true   # force (true) or suppress (false) the eject after a
                           # verified archive; unset = eject only in the TUI,
                           # where someone is present to take the disc
```

## The pipeline

| stage | what happens |
|-------|--------------|
| preflight | inspects payloads — a directory expands to its member files, warning about symlinks/special files and 0-byte files — refuses mounted VeraCrypt containers, selects the drive and probes the disc (`xorriso -toc -list_formats -list_speeds`), fit-checks with headroom, checks staging space; in the TUI the plan then stays open for editing until confirmed |
| parity | one recovery set per top-level payload: `par2 create -B<parent> -r<pct> -n1 -s<slice> -m<mem>` with every member file as a relative operand (a directory's files share one set; 0-byte files excluded — par2 cannot repair them); slice size computed toward the PAR2 32768-block ceiling (~2 MiB slices on 93 GiB) — never the default 2000 blocks that defeat 15% parity |
| checksums | streaming SHA-256 of every payload file (directory members by their relative disc path) and parity file → `checksums.sha256` |
| master | writes `MANIFEST.txt` + `RECOVERY.txt`, then `xorriso -as mkisofs -iso-level 3 -rock --md5`, extracts the file→LBA map, hashes the ISO |
| format | only with `--defect-management`: `xorriso -format as_needed`, then re-reads the reduced capacity and re-checks fit |
| burn | `xorriso -as cdrecord -v dev=<dev> [speed=<n>] fs=64m blank=as_needed -eject <iso>` — stream recording on unformatted BD-R; the full xorriso transcript tees to `<label>.burn.log` next to the staged ISO, and a failure reports xorriso's diagnostic lines plus the `burn-iso` retry hint (the staged ISO survives) |
| verify image | reloads the tray, polls drive readiness, reads exactly ISO-size bytes from the device (O_DIRECT, buffered fallback) and compares SHA-256 to the staged ISO |
| verify files | mounts the disc read-only via udisksctl and re-hashes every file against `checksums.sha256` |

The burn ejects the disc; the eject/reload cycle before read-back is what makes
verification cache-proof — the kernel cannot serve stale pages for a medium it
watched leave the drive. Close the tray when it opens; the readiness poll picks
the disc up and verification continues unattended.

## Disc layout

```
/vault.hc                        payload files at the root
/extras/…                        a directory payload keeps its whole tree
/parity/vault.hc.par2            par2 recovery set, one per payload
/parity/vault.hc.vol000+91.par2
/parity/extras.par2              a directory's files share one set
/checksums.sha256                SHA-256 of every payload and parity file
/MANIFEST.txt                    parameters, sizes, hashes, date
/RECOVERY.txt                    exact restore and repair commands
```

The staging directory keeps off-disc copies of everything that repairs a
damaged disc: the ISO (with a `.sha256` sidecar), the parity set,
`checksums.sha256`, `<label>.lba.txt` mapping every file to its disc
sectors, and `<label>.burn.log` (the full xorriso burn transcript). Keep it.

## Recovery

Every disc carries a `RECOVERY.txt` with these exact steps. A healthy disc
needs none of this: mount read-only, copy, then in the directory holding the
copies `sha256sum -c --ignore-missing /mnt/checksums.sha256`.
If a disc develops read errors:

```sh
# 1. Image everything readable (GNU ddrescue). Unreadable sectors stay
#    zero-filled and the image keeps its full length — exactly what par2 expects.
ddrescue /dev/sr0 recovered.iso rescue.map
ddrescue -r3 /dev/sr0 recovered.iso rescue.map   # extra retry passes

# 2. Extract the damaged payload (a directory payload extracts as a tree)
mount -o loop,ro recovered.iso /mnt
# or without root:
xorriso -osirrox on -indev recovered.iso -extract /vault.hc vault.hc

# 3. Repair it with the parity on the same disc (or your off-disc copy);
#    -B. bases the repair on the damaged copy here, not the read-only mount
par2 r -B. /mnt/parity/vault.hc.par2 vault.hc
# directory payload: copy the tree here first, then repair the whole set
cp -r /mnt/extras . && par2 r -B. /mnt/parity/extras.par2

# 4. Map damaged sectors to files, if you want to know what was hit
xorriso -indev recovered.iso -find / -exec report_lba --
```

## Notes

**Directory payloads.** A directory keeps its name at the disc root and its
whole tree below it. Every member file is checksummed by its relative disc
path, and one par2 set protects the payload — the set stores those relative
paths, so `par2 r -B.` repairs a copied tree exactly as RECOVERY.txt shows.
Symlinks and special files stay on the ISO (Rock Ridge stores them as-is) but
are excluded from checksums and parity, with a warning — link *targets* are
not archived unless they are also in the tree. 0-byte files are checksummed
but cannot be parity-protected (a par2 limitation). Hard limits, refused at
preflight: empty directory payloads, non-UTF-8 file names, payload names
starting with `-`, and more than 32768 files in one payload — `tar` the
directory first for those. Top-level payload names must be unique (the disc
root is flat).

**Defect management is off by default.** Unformatted BD-R streams at full speed
and full capacity; the pipeline's read-back verify + par2 already covers write
errors, so drive-level spare areas mostly cost you: formatting was observed to
eat 768 MiB of a 25 GB disc (not the man page's 256 MiB) and halves write
speed. `--defect-management` turns it on explicitly — ovenmitts then formats
first and re-reads the real remaining capacity before burning.

**Prefer 25 GB single-layer over BDXL.** SL discs burn and age more reliably
than DL/TL/QL and read in every BD drive tier — which matters as the drive
market shrinks. ovenmitts warns when it sees multi-layer media. Two 25 GB
discs usually beat one 50 GB disc.

**Fresh VeraCrypt container per generation.** Never re-burn diverged copies of
the same container: they share a master key (VeraCrypt's Volume Clones
warning), and write-once media makes that permanent. Create a new container per
archive generation and keep an external volume-header backup (Tools > Backup
Volume Header) — the header is the single point of total failure. Container
detection is heuristic, since a container is indistinguishable from random
bytes by design: payloads named `*.hc`/`*.tc` (VeraCrypt/TrueCrypt convention)
or extensionless files ≥ 64 MiB get the container warnings and hygiene checks;
anything else is treated as a plain file.

**Burn two copies.** The staged ISO is kept after a verified burn (drop it with
`--discard-iso`): insert a fresh disc and `ovenmitts burn-iso <iso>` writes a
bit-identical second copy, verified the same way. Store the copies apart and
run `ovenmitts check` on each disc periodically — it needs no source data, only
the MD5 tags embedded at mastering time.

## License

MIT — see [LICENSE](LICENSE).
