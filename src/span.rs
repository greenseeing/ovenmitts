//! Multi-disc spanning: a payload that cannot fit one disc burns as a set of
//! N balanced data discs plus one parity disc whose single par2 recovery set
//! rebuilds the entire contents of any ONE lost disc from the survivors.
//! This module owns the pure planning math ([`plan_span`]), the streaming
//! source splitter ([`split`]), and the on-disc set catalog
//! ([`write_set_txt`]); everything below the plan is the existing per-disc
//! machinery.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};

use crate::plan::{self, Payload};

pub const SPAN_MARGIN_PCT: u64 = 6; // per-disc reserve: RS02 floor (5%) + par2 non-MDS slack
pub const MAX_SET_DISCS: u32 = 10; // data + parity; ~187 GiB payload ceiling on BD-R 25
const R_SLACK_NUM: u64 = 102; // R must be >= ceil(R_need * 1.02)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscRole {
    Data,
    Parity,
}

impl DiscRole {
    pub fn as_str(self) -> &'static str {
        match self {
            DiscRole::Data => "data",
            DiscRole::Parity => "parity",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartPlan {
    pub file_name: String, // "vault.hc.001" (1-based, fixed width 3)
    pub offset: u64,       // byte offset in the source
    pub bytes: u64,        // 2048-multiple except possibly the last
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscPlan {
    pub index: u32,    // 1-based; parity disc is last
    pub label: String, // "<BASE>_1OF4"
    pub role: DiscRole,
    pub part: Option<PartPlan>, // Some for data discs, None for the parity disc
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanPlan {
    pub base_label: String,   // truncated to 32 - suffix.len()
    pub discs: Vec<DiscPlan>, // len = n_data + 1 (or n_data with --no-parity)
    pub source_name: String,
    pub source_bytes: u64,
    pub block: u64,            // par2 -s, from plan::slice_bytes_for(S, n_data)
    pub recovery_blocks: u64,  // par2 -c; 0 when parity disabled
    pub per_disc_iso_est: u64, // = budget when ecc on (RS02 fills), else content+overhead
    pub staging_peak: u64,     // S + R*block + (discs)*per_disc_iso_est
}

/// Everything the per-disc MANIFEST/RECOVERY writers need to know about the
/// set a disc belongs to. Carries runtime facts too (the source sha256 only
/// exists after the Split stage), so it is not part of [`SpanPlan`].
#[derive(Debug, Clone)]
pub struct SpanNote {
    pub disc: u32,
    pub of: u32,
    pub role: DiscRole,
    pub source_name: String,
    pub source_bytes: u64,
    pub source_sha256: String,
    pub source_container: bool,
    pub recovery_blocks: u64,
    pub block: u64,
}

impl SpanNote {
    /// Data-part count of the set (the parity disc, when present, is last).
    pub fn data_parts(&self) -> u32 {
        if self.recovery_blocks > 0 {
            self.of - 1
        } else {
            self.of
        }
    }

    pub fn part_names(&self) -> Vec<String> {
        (1..=self.data_parts())
            .map(|k| part_file_name(&self.source_name, k))
            .collect()
    }
}

/// "vault.hc.001" - 1-based, fixed width 3, so `cat vault.hc.???` reassembles
/// in glob order.
pub fn part_file_name(source_name: &str, k: u32) -> String {
    format!("{source_name}.{k:03}")
}

/// "<BASE>_{k}OF{n}" with k zero-padded to n's digit count - constant width,
/// sorts correctly. The base is truncated so the label stays within the
/// 32-char ISO volume-label budget. `base` is sanitize_label output.
pub fn disc_label(base: &str, k: u32, n: u32) -> String {
    let w = n.to_string().len();
    format!("{}_{k:0w$}OF{n}", truncated_base(base, n))
}

fn truncated_base(base: &str, n: u32) -> String {
    let suffix_len = 3 + 2 * n.to_string().len(); // "_kOFn"
    base.chars()
        .take(32usize.saturating_sub(suffix_len))
        .collect()
}

// The set's par2 index file (on every disc): ~20 bytes of IFSC checksum per
// source block plus per-file packets. Only the order of magnitude matters -
// the 6% margin absorbs the residue.
fn index_est(total_blocks: u64, files: u64) -> u64 {
    total_blocks * 20 + files * 512 + 64 * 1024
}

/// Plan a multi-disc set for a payload that does not fit one disc. Pure.
///
/// `label` is the sanitized volume label the single-disc path would use
/// (becomes the `<BASE>_kOFn` per-disc labels); `budget` is the per-disc
/// headroom-adjusted byte budget (plan.budget - DM-formatted capacity when
/// applicable); `overhead_pad` is the plan's ISO overhead estimate
/// (plan.overhead_bytes_est); `parity` adds the dedicated parity disc; `ecc`
/// means an RS02 layer will fill every ISO to the budget (it sizes
/// per_disc_iso_est and staging_peak only).
///
/// Ok(None): spanning does not apply - the payload fits one disc, is a
/// directory, or is not exactly one payload (v1 spans a single file).
/// Err: the payload cannot be spanned (would exceed [`MAX_SET_DISCS`]).
pub fn plan_span(
    payloads: &[Payload],
    label: &str,
    budget: u64,
    overhead_pad: u64,
    parity: bool,
    ecc: bool,
) -> Result<Option<SpanPlan>> {
    let [payload] = payloads else {
        return Ok(None);
    };
    if payload.is_dir {
        return Ok(None);
    }
    let s = payload.total_size;
    let usable = budget.saturating_sub(overhead_pad);
    let d = usable.saturating_mul(100 - SPAN_MARGIN_PCT) / 100;
    ensure!(
        d > 0,
        "cannot span onto this medium: the {} budget leaves no part capacity \
         after overhead",
        plan::human_bytes(budget)
    );
    if s <= d {
        return Ok(None);
    }

    let source_name = payload.name.clone();
    let mut n_data = s.div_ceil(d);
    let (parts, block, recovery_blocks, index_pad) = loop {
        ensure!(
            n_data + u64::from(parity) <= MAX_SET_DISCS as u64,
            "{} needs {n_data} data discs{} - the set ceiling is {MAX_SET_DISCS} \
             discs; split the payload or use larger media",
            plan::human_bytes(s),
            if parity { " + 1 parity" } else { "" }
        );
        // balanced fill minimizes the max per-disc block count (maximizes the
        // R margin) and keeps every disc equally far from the rim
        let part = s.div_ceil(n_data).div_ceil(plan::SECTOR) * plan::SECTOR;
        if part > d {
            // the 2048 round-up crossed the margin line (S ~ n*D edge)
            n_data += 1;
            continue;
        }
        let last = s - part * (n_data - 1);
        assert!(
            last > 0 && last <= part,
            "unbalanced split: {n_data} x {part} for {s}"
        );
        let parts: Vec<PartPlan> = (1..=n_data)
            .map(|k| PartPlan {
                file_name: part_file_name(&source_name, k as u32),
                offset: (k - 1) * part,
                bytes: if k == n_data { last } else { part },
            })
            .collect();
        let block = plan::slice_bytes_for(s, n_data);
        if !parity {
            break (parts, block, 0, 0);
        }
        let per_part: Vec<u64> = parts.iter().map(|p| p.bytes.div_ceil(block)).collect();
        let r_need = *per_part.iter().max().expect("at least one part");
        let index_pad = index_est(per_part.iter().sum(), n_data);
        // the parity disc reserves the same margin, so it gets RS02 too
        let r = usable.saturating_sub(index_pad) * (100 - SPAN_MARGIN_PCT) / 100 / block;
        if r < (r_need * R_SLACK_NUM).div_ceil(100) {
            // smaller parts shrink R_need by ~n/(n+1) per step; the disc
            // ceiling above bounds the walk
            n_data += 1;
            continue;
        }
        break (parts, block, r, index_pad);
    };

    let total = (n_data + u64::from(parity)) as u32;
    let mut discs: Vec<DiscPlan> = parts
        .iter()
        .enumerate()
        .map(|(i, p)| DiscPlan {
            index: i as u32 + 1,
            label: disc_label(label, i as u32 + 1, total),
            role: DiscRole::Data,
            part: Some(p.clone()),
        })
        .collect();
    if parity {
        discs.push(DiscPlan {
            index: total,
            label: disc_label(label, total, total),
            role: DiscRole::Parity,
            part: None,
        });
    }
    let content_max = parts[0].bytes.max(recovery_blocks * block + index_pad);
    let per_disc_iso_est = if ecc {
        budget
    } else {
        content_max + overhead_pad
    };
    let staging_peak = s + recovery_blocks * block + discs.len() as u64 * per_disc_iso_est;
    Ok(Some(SpanPlan {
        base_label: truncated_base(label, total),
        discs,
        source_name,
        source_bytes: s,
        block,
        recovery_blocks,
        per_disc_iso_est,
        staging_peak,
    }))
}

/// Post-split facts the plan cannot know: the hashes.
#[derive(Debug, Clone)]
pub struct SpanSet {
    /// (sha256, file name) per part, in part order.
    pub part_shas: Vec<(String, String)>,
    pub whole_sha: String,
}

const CHUNK: usize = 1024 * 1024;

/// Slice `source` into `set_dir/<part file>`s in ONE streaming pass,
/// computing every per-part sha256 AND the whole-file sha256 as the bytes go
/// by; each part is fsynced (data and directory entry) before the next
/// begins. cb(bytes_done, bytes_total) for progress.
pub fn split(
    source: &Path,
    parts: &[PartPlan],
    set_dir: &Path,
    cb: &mut dyn FnMut(u64, u64),
) -> Result<SpanSet> {
    ensure!(!parts.is_empty(), "split: no parts planned");
    let total: u64 = parts.iter().map(|p| p.bytes).sum();
    let mut expected = 0u64;
    for p in parts {
        ensure!(
            p.offset == expected,
            "split: parts are not contiguous at {}",
            p.file_name
        );
        expected += p.bytes;
    }
    let len = std::fs::metadata(source)
        .with_context(|| format!("stat {}", source.display()))?
        .len();
    ensure!(
        len == total,
        "{} is {len} bytes but the plan sliced {total} - source changed since \
         planning, re-run",
        source.display()
    );
    std::fs::create_dir_all(set_dir).with_context(|| format!("create {}", set_dir.display()))?;
    let mut src =
        std::fs::File::open(source).with_context(|| format!("open {}", source.display()))?;
    let mut whole = Sha256::new();
    let mut part_shas = Vec::with_capacity(parts.len());
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    cb(0, total);
    for p in parts {
        let path = set_dir.join(&p.file_name);
        // 0600 like every staged artifact: parts ARE the private payload
        let mut out = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("write {}", path.display()))?
        };
        let mut hasher = Sha256::new();
        let mut remaining = p.bytes;
        while remaining > 0 {
            let want = remaining.min(CHUNK as u64) as usize;
            let n = src
                .read(&mut buf[..want])
                .with_context(|| format!("read {}", source.display()))?;
            ensure!(
                n > 0,
                "{} ended {remaining} bytes early in {} - source changed during \
                 the split, re-run",
                source.display(),
                p.file_name
            );
            hasher.update(&buf[..n]);
            whole.update(&buf[..n]);
            out.write_all(&buf[..n])
                .with_context(|| format!("write {}", path.display()))?;
            remaining -= n as u64;
            done += n as u64;
            cb(done, total);
        }
        drop(out);
        crate::fsutil::fsync_existing(&path)?;
        part_shas.push((hex(&hasher.finalize()), p.file_name.clone()));
    }
    Ok(SpanSet {
        part_shas,
        whole_sha: hex(&whole.finalize()),
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// SET.txt: the set catalog, written once and byte-identical on every disc.
/// Contains no ISO hashes (chicken-and-egg) and never names the tool -
/// recovery needs only cat, sha256sum and par2. `created` is passed in so
/// the output is a pure function of its inputs. `parity_shas` are
/// (sha256, disc-relative path) rows for the parity artifacts, in
/// checksums order (index first, then volumes; empty with --no-parity).
pub fn write_set_txt(
    out: &Path,
    span: &SpanPlan,
    set: &SpanSet,
    parity_shas: &[(String, String)],
    media_label: &str,
    created: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    use std::fmt::Write as _;
    let data: Vec<&DiscPlan> = span
        .discs
        .iter()
        .filter(|d| d.role == DiscRole::Data)
        .collect();
    let n_total = span.discs.len();
    let n_data = data.len();
    let part_list = data
        .iter()
        .map(|d| {
            d.part
                .as_ref()
                .expect("data disc without part")
                .file_name
                .as_str()
        })
        .collect::<Vec<_>>()
        .join(" ");

    let mut t = String::new();
    let _ = writeln!(t, "ARCHIVE SET  {}", span.base_label);
    let _ = writeln!(
        t,
        "set id: {}-{}",
        span.base_label,
        created.format("%Y%m%dT%H%M%SZ")
    );
    let _ = writeln!(t, "created: {}", created.format("%Y-%m-%d %H:%M:%S UTC"));
    let _ = writeln!(
        t,
        "discs: {n_total} total = {n_data} data + {} parity ({media_label})",
        n_total - n_data
    );
    let _ = writeln!(
        t,
        "This file is identical on every disc of the set. Any one disc is enough"
    );
    let _ = writeln!(t, "to know what the whole set contains.");
    let _ = writeln!(t);
    let _ = writeln!(t, "DISCS");
    let kw = n_total.to_string().len();
    for disc in &span.discs {
        let desc = match &disc.part {
            Some(p) => p.file_name.as_str(),
            None => "par2 recovery volumes",
        };
        let _ = writeln!(
            t,
            "  {:>kw$}/{n_total}  {}  {:<6}  {desc}",
            disc.index,
            disc.label,
            disc.role.as_str()
        );
    }
    let _ = writeln!(t);
    let _ = writeln!(t, "SOURCE FILE");
    let _ = writeln!(t, "  name:   {}", span.source_name);
    let _ = writeln!(
        t,
        "  bytes:  {} ({})",
        span.source_bytes,
        plan::human_bytes(span.source_bytes)
    );
    let _ = writeln!(t, "  sha256: {}", set.whole_sha);
    let _ = writeln!(
        t,
        "  parts (offset = byte position in {}):",
        span.source_name
    );
    let parts: Vec<&PartPlan> = data
        .iter()
        .map(|d| d.part.as_ref().expect("data disc without part"))
        .collect();
    let bw = parts
        .iter()
        .map(|p| p.bytes.to_string().len())
        .max()
        .unwrap_or(1);
    let ow = parts
        .iter()
        .map(|p| p.offset.to_string().len())
        .max()
        .unwrap_or(1);
    for disc in &data {
        let p = disc.part.as_ref().expect("data disc without part");
        let _ = writeln!(
            t,
            "    {}  {:>bw$} bytes  offset {:<ow$}  disc {}",
            p.file_name, p.bytes, p.offset, disc.index
        );
    }
    let _ = writeln!(t);
    let _ = writeln!(t, "REASSEMBLE (copy every part into one directory first)");
    let _ = writeln!(t, "  cat {part_list} > {}", span.source_name);
    let _ = writeln!(
        t,
        "  printf '{}  {}\\n' | sha256sum -c",
        set.whole_sha, span.source_name
    );
    let _ = writeln!(t);
    let _ = writeln!(
        t,
        "CHECKSUMS (sha256sum -c compatible from a directory holding the copies)"
    );
    for (sha, name) in &set.part_shas {
        let _ = writeln!(t, "{sha}  {name}");
    }
    for (sha, rel) in parity_shas {
        let _ = writeln!(t, "{sha}  {rel}");
    }
    let _ = writeln!(t);
    if span.recovery_blocks > 0 {
        let _ = writeln!(t, "PARITY");
        let _ = writeln!(t, "  one par2 set spans the {n_data} part files above");
        let _ = writeln!(
            t,
            "  block {} bytes, {} source-block ceiling, {} recovery blocks",
            span.block,
            plan::PAR2_MAX_BLOCKS,
            span.recovery_blocks
        );
        let _ = writeln!(
            t,
            "  the index /parity/{}.par2 is on EVERY disc; the recovery volumes",
            span.source_name
        );
        let _ = writeln!(t, "  are on disc {n_total} only");
        let _ = writeln!(
            t,
            "  the recovery volumes can rebuild the ENTIRE contents of any ONE lost or"
        );
        let _ = writeln!(t, "  unreadable disc from the surviving discs:");
        let _ = writeln!(
            t,
            "    copy every readable part from the surviving discs into one directory,"
        );
        let _ = writeln!(
            t,
            "    add /parity/* from disc {n_total} (ddrescue what reads poorly), then:"
        );
        let _ = writeln!(t, "      par2 r {}.par2", span.source_name);
        let _ = writeln!(
            t,
            "    par2 recreates the missing part; reassemble and check as above."
        );
        let _ = writeln!(
            t,
            "  losing disc {n_total} itself loses no data (it can even be re-created:"
        );
        let _ = writeln!(
            t,
            "    par2 create -s{} -c{} -n1 {}.par2 {part_list} )",
            span.block, span.recovery_blocks, span.source_name
        );
        let _ = writeln!(
            t,
            "  two or more lost discs exceed this parity by design; the protection for"
        );
        let _ = writeln!(t, "  that is the second copy of the set.");
    } else {
        let _ = writeln!(t, "NO PARITY - this set was created with parity disabled.");
        let _ = writeln!(t, "LOSING ANY ONE DISC LOSES THE ENTIRE PAYLOAD.");
        let _ = writeln!(
            t,
            "The only protection is a second, independently burned copy of the set."
        );
    }
    crate::fsutil::write_durable(out, t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PayloadMember;
    use chrono::TimeZone;

    // BD-R 25 with 5% headroom (the spec's worked example) and the
    // single-file overhead pad: 16 MiB + 1 sector.
    const BUDGET: u64 = 23_774_049_076;
    const PAD: u64 = 16 * 1024 * 1024 + 2048;
    const GIB: u64 = 1024 * 1024 * 1024;

    fn part_budget() -> u64 {
        (BUDGET - PAD) * (100 - SPAN_MARGIN_PCT) / 100
    }

    fn file_payload(name: &str, size: u64, container: bool) -> Payload {
        Payload {
            name: name.into(),
            root: name.into(),
            is_dir: false,
            files: vec![PayloadMember {
                abs: name.into(),
                rel: name.into(),
                size,
                container,
            }],
            dirs: 0,
            total_size: size,
        }
    }

    fn dir_payload(name: &str, size: u64) -> Payload {
        Payload {
            name: name.into(),
            root: name.into(),
            is_dir: true,
            files: vec![PayloadMember {
                abs: format!("{name}/a").into(),
                rel: format!("{name}/a"),
                size,
                container: false,
            }],
            dirs: 1,
            total_size: size,
        }
    }

    fn span_of(size: u64, parity: bool, ecc: bool) -> SpanPlan {
        plan_span(
            &[file_payload("vault.hc", size, true)],
            "VAULT_2026Q3",
            BUDGET,
            PAD,
            parity,
            ecc,
        )
        .unwrap()
        .unwrap()
    }

    fn data_parts(span: &SpanPlan) -> Vec<&PartPlan> {
        span.discs.iter().filter_map(|d| d.part.as_ref()).collect()
    }

    fn assert_invariant(span: &SpanPlan) {
        let r_need = data_parts(span)
            .iter()
            .map(|p| p.bytes.div_ceil(span.block))
            .max()
            .unwrap();
        assert!(
            span.recovery_blocks >= (r_need * R_SLACK_NUM).div_ceil(100),
            "R {} < ceil(1.02 * {r_need})",
            span.recovery_blocks
        );
    }

    #[test]
    fn worked_example_matches_the_spec_numbers() {
        // 60 GiB vault.hc on BD-R 25, headroom 5%, ecc on -> 3 data + 1 parity
        let span = span_of(60 * GIB, true, true);
        assert_eq!(span.base_label, "VAULT_2026Q3");
        assert_eq!(span.source_name, "vault.hc");
        assert_eq!(span.source_bytes, 64_424_509_440);
        assert_eq!(span.discs.len(), 4);
        let labels: Vec<&str> = span.discs.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "VAULT_2026Q3_1OF4",
                "VAULT_2026Q3_2OF4",
                "VAULT_2026Q3_3OF4",
                "VAULT_2026Q3_4OF4",
            ]
        );
        let parts = data_parts(&span);
        assert_eq!(parts.len(), 3);
        for (i, p) in parts.iter().enumerate() {
            assert_eq!(p.file_name, format!("vault.hc.{:03}", i + 1));
            assert_eq!(p.bytes, 21_474_836_480);
            assert_eq!(p.offset, i as u64 * 21_474_836_480);
        }
        let parity = span.discs.last().unwrap();
        assert_eq!(parity.role, DiscRole::Parity);
        assert_eq!(parity.index, 4);
        assert!(parity.part.is_none());
        assert_eq!(span.block, plan::slice_bytes_for(60 * GIB, 3));
        assert_eq!(span.block, 1_966_204);
        assert_eq!(span.recovery_blocks, 11_357);
        assert_invariant(&span);
        // ecc on: RS02 fills every ISO to the disc budget
        assert_eq!(span.per_disc_iso_est, BUDGET);
        assert_eq!(
            span.staging_peak,
            60 * GIB + 11_357 * 1_966_204 + 4 * BUDGET
        );
    }

    #[test]
    fn hundred_gib_spans_five_plus_one() {
        let span = span_of(100 * GIB, true, true);
        assert_eq!(span.discs.len(), 6);
        assert_eq!(data_parts(&span).len(), 5);
        assert_eq!(span.block, 3_277_204);
        assert_eq!(span.recovery_blocks, 6_814);
        assert_invariant(&span);
    }

    #[test]
    fn minimum_span_is_two_plus_one() {
        let span = span_of(24 * GIB, true, true);
        assert_eq!(span.discs.len(), 3);
        let parts = data_parts(&span);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts.iter().map(|p| p.bytes).sum::<u64>(), 24 * GIB);
        assert_invariant(&span);
    }

    #[test]
    fn parts_are_balanced_aligned_and_cover_the_source() {
        let s = 60 * GIB + 12_345; // deliberately not sector-aligned
        let span = span_of(s, true, true);
        let parts = data_parts(&span);
        assert_eq!(parts.iter().map(|p| p.bytes).sum::<u64>(), s);
        let mut expected = 0u64;
        for p in &parts[..parts.len() - 1] {
            assert_eq!(p.bytes % plan::SECTOR, 0, "{}", p.file_name);
            assert_eq!(p.offset, expected);
            expected += p.bytes;
        }
        let last = parts.last().unwrap();
        assert_eq!(last.offset, expected);
        assert!(
            last.bytes <= parts[0].bytes,
            "last part must not be the biggest"
        );
    }

    #[test]
    fn bump_loop_converges_when_parts_would_kiss_the_rim() {
        // S = exactly 3*D: the sector round-up pushes parts past the margin
        // line and R_need past the slack - the plan must move to 4 data discs
        let span = span_of(3 * part_budget(), true, true);
        assert_eq!(data_parts(&span).len(), 4);
        assert_eq!(span.discs.len(), 5);
        assert_invariant(&span);
    }

    #[test]
    fn refuses_sets_beyond_ten_discs() {
        let err = plan_span(
            &[file_payload("vault.hc", 200 * GIB, true)],
            "V",
            BUDGET,
            PAD,
            true,
            true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("ceiling is 10"), "{err}");
        // without the parity disc the same payload squeaks under the ceiling
        let span = plan_span(
            &[file_payload("vault.hc", 200 * GIB, true)],
            "V",
            BUDGET,
            PAD,
            false,
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(span.discs.len(), 10);
    }

    #[test]
    fn not_applicable_cases_return_none() {
        // fits one disc
        assert!(span_opt(&[file_payload("v.hc", 20 * GIB, true)]).is_none());
        // directory payload
        assert!(span_opt(&[dir_payload("extras", 60 * GIB)]).is_none());
        // multiple payloads
        assert!(span_opt(&[
            file_payload("a.hc", 60 * GIB, true),
            file_payload("b.hc", 60 * GIB, true),
        ])
        .is_none());
    }

    fn span_opt(payloads: &[Payload]) -> Option<SpanPlan> {
        plan_span(payloads, "V", BUDGET, PAD, true, true).unwrap()
    }

    #[test]
    fn parity_off_drops_the_parity_disc() {
        let span = span_of(60 * GIB, false, true);
        assert_eq!(span.discs.len(), 3);
        assert!(span.discs.iter().all(|d| d.role == DiscRole::Data));
        assert_eq!(span.recovery_blocks, 0);
        assert_eq!(span.discs[0].label, "VAULT_2026Q3_1OF3");
        assert_eq!(span.staging_peak, 60 * GIB + 3 * BUDGET);
    }

    #[test]
    fn dm_formatted_budget_changes_the_split() {
        // 65 GB: 3 data discs on a full BD-R 25 budget, 4 on the smaller
        // DM-formatted capacity (spare areas eat ~800 MB)
        let payloads = [file_payload("vault.hc", 65_000_000_000, true)];
        let free = plan_span(&payloads, "V", BUDGET, PAD, true, true)
            .unwrap()
            .unwrap();
        assert_eq!(data_parts(&free).len(), 3);
        let dm_budget = 24_220_008_448 - 24_220_008_448 * 5 / 100;
        let dm = plan_span(&payloads, "V", dm_budget, PAD, true, true)
            .unwrap()
            .unwrap();
        assert_eq!(data_parts(&dm).len(), 4);
        assert_invariant(&dm);
    }

    #[test]
    fn rs02_stays_viable_on_the_worst_data_disc_and_the_parity_disc() {
        // the 6% margin exists so ecc::augment_target never declines a disc
        // of the set - assert it, per disc role, on both worked examples
        for s in [60 * GIB, 100 * GIB] {
            let span = span_of(s, true, true);
            let parts = data_parts(&span);
            let worst_part = parts.iter().map(|p| p.bytes).max().unwrap();
            let total_blocks: u64 = parts.iter().map(|p| p.bytes.div_ceil(span.block)).sum();
            let idx = index_est(total_blocks, parts.len() as u64);
            let huge_staging = 1u64 << 40;
            let worst_data_iso = worst_part + PAD;
            assert!(
                crate::ecc::augment_target(worst_data_iso, BUDGET, huge_staging).is_some(),
                "data disc of {s} bytes set must get an RS02 layer"
            );
            let parity_iso = span.recovery_blocks * span.block + idx + PAD;
            assert!(
                crate::ecc::augment_target(parity_iso, BUDGET, huge_staging).is_some(),
                "parity disc of {s} bytes set must get an RS02 layer"
            );
        }
    }

    #[test]
    fn per_disc_iso_est_without_ecc_is_content_plus_overhead() {
        let span = span_of(60 * GIB, true, false);
        let parts = data_parts(&span);
        let total_blocks: u64 = parts.iter().map(|p| p.bytes.div_ceil(span.block)).sum();
        let parity_content =
            span.recovery_blocks * span.block + index_est(total_blocks, parts.len() as u64);
        let content_max = parts[0].bytes.max(parity_content);
        assert_eq!(span.per_disc_iso_est, content_max + PAD);
        assert!(span.per_disc_iso_est < BUDGET);
    }

    #[test]
    fn plan_span_properties_hold_over_random_sizes() {
        let d = part_budget();
        let mut seed = 0x243F_6A88_85A3_08D3u64;
        let mut rng = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            seed
        };
        let mut planned = 0u32;
        for _ in 0..300 {
            let s = d + 1 + rng() % (d * 9);
            let payloads = [file_payload("vault.hc", s, true)];
            match plan_span(&payloads, "VAULT_2026Q3", BUDGET, PAD, true, true) {
                Err(_) => assert!(
                    s.div_ceil(d) >= 9,
                    "refusal below the disc ceiling for {s} bytes"
                ),
                Ok(None) => panic!("{s} > D must span"),
                Ok(Some(span)) => {
                    let parts = data_parts(&span);
                    assert_eq!(parts.iter().map(|p| p.bytes).sum::<u64>(), s);
                    let mut expected = 0u64;
                    for p in &parts {
                        assert_eq!(p.offset, expected, "{s}: gap at {}", p.file_name);
                        assert!(p.bytes <= d, "{s}: part {} exceeds D", p.file_name);
                        expected += p.bytes;
                    }
                    assert!(span.discs.len() <= MAX_SET_DISCS as usize);
                    assert!(span.discs.iter().all(|di| di.label.len() <= 32));
                    assert_invariant(&span);
                    planned += 1;
                }
            }
        }
        assert!(planned > 200, "only {planned}/300 sizes planned");
    }

    #[test]
    fn disc_label_pads_and_truncates() {
        assert_eq!(disc_label("VAULT_2026Q3", 1, 4), "VAULT_2026Q3_1OF4");
        assert_eq!(disc_label("VAULT_2026Q3", 4, 4), "VAULT_2026Q3_4OF4");
        // width follows n's digit count and zero-pads k
        assert_eq!(disc_label("V", 1, 10), "V_01OF10");
        assert_eq!(disc_label("V", 10, 10), "V_10OF10");
        // long bases truncate so the whole label stays within 32 chars
        let long = "A".repeat(40);
        let l1 = disc_label(&long, 1, 4);
        assert_eq!(l1.len(), 32);
        assert!(l1.ends_with("_1OF4"));
        let l10 = disc_label(&long, 2, 10);
        assert_eq!(l10.len(), 32);
        assert!(l10.ends_with("_02OF10"));
        // charset is the sanitized base plus [_0-9A-Z] suffix
        assert!(l10
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'));
    }

    #[test]
    fn part_names_are_one_based_fixed_width() {
        assert_eq!(part_file_name("vault.hc", 1), "vault.hc.001");
        assert_eq!(part_file_name("vault.hc", 10), "vault.hc.010");
    }

    fn plan_parts(sizes: &[u64], source: &str) -> Vec<PartPlan> {
        let mut offset = 0u64;
        sizes
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let p = PartPlan {
                    file_name: part_file_name(source, i as u32 + 1),
                    offset,
                    bytes: *b,
                };
                offset += b;
                p
            })
            .collect()
    }

    #[test]
    fn split_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("vault.hc");
        let bytes: Vec<u8> = (0..10 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&source, &bytes).unwrap();
        let set_dir = dir.path().join("set");
        let m = 4 * 1024 * 1024u64;
        let parts = plan_parts(&[m, m, 2 * 1024 * 1024], "vault.hc");

        let mut calls = Vec::new();
        let set = split(&source, &parts, &set_dir, &mut |d, t| calls.push((d, t))).unwrap();

        let total = bytes.len() as u64;
        assert_eq!(calls.first().copied(), Some((0, total)));
        assert_eq!(calls.last().copied(), Some((total, total)));
        assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0));

