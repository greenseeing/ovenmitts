# Findings: M-DISC BD-R Archival Claims Verification

## Research Scope
Verify the technical claims in `ref/initial-research.md` about archiving VeraCrypt containers to M-DISC BD-R on Linux: ISO 9660 level 3 multi-extent behavior, the xorriso/growisofs/par2 pipeline, the Brasero file-size limit, BD-R spare areas / defect management, UDF vs ISO trade-offs, and capacity figures for 25 GB BD-R and 100 GB BDXL. Findings feed the design of a burn tool built in this repo.

## One-Line Summary
The initial research's architecture (ISO 9660 level 3 + parity + rigorous verification) survives verification, but its burn tool (growisofs) must be replaced with xorriso end-to-end, its defect-management advice flips to off-by-default, and its par2 command silently under-provisions the one parameter that decides real recoverability (block count).

## Summary
The core filesystem claims are solid: ISO 9660 level 3 multi-extent files remove the 4 GiB limit, xorriso masters them correctly, and the Linux kernel reassembles them transparently (verified against the spec text and kernel source). Exact capacities check out (25,025,314,816 B for BD-R 25; 100,103,356,416 B for BDXL 100 — unformatted). The par2 layer and ddrescue recovery path are sound in mechanism. But the pipeline itself needs three corrections. First, growisofs is unmaintained since 2008 with BD-R-specific defects, and its default POW-formatting renders media unusable to other tools; xorriso (actively maintained, 1.5.8 April 2026, with a real-world 93.2 GiB BDXL burn on record) should master *and* burn. Second, "leave defect management on" is a contested opinion, not settled practice — libburn's author defaults to unformatted stream recording at full speed and capacity, relying on post-burn verification, which this pipeline already does; formatting also costs ~768 MiB observed (not 256 MiB). Third, `par2 create -r15 -n1` with default block count (2000) produces ~46 MiB slices on a 93 GiB container — 300 scattered sector errors defeat 15% redundancy; block count must be raised toward the 32768 format ceiling. Adversarial passes added: parity data lands on the disc's outer rim where failures concentrate; a fresh VeraCrypt container should be created per archive generation (VeraCrypt's own Volume Clones warning); and the biggest threat is market-side — the 2024–2026 collapse of drive/media manufacturing, which no software choice mitigates.

## Key Findings
- ISO 9660 stores extent length in 32 bits (max aligned extent 4,294,965,248 B); level 3 chains extents via directory-record flag bit 7; no spec cap on extents per file [1] — _confidence: high_
- The Linux kernel isofs driver reassembles multi-extent chains transparently (`isofs_read_level3_size()`); the unaligned-tail read bug was fixed 2009-09-27 and verified even on RHEL5 [2][18] — _confidence: high_
- xorriso's default per-file cap at level 3 is 400 GiB − 200 KiB (100 extents) — an implementation default, not a spec limit; irrelevant below 100 GB [3] — _confidence: high_
- xorriso does not produce UDF at all; its author states UDF "offers no practical advantages over ISO 9660" for Linux-read backups [4][30] — _confidence: high_
- Brasero's real limit is 4 GiB−1 (not 2 GiB), caused by mkisofs-family mastering at interchange level 1/2; Brasero is effectively unmaintained (2025–26 GNOME maintainer check-ins unanswered) [26][27] — _confidence: high_
- growisofs/dvd+rw-tools: last upstream release 7.1 (2008-03-05); DAO ≥3.x-kernel bug, spurious BD-R error, and overflow-check-against-unformatted-capacity bug remain unfixed upstream; the CLOSE SESSION bug is patched in distro packages since 2015 [9][10][11][12][14] — _confidence: high_
- growisofs POW-formats blank BD-R by default; xorriso then classifies such media "unsuitable: is POW formatted" — mixing the two tools on one disc breaks the workflow [4][7] — _confidence: high_
- xorriso burns BD-R/BDXL directly (`-as cdrecord` / xorrecord); a real 93.2 GiB BDXL TL burn is on record with the author assisting; xorriso 1.5.8 released 2026-04 [5][6][19][20] — _confidence: medium-high_
- BD-R defect management: growisofs formats by default (spare areas, ~½ speed); xorriso/libburn never format by default and document unformatted full-speed stream recording as normal; observed spare-area cost 768 MiB (24,220,008,448 B formatted vs 25,025,314,816 B unformatted) [5][7][12][22][24] — _confidence: high_
- Exact capacities: BD-R 25 = 25,025,314,816 B; BD-R DL 50 = 50,050,629,632 B; BDXL 100 = 100,103,356,416 B; BD-R XL 128 = 128,001,769,472 B (all unformatted) [25] — _confidence: high_
- Capacity preflight must read formatted capacity (`xorriso -list_formats`) if formatting, or unformatted Free Blocks if stream-recording — blank-disc "Free Blocks" reports unformatted capacity [12][22] — _confidence: high_
- par2cmdline is actively maintained again (v1.0.0 Apr 2024 → v1.2.0 Jun 2026); par2cmdline-turbo is the SIMD-fast fork; PAR2 format ceiling is 32768 blocks [33][34][35] — _confidence: high_
- par2 default block count (2000) is the recoverability trap: ~46 MiB slices on 93 GiB; 300 scattered 2 KiB errors in distinct slices defeat 15% parity despite <1 MiB physical damage — set `-b` near 32768 [36][37] — _confidence: high_
- PAR2 zero-pads incomplete slices by design and locates slices via per-slice CRC32+MD5, so the ddrescue (zero-fill, full-length) → par2repair path is mechanically sound [36] — _confidence: high_
- Parity placement: files written last land at the disc's outer region/last layer, exactly where BDXL failures were observed — keep parity also off-disc, and avoid filling discs to the rim [49] — _confidence: medium_
- Post-burn verification: eject/reload defeats the page cache (poll drive readiness after reload); read exactly ISO-size/2048 blocks and hash; `dd iflag=direct` is the cache-bypass for clamshell drives that can't auto-reload; xorriso `--md5` session tags + `-check_media` enable source-free future verification [16][23][61][65] — _confidence: high_
- VeraCrypt: mounting a container from CD/DVD is officially supported; master key exists only in the two headers (single point of total failure — keep an external header backup); XTS confines a bit error with no cascade (practical blast radius: the 2048-B disc sector → four 512-B data units) [38][39][40][42] — _confidence: high_
- VeraCrypt Volume Clones warning: burning diverged copies of the *same* container (same master key) to indestructible media is the pattern VeraCrypt warns against — create a fresh container per archive generation [41] — _confidence: high_
- M-DISC BD-R burns in standard BD burners (BDA spec-compliant) [44][45] — _confidence: high_; but its longevity advantage over regular inorganic HTL BD-R is disputed: 2022 reformulation, shared media IDs with standard inorganic BD-R, and the LNE study rated M-DISC DVD worst of the inorganic discs tested [44][46][47][48] — _confidence: medium_
- 25 GB single-layer is the safest format: readable by every BD drive tier, and community experience finds SL burns/ages more reliably than DL/TL/BDXL [25][49][67] — _confidence: medium_
- Market risk dominates: Pioneer and LG exited drives (2024–25), Sony quit recordable BD media (2025); Verbatim + I-O Data pledged continued supply; spare drives are part of any 20-year plan [53][54][55] — _confidence: high_
- Tooling gap is real: cdw (last ncurses burn frontend) closed 2023; darbrrb/bdarchiver wrap the aging growisofs; nothing maintained occupies the "archival BD-R with parity + verification" niche [56][57][64] — _confidence: medium-high_
- dvdisaster lives on in maintained forks (speed47, jcea); RS03 image-level ECC protects filesystem metadata that par2 cannot — a complementary, optional layer [58][59] — _confidence: high_

## Claim Verification
Verdicts on the original document's claims (initial-research.md):

- ISO level 3 multi-extent removes the 4 GiB limit; xorriso implements it correctly — **supported** [1][3][17]
- Kernel isofs reassembles multi-extent transparently; VeraCrypt can mount straight off the disc — **supported** [2][18][38]
- Usable capacity 25,025,314,816 B (25 GB) / ~93.1 GiB (100 GB) — **supported** for unformatted media; **weakened** if defect-management formatting is applied (−768 MiB observed) [12][25]
- Brasero caps files at 2 GiB due to ISO level 1/2 — **weakened**: mechanism right, threshold actually 4 GiB−1 [26]
- growisofs `-Z` burn is the reliable BD-R path — **weakened**: unmaintained since 2008, multiple BD-R defects, author of the successor stack recommends xorriso/cdrskin [9][10][12][14][15]
- growisofs formats BD-R with spare areas by default, halving speed — **supported** [7]
- Leave defect management on for archival — **weakened**: contested between toolchain authors; libburn defaults off; redundant given read-back verify + par2; costs capacity and can fail more on marginal media [5][22][24]
- Burn at 4x for M-DISC — **unverified**: community folklore, no primary source; drives override requested speeds anyway [5][19]
- `-dvd-compat` is ignored/harmless on BD-R — **refuted** as stated: undefined on BD, interacts with session-close behavior [7][11]
- Two-stage verify (exact-count dd + mounted checksums) with eject/reload — **supported**; add drive-readiness poll and `iflag=direct` fallback [23][61][65]
- `par2 create -r15 -n1` correct; parity critical for encrypted payloads — **supported** with a material caveat: default block count must be overridden (`-b`) or real recoverability is far below the nominal 15% [33][36][37]
- ddrescue → extract → par2 repair recovery path — **supported** (zero-fill + full-length caveats) [36]
- XTS bit error damages only the 16-byte AES block, no cascade — **supported** in direction (no cascade); granularity nuance: plan around 2048-B disc sectors [42]
- Header backup advice (embedded + external) — **supported**; missed factor: don't reuse one container across generations (Volume Clones) [39][40][41]
- UDF is the wrong choice for this use case on Linux — **supported** overall (tooling maturity, xorriso can't write UDF) [4][28][30]; two sub-arguments **weakened**: kernel UDF 2.5/2.6 *read* support has been fine since 2008, and mkudffs *can* target BD-R without a metadata partition; note VeraCrypt's own FAQ still (outdatedly) recommends UDF [29][38]
- Multi-extent files read correctly on Linux, macOS, Windows Vista+ — **weakened**: Linux/Windows evidenced; macOS evidence contradicts (10.4.8 cd9660 has no level 3 support, no fix documented); "pre-2007" cutoff unsourced [17]
- K3b handles >4 GiB by enabling level 3 — **refuted** as stated: K3b's changelog credits auto-enabled UDF extensions [32]
- M-DISC BD-R burns in any standard BD burner — **supported** [44][45]; M-DISC longevity superiority over regular HTL BD-R — **weakened** (2022 reformulation, shared media IDs, LNE results) [44][46][47]
- BDXL narrows the reader pool; 25 GB SL is safest long-term — **supported**, strengthened by the 2024–26 hardware-market collapse and SL reliability consensus [25][53][54][55][67]
- DVD-R organic dye lasts 5–15 years in hot/humid climates — **directionally supported**, exact range is a synthesis, not a sourced statistic [51][52]

## Counter-Arguments
- VeraCrypt's own FAQ recommends UDF for burning containers — the rationale (ISO can't exceed 2 GB) is outdated, but the vendor's written guidance points the other way [38]
- The growisofs CLOSE SESSION bug is fixed in distro packages since 2015 — "growisofs is buggy" partly reflects upstream, not what Debian actually ships [11]
- mkudffs can create UDF for BD-R without a metadata partition, and pktcdvd was never needed for premastered burning — two of the original four anti-UDF arguments don't apply to BD-R [29][31]
- Defect management ON has a real defense: write-time verify + drive-portable defect list; no evidence POW-formatted discs read worse in other drives, and dd+hash verification demonstrably works on DM-formatted media [13]
- No public test exists of a single ~90 GiB multi-extent file (~23 extents) specifically — image-scale is proven, single-file extent count is extrapolated [19]
- M-DISC-specific xorriso burn reports are sparse ("rarely tested" per its author) — M-DISC presents as ordinary BD-R to software, so this is low-risk, but unproven [16]

## Recommendation
Build the tool on **xorriso end-to-end**: master with `xorriso -as mkisofs -iso-level 3 -rock --md5` (staged ISO for bit-identical duplicate burns), burn with `xorriso -as cdrecord` (stream recording, unformatted, full speed — no growisofs anywhere in the pipeline), verify with eject/reload + readiness poll + exact-block-count SHA-256 read-back (+ `iflag=direct` fallback), then mounted `sha256sum -c`, with `xorriso -check_media` available for source-free periodic re-verification. Parity: par2 (turbo if present) at ≥15% with block count scaled toward the 32768 ceiling, parity + checksums + a file→LBA manifest kept on-disc *and* off-disc, and discs never filled to the rim. Expose defect-management formatting as an explicit option, sizing from `-list_formats`, not as the default. Recommend 25 GB SL media over BDXL, a fresh VeraCrypt container per generation, external header backups, and two copies per archive. The biggest risk to this recommendation is not software: it is the 2024–2026 collapse of the optical drive/media manufacturing base — mitigation (spare drives, media from multiple batches, periodic `-check_media`, planned migration) must be part of the operating practice, and the second-order risk is par2 misconfiguration, which the tool eliminates by computing block count automatically.
