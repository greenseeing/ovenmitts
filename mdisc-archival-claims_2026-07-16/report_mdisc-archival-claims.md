## Executive Summary

The initial research's architecture — ISO 9660 level 3 mastering, a par2 parity layer, and rigorous two-stage verification — survives adversarial verification. Its tool choices do not, entirely: growisofs (unmaintained since 2008, with BD-R-specific defects and a POW-formatting default that renders media unusable to other tools) must be replaced by xorriso end-to-end; the "leave defect management on" advice flips to off-by-default (contested between toolchain authors, redundant given read-back verification, and costly: ~768 MiB observed capacity loss); and the quoted par2 command silently under-provisions block count — the single parameter that decides whether 15% parity actually recovers a damaged disc. Adversarial passes added factors the primary research missed: parity data lands on the disc's failure-prone outer rim, VeraCrypt's own documentation warns against re-burning diverged copies of one container, and the dominant long-term risk is the 2024–2026 collapse of the optical drive/media manufacturing base, which no software choice mitigates.

## Research Scope

Verify the claims in `ref/initial-research.md` (a prior AI-generated research conversation) about archiving VeraCrypt containers to M-DISC BD-R on Linux, specifically: ISO 9660 level 3 multi-extent behavior, xorriso/growisofs/par2 pipeline correctness, the Brasero file-size limit, BD-R spare areas / defect management, UDF vs ISO trade-offs, and capacity figures for 25 GB BD-R and 100 GB BDXL. Six parallel researchers gathered from primary sources (spec texts, kernel source, man pages, maintainer mailing-list posts, bug trackers); two adversarial researchers then hunted contradictions and delivered per-claim verdicts. The findings feed directly into the design of a burn tool built in this repository.

## Detailed Findings

### 1. Filesystem layer: ISO 9660 level 3 is correct and verified to the source

The mechanics hold exactly as claimed. ISO/IEC 9660 stores extent length in a 32-bit field (largest 2048-aligned extent: 4,294,965,248 bytes = 0xFFFFF800) [1]. Interchange level 3 chains extents using directory-record flag bit 7 ("not the final directory record"), with no spec-level cap on extents per file [1]. The Linux kernel's `isofs_read_level3_size()` walks the chain and presents one contiguous inode — verified directly in `fs/isofs/inode.c`, which states "file size is only limited by the maximum size of a file system, which is 8 TB" [2]. The historical unaligned-tail read bug was fixed 2009-09-27 and a >4 GiB unaligned file verified correct even on RHEL5's 2.6.18 kernel [18].

Two corrections to the original document:

