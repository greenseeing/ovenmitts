# Sources: M-DISC BD-R Archival Claims Verification

## Research Scope
Verify the technical claims in `ref/initial-research.md` about archiving VeraCrypt containers to M-DISC BD-R on Linux: ISO 9660 level 3 multi-extent behavior, the xorriso/growisofs/par2 pipeline, the Brasero file-size limit, BD-R spare areas / defect management, UDF vs ISO trade-offs, and capacity figures for 25 GB BD-R and 100 GB BDXL. Audience: this project — the verified findings feed directly into the design of a burn tool. Six parallel researchers gathered primary sources; two adversarial researchers hunted contradictions and verified the load-bearing claims.

## Sources
| # | Source | URL | Date | Relevance |
|---|--------|-----|------|-----------|
| 1 | ISO/IEC 9660:1999 normative text (pismotec) | https://pismotec.com/cfs/iso9660-1999.html | 1999 | 32-bit Data Length field, multi-extent flag (bit 7), level 3 semantics |
| 2 | Linux kernel fs/isofs/inode.c | https://raw.githubusercontent.com/torvalds/linux/master/fs/isofs/inode.c | current | isofs_read_level3_size() multi-extent reassembly, 8 TB comment |
| 3 | xorrisofs(1) man page (Debian) | https://manpages.debian.org/testing/xorriso/xorrisofs.1.en.html | current | -iso-level 3, default 400 GiB/100-extent file cap, --md5/--for_backup, Rock Ridge default |
| 4 | xorriso(1) man page (Debian/Mankier) | https://manpages.debian.org/unstable/xorriso/xorriso.1.en.html | current | no-UDF statement, -check_media, -list_formats, POW-media refusal, -file_size_limit |
| 5 | xorrecord(1) man page (Debian) | https://manpages.debian.org/unstable/xorriso/xorrecord.1.en.html | current | BD-R unformatted-by-default, stream recording, blank=as_needed, speed caveats |
| 6 | GNU xorriso 1.5.8 release announcement (info-gnu) | https://lists.gnu.org/archive/html/info-gnu/2026-04/msg00001.html | 2026-04-02 | xorriso actively maintained |
| 7 | growisofs(1) man page | https://manpages.debian.org/unstable/growisofs/growisofs.1.en.html | 2008 | BD-R auto-format w/ spare areas, ~1/2 speed, undocumented -use-the-force-luke options, -dvd-compat scope |
| 8 | dvd+rw-format(1) man page | https://www.mankier.com/1/dvd+rw-format | current | -ssa= documented spare-area control |
| 9 | dvd+rw-tools — Wikipedia | https://en.wikipedia.org/wiki/Dvd%2Brw-tools | current | last release 7.1 (2008-03-05); spare:none "not recommended" note |
| 10 | Debian Bug #794868 — DAO burn failure kernel ≥3.x | https://bugs.debian.org/cgi-bin/bugreport.cgi?bug=794868 | 2015–2025 | unfixed-upstream growisofs bug, patched only downstream |
| 11 | Debian Bug #713016 — BD-R CLOSE SESSION | https://bugs.debian.org/cgi-bin/bugreport.cgi?bug=713016 | 2013–2015 | CLOSE SESSION bug patched in Debian 7.1-11 (not upstream) |
| 12 | LP #1424215 — growisofs overflow vs unformatted capacity | https://bugs.launchpad.net/ubuntu/+source/dvd+rw-tools/+bug/1424215 | 2015 | formatted capacity 24,220,008,448 B (~768 MiB spare observed) |
| 13 | LP #1113679 — hashes verify OK on DM-formatted disc | https://bugs.launchpad.net/ubuntu/+source/dvd+rw-tools/+bug/1113679 | 2013 | dd+hash read-back works on defect-managed BD-R |
| 14 | Arch Bug FS#47797 — growisofs spurious BD-R error | https://bugs.archlinux.org/task/47797 | 2016–2023 | patch never merged upstream in 7+ years |
| 15 | cdwrite ML: Schmitt on BD-R DL burning (msg14101) | https://www.mail-archive.com/cdwrite@other.debian.org/msg14101.html | 2015 | Schmitt recommends xorriso/cdrskin over growisofs for BD-R |
| 16 | cdwrite ML: Schmitt on M-DISC + -check_media | https://lists.debian.org/cdwrite/2022/06/msg00004.html | 2022-06 | M-DISC transparent to software; periodic -check_media advice |
| 17 | cdwrite ML: multi-extent status (msg13169) | https://www.mail-archive.com/cdwrite@other.debian.org/msg13169.html | 2010 | libisofs multi-extent implementation, reader-testing advice |
| 18 | cdwrite ML 2010-01: isofs unaligned-tail fix verified | https://lists.debian.org/cdwrite/2010/01/msg00075.html | 2010-01 | kernel bug for unaligned >4 GiB tails patched 2009-09-27 |
| 19 | debian-cd ML: 93.2 GiB BDXL burn via xorriso | http://www.mail-archive.com/debian-cd@lists.debian.org/msg26504.html | n.d. | real BDXL TL burn with xorriso -as cdrecord, ~2x actual speed |
| 20 | bug-xorriso 2016-01: BDXL support | https://lists.gnu.org/archive/html/bug-xorriso/2016-01/msg00004.html | 2016-01 | BDXL reports same profile 0x41 as BD-R; USB-3 timeout hypothesis |
| 21 | cdwrite ML 2012-09: growisofs BD-R DL layer-break failure | https://lists.debian.org/cdwrite/2012/09/msg00002.html | 2012-09 | growisofs failing at ~50% on BD-R DL; cdrskin succeeded |
| 22 | Gentoo wiki: CD/DVD/BD writing | https://wiki.gentoo.org/wiki/CD/DVD/BD_writing | current | DM capacity trap, spare:none required to fit full image, DM failure modes |
| 23 | ArchWiki: Optical disc drive | https://wiki.archlinux.org/title/Optical_disc_drive | current | eject+reinsert before verify (kernel cache) |
| 24 | libburn cookbook (BD-R SRM/POW/DM) | https://sources.debian.org/src/libburn/latest/doc/cookbook.txt/ | current | libburn BD-R write modes; DM formatting trade-offs |
| 25 | Blu-ray Disc recordable — Wikipedia | https://en.wikipedia.org/wiki/Blu-ray_Disc_recordable | current | exact capacities (25,025,314,816 / 50,050,629,632 / 100,103,356,416 / 128,001,769,472 B); spare areas; BDXL separate tier |
| 26 | Brasero LP #205919 — cannot burn 4 GiB files | https://bugs.launchpad.net/ubuntu/+source/brasero/+bug/205919 | 2008 | Brasero's actual limit is 4 GiB−1 (mkisofs-family backend, level 1/2) |
| 27 | Brasero GitLab #380 — maintainer status check-in | https://gitlab.gnome.org/GNOME/brasero/-/issues/380 | 2025-01 | Brasero effectively unmaintained |
| 28 | genisoimage(1) man page | https://linux.die.net/man/1/genisoimage | n.d. | UDF "alpha status", 1.02 hybrid only, no POSIX perms |
| 29 | mkudffs(8) man page | https://manpages.debian.org/testing/udftools/mkudffs.8.en.html | current | no metadata-partition creation; BD-R exception (no metadata partition needed) |
| 30 | debian-user ML: Schmitt on UDF vs ISO for backup | https://lists.debian.org/debian-user/2025/01/msg00493.html | 2025-01 | "UDF offers no practical advantages over ISO 9660 ... on GNU/Linux" |
| 31 | Phoronix: Linux 6.17 removes pktcdvd | https://www.phoronix.com/news/Linux-To-Remove-pktcdvd | 2025 | packet-writing driver removed (irrelevant to BD-R mastering) |
| 32 | K3b ChangeLog | https://github.com/KDE/k3b/blob/master/ChangeLog | historical | >2 GB files handled via auto-enabled UDF extensions, not level 3 |
| 33 | par2cmdline repo (README, man) | https://github.com/Parchive/par2cmdline | current | -r/-n/-b/-s semantics, 32768-block format ceiling, defaults |
| 34 | par2cmdline releases | https://github.com/Parchive/par2cmdline/releases | 2026-06 | v1.0.0 (2024) → v1.2.0 (2026-06-10): active again |
| 35 | par2cmdline-turbo | https://github.com/animetosho/par2cmdline-turbo | current | SIMD-accelerated fork for large files |
| 36 | PAR2 Specification v2.0 | https://parchive.github.io/doc/Parity%20Volume%20Set%20Specification%20v2.0.html | 2003 | zero-padded slices, per-slice CRC32+MD5 → ddrescue compatibility |
| 37 | par2(1) man page | https://manpages.debian.org/unstable/par2/par2.1.en.html | current | default 2000 blocks — the block-count trap |
| 38 | VeraCrypt FAQ | https://veracrypt.io/en/FAQ.html | current | container-on-CD/DVD mounting supported; (outdated) UDF advice |
| 39 | VeraCrypt Volume Format Specification | https://veracrypt.io/en/VeraCrypt%20Volume%20Format%20Specification.html | current | master key exists only in headers; backup header at end |
| 40 | VeraCrypt Program Menu docs | https://veracrypt.io/en/Program%20Menu.html | current | Backup/Restore Volume Header; embedded backup header |
| 41 | VeraCrypt Volume Clones | https://veracrypt.io/en/Volume%20Clones.html | current | warning against reusing one container across archive generations |
| 42 | Disk encryption theory — Wikipedia | https://en.wikipedia.org/wiki/Disk_encryption_theory | current | XTS error containment granularity |
| 43 | VeraCrypt GitHub issue #440 | https://github.com/veracrypt/VeraCrypt/issues/440 | wontfix | read-only *block device* mount edge case on Linux |
| 44 | M-DISC — Wikipedia | https://en.wikipedia.org/wiki/M-DISC | current | glassy carbon vs MABL, 2022 reformulation controversy, Millenniata bankruptcy |
| 45 | mdisc.com FAQ | https://www.mdisc.com/faq.html | n.d. | M-DISC BD-R spec-compliant with standard BD burners |
| 46 | LNE accelerated-aging report (Syylex/DVD) | https://www.lne.fr/sites/default/files/inline-files/syylex-glass-dvd-accelerated-aging-report.pdf | 2010–2012 | M-DISC DVD performed worst of inorganic discs tested |
| 47 | How-To Geek: M-DISC history | https://www.howtogeek.com/this-would-be-the-best-cold-storage-format-out-there-except-for-this-reason/ | recent | current M-DISC BD shares media IDs with standard inorganic BD-R |
| 48 | Rosehill: M-DISC vs regular Blu-ray debate | https://danielrosehill.medium.com/on-the-great-m-disc-vs-regular-blu-ray-debate-4318eaf37ee5 | n.d. | M-DISC advantage over HTL BD-R unresolved |
| 49 | Gough Lui: Experimenting with BDXL Part 2 | https://goughlui.com/2024/10/27/experimenting-with-bdxl-part-2-burning-some-discs/ | 2024-10 | BDXL failures concentrate at outer region / later layers |
| 50 | Gough Lui: Verbatim M-DISC BD-R 25GB review | https://goughlui.com/2015/10/16/review-tested-verbatim-lifetime-archival-millenniatam-disc-4x-bd-r-25gb/ | 2015-10 | packaging fine print: "several hundred years", not 1000 |
| 51 | NIST/LOC Optical Disc Longevity Study | https://www.loc.gov/preservation/resources/rt/NIST_LC_OpticalDiscLongevity.pdf | c.2007 | accelerated-aging methodology; severe conditions shorten life |
| 52 | CLIR pub121 §4/§5 | https://www.clir.org/pubs/reports/pub121/sec4/ | n.d. | dye degradation kinetics vs heat/humidity |
| 53 | Tom's Hardware: Verbatim & I-O Data supply pledge | https://www.tomshardware.com/tech-industry/verbatim-and-i-o-data-extend-blu-ray-supply-pledge-as-manufacturers-exit-the-market | 2025/26 | media supply base contracting |
| 54 | Blocks & Files: Sony quits recordable BD | https://www.blocksandfiles.com/disk/2025/02/03/sony-quits-recordable-blu-ray-disc-market/1601137 | 2025-02 | Sony exit |
| 55 | TechPowerUp: Pioneer ends BD drive production | https://www.techpowerup.com/336803/pioneer-has-ended-production-of-computer-blu-ray-drives-transfers-pddm-business-to-shanxi-group | 2025 | drive supply base contracting |
| 56 | darbrrb | https://github.com/jaredjennings/darbrrb | n.d. | closest existing tool (dar+par2+growisofs); origin: a failed restore |
| 57 | bdarchiver | https://github.com/salfter/bdarchiver | n.d. | shell suite: mkisofs+growisofs+dvdisaster |
| 58 | speed47/dvdisaster fork | https://github.com/speed47/dvdisaster | 2025 | maintained image-level RS03 ECC, complementary to par2 |
| 59 | dvdisaster.jcea.es | https://dvdisaster.jcea.es/ | current | upstream continuation |
| 60 | TLP FAQ: USB (autosuspend) | https://linrunner.de/tlp/faq/usb.html | current | USB autosuspend can kill long burns |
| 61 | Arch forums: drive not ready after eject -t | https://bbs.archlinux.org/viewtopic.php?id=184453 | 2014 | verify needs drive-readiness poll after reload |
| 62 | debian.user: xorriso report_lba for damaged media | https://groups.google.com/g/linux.debian.user/c/LfS3jWxsn6M | n.d. | file→LBA manifest enables mount-free recovery |
| 63 | Verbatim 98914 M-DISC BDXL 100GB product page | https://www.verbatim.com/en/m-disc/products/98914-m-disc-bdxl-100gb-6x-with-branded-surface-25pk-spindle | current | 100 GB M-DISC SKUs still sold |
| 64 | cdw project (SourceForge, closed 2023) | https://sourceforge.net/projects/cdw/ | 2023 | last ncurses burn frontend is dead — tooling gap real |
| 65 | G-Loaded: verify burned image on Linux | https://www.g-loaded.eu/2006/10/07/verify-a-burned-cddvd-image-on-linux/ | 2006 | exact-block-count dd read-back method |
| 66 | NIST SP 500-252: Care and Handling of CDs/DVDs | https://nvlpubs.nist.gov/nistpubs/legacy/sp/NISTspecialpublication500-252.pdf | 2003 | storage/handling standards |
| 67 | VideoHelp: tips burning Verbatim BD-R M-DISC | https://forum.videohelp.com/threads/404334-Tips-on-burning-data-to-Verbatim-BD-R-M-Discs | n.d. | community consensus: single-layer > DL/TL for archival |
| 68 | Red Hat Bugzilla #167036 | https://bugzilla.redhat.com/show_bug.cgi?id=167036 | 2005 | growisofs has no built-in verify |
| 69 | ECMA-119 6th edition | https://ecma-international.org/wp-content/uploads/ECMA-119_6th_edition_december_2025.pdf | 2025-12 | no technical change to multi-extent mechanics |