        let mut reassembled = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            let on_disk = std::fs::read(set_dir.join(&p.file_name)).unwrap();
            let expected = &bytes[p.offset as usize..(p.offset + p.bytes) as usize];
            assert_eq!(on_disk, expected, "chunk {i} differs");
            let independent =
                crate::hashing::sha256_file(&set_dir.join(&p.file_name), &mut |_, _| {}).unwrap();
            assert_eq!(set.part_shas[i], (independent, p.file_name.clone()));
            reassembled.extend_from_slice(&on_disk);
        }
        assert_eq!(
            reassembled, bytes,
            "cat of the parts must restore the source"
        );
        let whole = crate::hashing::sha256_file(&source, &mut |_, _| {}).unwrap();
        assert_eq!(set.whole_sha, whole);
        // parts must never read as VeraCrypt containers (extension "001"),
        // which keeps per-part veracrypt lines out of RECOVERY.txt for free
        let (p, _) = Payload::inspect(set_dir.join("vault.hc.001")).unwrap();
        assert!(!p.looks_like_container());
    }

    #[test]
    fn split_rejects_source_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("v");
        std::fs::write(&source, vec![0u8; 100]).unwrap();
        let parts = plan_parts(&[64, 64], "v");
        let err = split(&source, &parts, &dir.path().join("set"), &mut |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("source changed"), "{err}");
    }

    #[test]
    fn split_rejects_non_contiguous_parts() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("v");
        std::fs::write(&source, vec![0u8; 128]).unwrap();
        let mut parts = plan_parts(&[64, 64], "v");
        parts[1].offset = 32;
        let err = split(&source, &parts, &dir.path().join("set"), &mut |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("not contiguous"), "{err}");
    }

    const WHOLE_SHA: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    fn golden_inputs() -> (SpanPlan, SpanSet, Vec<(String, String)>) {
        let span = span_of(60 * GIB, true, true);
        let set = SpanSet {
            part_shas: vec![
                ("aa".repeat(32), "vault.hc.001".into()),
                ("bb".repeat(32), "vault.hc.002".into()),
                ("cc".repeat(32), "vault.hc.003".into()),
            ],
            whole_sha: WHOLE_SHA.into(),
        };
        let parity_shas = vec![
            ("dd".repeat(32), "parity/vault.hc.par2".into()),
            ("ee".repeat(32), "parity/vault.hc.vol000+11357.par2".into()),
        ];
        (span, set, parity_shas)
    }

    fn created() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 8, 5, 14, 0, 0).unwrap()
    }

    #[test]
    fn set_txt_golden_layout() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("SET.txt");
        let (span, set, parity_shas) = golden_inputs();
        write_set_txt(
            &out,
            &span,
            &set,
            &parity_shas,
            "BD-R 25 GB (single layer)",
            created(),
        )
        .unwrap();
        let text = std::fs::read_to_string(&out).unwrap();

        assert!(text.starts_with("ARCHIVE SET  VAULT_2026Q3\n"), "{text}");
        assert!(text.contains("set id: VAULT_2026Q3-20260805T140000Z\n"));
        assert!(text.contains("created: 2026-08-05 14:00:00 UTC\n"));
        assert!(text.contains("discs: 4 total = 3 data + 1 parity (BD-R 25 GB (single layer))\n"));
        assert!(text.contains("\n  1/4  VAULT_2026Q3_1OF4  data    vault.hc.001\n"));
        assert!(text.contains("\n  4/4  VAULT_2026Q3_4OF4  parity  par2 recovery volumes\n"));
        assert!(text.contains("  bytes:  64424509440 (60.00 GiB)\n"));
        assert!(text.contains(&format!("  sha256: {WHOLE_SHA}\n")));
        assert!(
            text.contains("    vault.hc.001  21474836480 bytes  offset 0            disc 1\n"),
            "{text}"
        );
        assert!(
            text.contains("    vault.hc.002  21474836480 bytes  offset 21474836480  disc 2\n"),
            "{text}"
        );
        // the cat line lists every part, in order
        assert!(
            text.contains("\n  cat vault.hc.001 vault.hc.002 vault.hc.003 > vault.hc\n"),
            "{text}"
        );
        assert!(text.contains(&format!(
            "  printf '{WHOLE_SHA}  vault.hc\\n' | sha256sum -c\n"
        )));
        // CHECKSUMS block is sha256sum-consumable, parts first then parity
        let block: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.starts_with("CHECKSUMS"))
            .skip(1)
            .take_while(|l| !l.is_empty())
            .collect();
        let entries = crate::hashing::parse_checksums(&block.join("\n")).unwrap();
        let rels: Vec<&str> = entries.iter().map(|(_, r)| r.as_str()).collect();
        assert_eq!(
            rels,
            vec![
                "vault.hc.001",
                "vault.hc.002",
                "vault.hc.003",
                "parity/vault.hc.par2",
                "parity/vault.hc.vol000+11357.par2",
            ]
        );
        // the exact repair command, and the parity-disc re-creation command
        assert!(text.contains("\n      par2 r vault.hc.par2\n"), "{text}");
        assert!(
            text.contains(
                "par2 create -s1966204 -c11357 -n1 vault.hc.par2 \
                 vault.hc.001 vault.hc.002 vault.hc.003 )"
            ),
            "{text}"
        );
        assert!(
            text.contains("block 1966204 bytes, 32768 source-block ceiling, 11357 recovery blocks")
        );
        assert!(
            !text.contains("ovenmitts"),
            "on-disc files carry no tool branding: {text}"
        );
    }

    #[test]
    fn set_txt_is_byte_identical_across_writes() {
        let dir = tempfile::tempdir().unwrap();
        let (span, set, parity_shas) = golden_inputs();
        let media = "BD-R 25 GB (single layer)";
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        write_set_txt(&a, &span, &set, &parity_shas, media, created()).unwrap();
        write_set_txt(&b, &span, &set, &parity_shas, media, created()).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    }

    #[test]
    fn set_txt_without_parity_warns_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("SET.txt");
        let span = span_of(60 * GIB, false, true);
        let set = SpanSet {
            part_shas: vec![
                ("aa".repeat(32), "vault.hc.001".into()),
                ("bb".repeat(32), "vault.hc.002".into()),
                ("cc".repeat(32), "vault.hc.003".into()),
            ],
            whole_sha: WHOLE_SHA.into(),
        };
        write_set_txt(
            &out,
            &span,
            &set,
            &[],
            "BD-R 25 GB (single layer)",
            created(),
        )
        .unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("discs: 3 total = 3 data + 0 parity"));
        assert!(text.contains("NO PARITY - this set was created with parity disabled.\n"));
        assert!(text.contains("LOSING ANY ONE DISC LOSES THE ENTIRE PAYLOAD.\n"));
        assert!(text
            .contains("The only protection is a second, independently burned copy of the set.\n"));
        assert!(!text.contains("PARITY\n"), "{text}");
        assert!(!text.contains("par2 r"), "{text}");
        assert!(!text.contains("ovenmitts"));
    }

    #[test]
    fn span_note_derives_part_names() {
        let note = SpanNote {
            disc: 2,
            of: 4,
            role: DiscRole::Data,
            source_name: "vault.hc".into(),
            source_bytes: 64_424_509_440,
            source_sha256: WHOLE_SHA.into(),
            source_container: true,
            recovery_blocks: 11_357,
            block: 1_966_204,
        };
        assert_eq!(note.data_parts(), 3);
        assert_eq!(
            note.part_names(),
            vec!["vault.hc.001", "vault.hc.002", "vault.hc.003"]
        );
        let bare = SpanNote {
            recovery_blocks: 0,
            of: 3,
            ..note
        };
        assert_eq!(bare.data_parts(), 3);
    }
}