- **xorriso's default per-file cap is 400 GiB − 200 KiB** (100 extents × ~4 GiB), an implementation default raisable via `-file_size_limit` — not the spec's ~8 TiB [3]. Irrelevant below 100 GB, but the "effectively ~8 TiB" table entry was wrong for the actual tool.
- **The cross-OS claim was overstated.** Linux and Windows read multi-extent correctly (empirically tested); the macOS claim is contradicted by the one documented test (OS X 10.4.8's cd9660 driver has no level 3 support) with no fix on record, and the "pre-2007" cutoff is unsourced [17]. For a Linux-only recovery scenario this changes nothing. Sizing the container to a whole number of MiB keeps every extent 2048-aligned — free hardening against even the ancient kernel bug.

Rock Ridge remains correct for archival ("strongly discouraged to disable"), and `--md5` embeds per-file and session checksums that enable source-free verification decades later via `-check_media` [3][4][16].

### 2. Burn layer: replace growisofs with xorriso — the report's biggest correction

growisofs/dvd+rw-tools last released upstream in March 2008 [9]. Documented, still-unfixed-upstream defects: DAO burns of non-32KB-aligned images fail on kernels ≥3.x (Debian #794868, patched only downstream) [10]; a spurious error after successful BD-R burns (Arch FS#47797, patch carried by distros for 7+ years, never merged) [14]; and a medium-overflow check computed against *unformatted* capacity, so images sized between formatted and unformatted capacity fail at end of burn (LP #1424215) [12]. The CLOSE SESSION bug is fixed in Debian-family packages since 2015 — "growisofs is buggy" partly reflects upstream, not shipped packages [11] — but the maintenance asymmetry is decisive: xorriso released 1.5.8 in April 2026 with active follow-ups [6], its author burns his own backups with it and recommends it (or cdrskin) for BD-R [15], and a real-world 93.2 GiB BDXL triple-layer burn via `xorriso -as cdrecord` is on record [19]. BDXL needs nothing special — TL/QL media report the same profile 0x41 as BD-R [20].

**The tool-mixing hazard is the sharpest finding:** growisofs POW-formats blank BD-R by default; xorriso then classifies such media "unsuitable: is POW formatted" and refuses it [4][7]. A pipeline must pick one burn stack and never mix. M-DISC-specific caveat: the media presents to software as ordinary BD-R (its author: burn reports "rarely tested") — low-risk but worth a first-disc end-to-end test on the user's own drive [16].

Burn speed: no primary source supports "4x recommended for M-DISC"; drives override requested speeds anyway (the documented BDXL burn ran at ~2x on 4x-rated media) [5][19]. The tool should pass a requested speed through and report the actual rate, nothing more.

### 3. Defect management: contested, not settled — default OFF

The original document said to leave BD-R spare-area formatting on. Both adversarial passes weakened this to a per-user choice, with the better default being off:

| | Defect management ON | Stream recording (unformatted) |
|---|---|---|
| Write-time behavior | Drive verifies each block, remaps bad ones | Blind write at full nominal speed |
| Speed | ~½ nominal [7] | Full nominal [5] |
| Capacity | −256 MiB minimum; −768 MiB observed [12] | Full 25,025,314,816 B |
| Failure mode | Can fail entire burn on marginal media; silently masks marginal media quality [22] | Loud failure at verify time — reject the disc |
| Advocate | growisofs docs ("spare:none not recommended") [7][9] | libburn/xorriso default; author formats nothing [5][24] |

Given this pipeline already does full read-back hashing plus par2 parity, drive-level write verification is largely redundant — and a marginal disc is better rejected loudly at verify time than silently patched by remapping. No evidence was found that DM-formatted discs read worse in other drives (the compatibility fear is unproven both ways), and dd+hash verification demonstrably works on DM-formatted media [13]. The tool exposes formatting as an explicit option and, when used, sizes payloads from `-list_formats` output rather than any constant [4][22].

### 4. Capacity figures: confirmed, with the formatted/unformatted trap

Exact unformatted capacities [25]:

| Media | Bytes | ≈ GiB |
|---|---|---|
| BD-R 25 (SL) | 25,025,314,816 | 23.31 |
| BD-R 50 (DL) | 50,050,629,632 | 46.61 |
| BDXL 100 (TL) | 100,103,356,416 | 93.23 |
| BD-R XL 128 (QL) | 128,001,769,472 | 119.21 |

A blank disc's "Free Blocks" reports *unformatted* capacity; formatting with spare areas shrinks it (observed: 24,220,008,448 B after growisofs's default format — a 768 MiB loss, 3× the man-page "minimal" figure) [12]. Any capacity preflight must know which mode it is in.

### 5. Parity layer: par2 is right, its defaults are wrong for this job

par2cmdline is actively maintained again (v1.0.0 April 2024 → v1.2.0 June 2026) [34]; par2cmdline-turbo is the SIMD-accelerated fork worth preferring at these sizes [35]. The syntax `-r15 -n1` is valid, and PAR2's design (zero-padded slices, per-slice CRC32+MD5 location) makes the ddrescue → zero-filled full-length image → `par2repair` path mechanically sound [33][36].

The trap: **default block count is 2000** → ~46 MiB slices on a 93 GiB container. 15% redundancy buys ~300 recovery slices, so ~300 *scattered* 2 KiB sector errors landing in distinct slices defeat the whole parity set despite <1 MiB of physical damage [36][37]. Block count must be pushed toward the format's 32768 ceiling (~2.9 MiB slices at 93 GiB), which costs only create-time CPU/RAM (set `-m` explicitly; the default is 16 MB). The tool computes this automatically — this misconfiguration is a bigger practical risk to recoverability than any burn-tool choice.

Placement matters too: files written last land at the disc's outer region/last layer, exactly where real BDXL failures were observed ("slow read on L1 ... failure to read near the beginning of L2") [49]. Mitigations: keep parity and checksums off-disc as well, don't fill discs to the rim, and record a file→LBA manifest (`xorriso -find / -exec report_lba`) so a damaged disc can be salvaged by offset without a mountable filesystem [62]. For metadata-level protection below the filesystem, dvdisaster RS03 (maintained forks: speed47, jcea) is a complementary optional layer [58][59].

### 6. Verification protocol: confirmed, two hardening additions

Eject/reload before read-back is required — the kernel page cache otherwise serves the verify read [23]. Additions from the adversarial pass: poll drive readiness after `eject -t` (immediate reads fail "No medium found" on some drives) [61], and on clamshell drives that cannot auto-reload, `dd iflag=direct` (O_DIRECT, 2048-aligned) is the cache bypass [49]. Read exactly `ISO_size/2048` blocks — full-device reads mismatch on trailing run-out [65]. The mounted `sha256sum -c` second stage stands. `xorriso -check_media` against embedded MD5 tags adds source-free verification for periodic disc health checks — the aging-monitoring mechanism, already built into the burn tool [16]. growisofs, for contrast, has no verification at all [68].

### 7. VeraCrypt layer: confirmed, one missed warning that changes the workflow

Confirmed: containers mount fine from read-only ISO 9660 media (official FAQ; the GitHub #440 failure is specific to raw write-blocked block devices, not files on read-only filesystems) [38][43]; the master key exists only in the primary and embedded-backup headers, making them the single point of total failure — external header backups are mandatory [39][40]. XTS confines a ciphertext bit error without cascade; the practical planning unit is the 2048-byte disc sector (= four 512-byte XTS data units) [42].

Missed by the original research, and it changes the intended workflow: **VeraCrypt's Volume Clones page warns against exactly the "one container, re-filled and re-burned each time" pattern** the user planned. Diverged copies sharing one master key undermine cryptanalysis resistance and void plausible deniability — and write-once archival media makes every generation permanently un-deletable. Each archive generation should be a freshly created container (new salt/keys) [41]. Two corollaries: the staged ISO/par2 files are ciphertext, so staging-disk residue leaks little beyond existence/size; and the VeraCrypt FAQ's own advice to use UDF for burning rests on an outdated "ISO can't exceed 2 GB" rationale [38].

### 8. UDF verdict: right conclusion, two of four arguments corrected

ISO 9660 level 3 remains the right choice for Linux-read archival — xorriso cannot write UDF at all, genisoimage's UDF is alpha-status 1.02 with no POSIX permissions, and the xorriso author states UDF "offers no practical advantages over ISO 9660" for this purpose [4][28][30]. But two of the original document's supporting arguments don't survive: Linux kernel *read* support for UDF 2.50/2.60 has been in place since 2.6.26 (2008) — the genuine gap is userspace *creation* tooling — and mkudffs can target BD-R without a metadata partition (the write-once exception in its own man page) [29]. pktcdvd's removal in kernel 6.17 is real but irrelevant — packet writing was never needed for premastered burning [31].

### 9. Ecosystem claims: Brasero, K3b, M-DISC, market

- **Brasero**: the real limit is 4 GiB−1, not 2 GiB (mkisofs-family backend at interchange level 1/2, never passing `-iso-level 3` or `-allow-limited-size`); effectively unmaintained — GNOME's January 2025 maintainer check-in went unanswered [26][27].
- **K3b**: handles big files by auto-enabling *UDF extensions* per its own changelog — not by enabling level 3, which is only a manual custom option. The original claim conflated two mechanisms [32].
- **M-DISC**: burns in standard BD burners (BDA-certified; the M-READY drive requirement was DVD-era) [44][45]. The longevity advantage over regular inorganic HTL BD-R is genuinely disputed: standard HTL BD-R is already inorganic; Verbatim's 2022 reformulation reportedly switched M-DISC BD to the same MABL layer as its ordinary line, sharing media IDs; and the independent LNE aging study rated M-DISC DVD *worst* of the inorganic discs tested [44][46][47][48]. The "1,000 years" figure traces to vendor marketing around a DoD test never independently published; Verbatim's own fine print says "several hundred years" [50].
- **Market (the dominant risk)**: Pioneer exited optical drives (2025, business transferred), LG discontinued its burners, Sony quit recordable BD media (2025); Verbatim and I-O Data pledged continued supply [53][54][55]. 100 GB M-DISC SKUs remain purchasable (~$300+/25pk) [63]. Community consensus and failure reports favor 25 GB single-layer over BDXL for archival reliability [49][67]. A 20-year plan must include spare drives, media from multiple batches, and periodic re-verification with planned migration.
- **Tooling gap**: cdw, the last ncurses burn frontend, closed in 2023; darbrrb and bdarchiver (the closest existing archival tools) wrap growisofs and predate these findings. Nothing maintained occupies this niche — a new tool does not duplicate an established project [56][57][64].

## Peripheral Vision

Counter-arguments surfaced and how they were weighed:

- *VeraCrypt's FAQ recommends UDF* — outdated rationale (pre-level-3); acknowledged, does not overturn the tooling-maturity argument [38].
- *growisofs bugs are partly fixed downstream* — true for CLOSE SESSION (2015); does not change the maintenance asymmetry or the POW-format hazard [11].
- *Defect management has a real defense* — write-time verify, drive-portable defect list, and no evidence of read-compatibility harm; hence "option," not "never" [13].
- *No public test of a single ~90 GiB multi-extent file* — image-scale proven at 93.2 GiB, single-file extent count extrapolated from spec + kernel source; residual risk accepted, mitigated by first-disc end-to-end test [19].
- *One fetched source contained a prompt-injection attempt* (bentasker.co.uk, flagged and ignored by the researcher); its content was not load-bearing.
- Minor unresolved discrepancy: one researcher read par2cmdline upstream stable as 0.8.1; the releases page (fetched directly) shows v1.2.0 (2026-06-10). The releases page is authoritative; either way the tool works with any system `par2`.

Per-claim verdicts on the five load-bearing claims (both adversarial agents):

| # | Load-bearing claim | Verdict |
|---|---|---|
| 1 | ISO 9660 L3 multi-extent handles 20–93 GiB single files; Linux reads them back correctly | **supported** ×2 |
| 2 | xorriso/xorrecord is reliable + maintained for BD-R/BDXL, superior to growisofs | **supported** ×2 |
| 3 | Defect management ON is preferable for archival | **weakened** ×2 → default OFF, expose as option |
| 4 | par2 -r15 -n1 in-ISO + ddrescue recovery is sound | **supported with material sizing caveat** ×2 |
| 5 | Eject/reload + exact-count dd read-back verification | **supported** ×2, with readiness-poll and O_DIRECT additions |

## Recommendation

Build the tool on xorriso end-to-end: master with `-as mkisofs -iso-level 3 -rock --md5` to a staged ISO (bit-identical duplicate burns), burn with `-as cdrecord` in unformatted stream-recording mode, verify with eject/reload + readiness poll + exact-block-count SHA-256 (+ `iflag=direct` fallback) then mounted `sha256sum -c`, and offer `-check_media` for periodic source-free health checks. Compute par2 block count automatically (toward the 32768 ceiling), keep parity/checksums/LBA-manifest both on- and off-disc, never fill to the rim, and expose defect-management formatting (sized from `-list_formats`) and dvdisaster RS03 as options rather than defaults. Recommend 25 GB SL media, a fresh container per generation, external header backups, and two copies. **Biggest risk:** the optical hardware/media market collapse — mitigated operationally (spare drives, multiple media batches, periodic re-verification, planned migration), not in software. Second-order risk: par2 misconfiguration, which the tool eliminates by construction.

## Sources

See `sources_mdisc-archival-claims.md` for the full numbered table with dates and relevance notes. Key primary sources: ISO/IEC 9660:1999 text [1], Linux kernel isofs source [2], xorriso/xorrisofs/xorrecord man pages [3][4][5], growisofs man page [7], Debian/Launchpad/Arch bug trackers [10][11][12][14], Thomas Schmitt's mailing-list statements [15][16][17][30], PAR2 v2.0 specification [36], VeraCrypt official documentation [38][39][40][41], and Wikipedia's BD-R capacity tables cross-checked against T10/BDA-derived documents [25].
