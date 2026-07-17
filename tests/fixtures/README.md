# Fixture provenance

Real xorriso / dvd+rw-mediainfo output captured from published sources; line
formats cross-checked against the printf statements in xorriso 1.5.6 source
(`xorriso/drive_mgt.c`, `libburn/mmc.c`) and dvd+rw-tools 7.1
(`dvd+rw-mediainfo.cpp`).

| fixture | source |
|---|---|
| `xorriso_toc_bdr_blank_mdisc.txt` | blank Millenniata M-DISC BD-R 25, https://www.mail-archive.com/cdwrite@other.debian.org/msg14517.html |
| `xorriso_toc_bdr_pow.txt` | POW-formatted Verbatim BD-R 50, same message |
| `xorriso_toc_dvdrw_closed.txt` | closed DVD-RW, xorriso 1.5.6 `doc/qemu_xorriso.wiki` |
| `xorriso_list_formats_bdr_blank.txt` | blank BD-R 25, https://lists.gnu.org/archive/html/bug-xorriso/2021-01/msg00000.html |
| `xorriso_list_formats_bdre_formatted.txt` | formatted BD-RE format block from xorriso 1.5.6 `doc/qemu_xorriso.wiki` ("Format status: formatted, with 23610.0 MiB"); short-toc header lines assembled per `Xorriso_toc()` output format, "Media summary ... 22.4g free" line from the same doc |
| `xorriso_probe_bdxl_blank.txt` | BDXL 100 format descriptors + "Write speed L/H" from https://lists.gnu.org/archive/html/bug-xorriso/2016-01/msg00012.html and msg00013.html; short-toc header assembled (blank BDXL free = 93.2g) |
| `xorriso_probe_bdr_blank_full.txt` | combined `-toc -list_formats -list_speeds` single-invocation output, assembled from the two real BD-R samples above; speed lines follow the exact `Xorriso_list_speeds_sub()` printf formats and the 2x/4x kB values reported in the 2016 thread |
| `dvd_rw_mediainfo_bdr_pow.txt` | TDK BD-R SRM+POW, https://lists.debian.org/cdwrite/2010/10/msg00049.html |
