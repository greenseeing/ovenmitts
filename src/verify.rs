use std::io::{BufRead, BufReader, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::tools::Tools;

const SECTOR: u64 = 2048;
const CHUNK: usize = 1 << 20;
const POLL: Duration = Duration::from_secs(2);

/// Reload the tray (`eject -t` when available) and poll the device until the
/// medium is readable or timeout. Clamshell drives cannot auto-reload: after
/// half the timeout, emit a "please reinsert" hint via cb and keep polling.
pub fn wait_medium_ready(
    tools: &Tools,
    device: &str,
    timeout: Duration,
    cb: &mut dyn FnMut(String),
) -> Result<()> {
    if let Some(eject_bin) = &tools.eject {
        let _ = Command::new(eject_bin).arg("-t").arg(device).output();
    }
    let start = Instant::now();
    let mut hinted = false;
    loop {
        if medium_readable(device) {
            return Ok(());
        }
        let elapsed = start.elapsed();
        if !hinted && elapsed >= timeout / 2 {
            cb(format!(
                "drive still not ready - clamshell/slot drives cannot reload \
                 themselves, please reinsert the disc in {device}"
            ));
            hinted = true;
        }
        if elapsed >= timeout {
            bail!("{device}: no readable medium after {}s", timeout.as_secs());
        }
        std::thread::sleep(POLL);
    }
}

fn medium_readable(device: &str) -> bool {
    let mut sector = [0u8; SECTOR as usize];
    std::fs::File::open(device)
        .and_then(|mut f| f.read_exact(&mut sector))
        .is_ok()
}

/// Eject the tray (best effort; used before reload to defeat the page cache).
pub fn eject(tools: &Tools, device: &str) -> Result<()> {
    let Some(bin) = &tools.eject else {
        bail!("'eject' not found - needed for cache-proof verification (install util-linux)");
    };
    let out = Command::new(bin)
        .arg(device)
        .output()
        .with_context(|| format!("running eject {device}"))?;
    if !out.status.success() {
        bail!(
            "eject {device} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub struct Readback {
    pub sha256: String,
    pub o_direct: bool,
}

/// Read exactly `bytes` from the device and SHA-256 them. Opens O_DIRECT with
/// a 4096-aligned buffer to bypass the page cache; reads whole 2048-byte
/// sectors but hashes only the first `bytes` bytes (run-out stays out).
/// On any O_DIRECT failure falls back to buffered reads with o_direct=false
/// so the caller can warn - buffered reads may be served from the page cache
/// unless an eject/reload cycle preceded them.
pub fn readback_hash(device: &str, bytes: u64, cb: &mut dyn FnMut(u64, u64)) -> Result<Readback> {
    cb(0, bytes);
    if let Some(sha256) = odirect_hash(device, bytes, cb) {
        return Ok(Readback {
            sha256,
            o_direct: true,
        });
    }
    buffered_hash(device, bytes, cb).map(|sha256| Readback {
        sha256,
        o_direct: false,
    })
}

fn odirect_hash(device: &str, bytes: u64, cb: &mut dyn FnMut(u64, u64)) -> Option<String> {
    let path = std::ffi::CString::new(device).ok()?;
    let raw = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECT | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let buf = AlignedBuf::new(CHUNK, 4096)?;
    let mut hasher = Sha256::new();
    // pread at an explicitly tracked offset: `done` only ever advances by
    // whole sectors (except the final tail), so the device offset stays
    // sector-aligned even across short reads, and EINTR retries are safe.
    let mut done: u64 = 0;
    let mut stalls = 0u32;
    while done < bytes {
        let remaining = bytes - done;
        let padded = remaining.div_ceil(SECTOR) * SECTOR;
        let req = padded.min(CHUNK as u64) as usize;
        let n = unsafe { libc::pread(fd.as_raw_fd(), buf.ptr.cast(), req, done as libc::off_t) };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
        if n == 0 {
            return None;
        }
        let take = if done + n as u64 >= bytes {
            (bytes - done) as usize
        } else {
            (n as u64 / SECTOR * SECTOR) as usize
        };
        if take == 0 {
            stalls += 1;
            if stalls > 8 {
                return None;
            }
            continue;
        }
        stalls = 0;
        hasher.update(unsafe { std::slice::from_raw_parts(buf.ptr, take) });
        done += take as u64;
        cb(done, bytes);
    }
    Some(hex(&hasher.finalize()))
}

fn buffered_hash(device: &str, bytes: u64, cb: &mut dyn FnMut(u64, u64)) -> Result<String> {
    let mut f =
        std::fs::File::open(device).with_context(|| format!("opening {device} for read-back"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    while done < bytes {
        let want = (bytes - done).min(CHUNK as u64) as usize;
        let n = f
            .read(&mut buf[..want])
            .with_context(|| format!("reading {device}"))?;
        if n == 0 {
            bail!("{device}: EOF after {done} of {bytes} bytes");
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        cb(done, bytes);
    }
    Ok(hex(&hasher.finalize()))
}

struct AlignedBuf {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl AlignedBuf {
    fn new(size: usize, align: usize) -> Option<Self> {
        let layout = std::alloc::Layout::from_size_align(size, align).ok()?;
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            return None;
        }
        Some(Self { ptr, layout })
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// If the device is auto-mounted (udisks), unmount it before raw access.
pub fn ensure_unmounted(tools: &Tools, device: &str) -> Result<()> {
    let mounts = std::fs::read_to_string("/proc/mounts").context("reading /proc/mounts")?;
    let canonical = std::fs::canonicalize(device)
        .ok()
        .map(|p| p.display().to_string());
    let hit = proc_mounts_mountpoint(&mounts, device).or_else(|| {
        canonical
            .as_deref()
            .and_then(|d| proc_mounts_mountpoint(&mounts, d))
    });
    let Some(mp) = hit else {
        return Ok(());
    };
    let Some(udisksctl) = &tools.udisksctl else {
        bail!("{device} is mounted at {mp}; unmount it first: sudo umount {device}");
    };
    let out = Command::new(udisksctl)
        .args(["unmount", "-b", device])
        .output()
        .context("running udisksctl unmount")?;
    if !out.status.success() {
        bail!(
            "udisksctl unmount -b {device} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn proc_mounts_mountpoint(mounts: &str, device: &str) -> Option<String> {
    for line in mounts.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(dev), Some(mp)) = (fields.next(), fields.next()) else {
            continue;
        };
        if dev == device {
            return Some(unescape_proc_mounts(mp));
        }
    }
    None
}

fn unescape_proc_mounts(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && (b'0'..=b'3').contains(&b[i + 1])
            && (b'0'..=b'7').contains(&b[i + 2])
            && (b'0'..=b'7').contains(&b[i + 3])
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mount read-only via udisksctl (no root); returns the mountpoint.
/// Errors with the manual `sudo mount -o ro` command when udisksctl is absent.
pub fn mount_ro(tools: &Tools, device: &str) -> Result<PathBuf> {
    let Some(udisksctl) = &tools.udisksctl else {
        bail!("udisksctl not found; mount manually: sudo mount -o ro {device} /mnt");
    };
    let ro = Command::new(udisksctl)
        .args(["mount", "-b", device, "-o", "ro"])
        .output()
        .context("running udisksctl mount")?;
    let out = if ro.status.success() {
        ro
    } else {
        // older udisks without -o; optical media get mounted read-only anyway
        let plain = Command::new(udisksctl)
            .args(["mount", "-b", device])
            .output()
            .context("running udisksctl mount")?;
        if !plain.status.success() {
            bail!(
                "udisksctl mount -b {device} failed: {}",
                String::from_utf8_lossy(&plain.stderr).trim()
            );
        }
        plain
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_udisks_mountpoint(&stdout)
        .with_context(|| format!("unrecognized udisksctl mount output: {}", stdout.trim()))
}

/// Unmount via udisksctl; Err surfaces as a warning at the call sites.
pub fn unmount(tools: &Tools, device: &str) -> Result<()> {
    let Some(udisksctl) = &tools.udisksctl else {
        return Ok(());
    };
    let out = Command::new(udisksctl)
        .args(["unmount", "-b", device])
        .output()
        .with_context(|| format!("spawning {}", udisksctl.display()))?;
    if !out.status.success() {
        bail!(
            "udisksctl unmount -b {device} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `xorriso -md5 on -indev <dev> -check_media --` using the MD5 tags embedded
/// at mastering time; returns true when the medium checks clean.
/// xorriso's -md5 default is off, and check runs verify the session tags only
/// when it is on; it must precede -indev so the tags load with the image.
pub fn check_media(
    tools: &Tools,
    device: &str,
    cb: &mut dyn FnMut(Option<f32>, String),
) -> Result<bool> {
    let mut child = Command::new(&tools.xorriso)
        .args(["-md5", "on", "-indev", device, "-check_media", "--"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning xorriso -check_media")?;
    let _guard = crate::burn::ChildGuard::new(child.id());
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut pumps: Vec<JoinHandle<()>> = Vec::new();
    // result channel (stdout) carries the tables, info channel (stderr) the pacifier
    if let Some(o) = child.stdout.take() {
        pumps.push(spawn_line_pump(o, tx.clone()));
    }
    if let Some(e) = child.stderr.take() {
        pumps.push(spawn_line_pump(e, tx.clone()));
    }
    drop(tx);
    let mut all = String::new();
    for line in rx {
        if line.contains("blocks read") || line.contains("SORRY") || line.contains("FAILURE") {
            cb(None, line.trim().to_string());
        }
        all.push_str(&line);
        all.push('\n');
    }
    for p in pumps {
        let _ = p.join();
    }
    let status = child.wait().context("waiting for xorriso -check_media")?;
    if !status.success() && !all.contains("Media checks :") {
        bail!(
            "xorriso -check_media failed ({status}): {}",
            tail_lines(&all, 6)
        );
    }
    Ok(parse_check_media(&all))
}

fn spawn_line_pump(r: impl Read + Send + 'static, tx: Sender<String>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for line in BufReader::new(r).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    })
}

fn tail_lines(s: &str, n: usize) -> String {
    let mut lines: Vec<&str> = s.lines().rev().take(n).collect();
    lines.reverse();
    lines.join("\n")
}

/// Pure: decide "clean" from xorriso -check_media output (testable).
/// Region qualities: "+" readable, "-" damaged, "0" untested/allowed-unread -
/// except md5_mismatch, which the default bad_limit files under "0".
pub fn parse_check_media(out: &str) -> bool {
    let mut confirmed = false;
    for line in out.lines() {
        let l = line.trim_start();
        let region = l
            .strip_prefix("Media region :")
            .or_else(|| l.strip_prefix("MD5 tag range:"));
        if let Some(rest) = region {
            let Some(quality) = rest.rsplit(',').next().map(str::trim) else {
                continue;
            };
            if quality.starts_with('-') || quality.contains("md5_mismatch") {
                return false;
            }
            if quality.starts_with('+') {
                confirmed = true;
            }
        } else if l.contains("MD5 MISMATCH") || l.contains("Event triggered by media read error") {
            return false;
        }
    }
    confirmed
}

/// Pure: extract mountpoint from `udisksctl mount` stdout.
/// udisks < 2.7 ends the line with a period, newer versions do not.
pub fn parse_udisks_mountpoint(out: &str) -> Option<PathBuf> {
    for line in out.lines() {
        let Some(rest) = line.trim().strip_prefix("Mounted ") else {
            continue;
        };
        let Some(idx) = rest.find(" at ") else {
            continue;
        };
        let mut mp = rest[idx + 4..].trim();
        if let Some(stripped) = mp.strip_suffix('.') {
            mp = stripped;
        }
        if mp.starts_with('/') {
            return Some(PathBuf::from(mp));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_hex(data: &[u8]) -> String {
        hex(&Sha256::digest(data))
    }

    fn tmp_file_with(len: usize, seed: u8) -> (tempfile::TempDir, PathBuf, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..len)
            .map(|i| ((i as u32).wrapping_mul(31).wrapping_add(seed as u32) % 251) as u8)
            .collect();
        let p = dir.path().join("payload.bin");
        std::fs::write(&p, &data).unwrap();
        (dir, p, data)
    }

    fn no_tools() -> Tools {
        Tools {
            xorriso: PathBuf::from("/bin/true"),
            par2: None,
            par2_version: None,
            udisksctl: None,
            veracrypt: None,
            eject: None,
            mediainfo: None,
        }
    }

    #[test]
    fn readback_hashes_exact_non_sector_multiple() {
        let (_d, p, data) = tmp_file_with(5000, 7);
        let mut calls = Vec::new();
        let h = readback_hash(p.to_str().unwrap(), 5000, &mut |d, t| calls.push((d, t)))
            .unwrap()
            .sha256;
        assert_eq!(h, ref_hex(&data));
        assert_eq!(*calls.last().unwrap(), (5000, 5000));
        assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    #[test]
    fn readback_ignores_trailing_runout() {
        let (_d, p, data) = tmp_file_with(8192, 3);
        let h = readback_hash(p.to_str().unwrap(), 5000, &mut |_, _| {})
            .unwrap()
            .sha256;
        assert_eq!(h, ref_hex(&data[..5000]));
    }

    #[test]
    fn readback_sector_multiple() {
        let (_d, p, data) = tmp_file_with(4096, 9);
        let h = readback_hash(p.to_str().unwrap(), 4096, &mut |_, _| {})
            .unwrap()
            .sha256;
        assert_eq!(h, ref_hex(&data));
    }

    #[test]
    fn readback_multi_chunk() {
        let len = (1 << 20) + 12345;
        let (_d, p, data) = tmp_file_with(len, 1);
        let h = readback_hash(p.to_str().unwrap(), len as u64, &mut |_, _| {})
            .unwrap()
            .sha256;
        assert_eq!(h, ref_hex(&data));
    }

    #[test]
    fn readback_zero_bytes() {
        let (_d, p, _) = tmp_file_with(100, 0);
        let h = readback_hash(p.to_str().unwrap(), 0, &mut |_, _| {})
            .unwrap()
            .sha256;
        assert_eq!(h, ref_hex(&[]));
    }

    #[test]
    fn readback_short_source_errors() {
        let (_d, p, _) = tmp_file_with(1000, 2);
        assert!(readback_hash(p.to_str().unwrap(), 4096, &mut |_, _| {}).is_err());
    }

    #[test]
    fn udisks_mountpoint_with_trailing_period() {
        assert_eq!(
            parse_udisks_mountpoint("Mounted /dev/sr0 at /run/media/user/ARCHIVE.\n"),
            Some(PathBuf::from("/run/media/user/ARCHIVE"))
        );
    }

    #[test]
    fn udisks_mountpoint_without_trailing_period() {
        assert_eq!(
            parse_udisks_mountpoint("Mounted /dev/sr0 at /run/media/user/ARCHIVE\n"),
            Some(PathBuf::from("/run/media/user/ARCHIVE"))
        );
    }

    #[test]
    fn udisks_mountpoint_with_spaces() {
        assert_eq!(
            parse_udisks_mountpoint("Mounted /dev/loop43p2 at /media/odeda/Game Install\n"),
            Some(PathBuf::from("/media/odeda/Game Install"))
        );
    }

    #[test]
    fn udisks_mountpoint_rejects_garbage() {
        assert_eq!(parse_udisks_mountpoint(""), None);
        assert_eq!(
            parse_udisks_mountpoint("Error mounting /dev/sr0: GDBus.Error: not authorized\n"),
            None
        );
    }

    #[test]
    fn check_media_clean_fixture() {
        assert!(parse_check_media(include_str!(
            "../tests/fixtures/check_media_clean.txt"
        )));
    }

    #[test]
    fn check_media_damaged_fixture() {
        assert!(!parse_check_media(include_str!(
            "../tests/fixtures/check_media_damaged.txt"
        )));
    }

    #[test]
    fn check_media_md5_clean_fixture() {
        assert!(parse_check_media(include_str!(
            "../tests/fixtures/check_media_md5_clean.txt"
        )));
    }

    #[test]
    fn check_media_md5_mismatch_fixture() {
        assert!(!parse_check_media(include_str!(
            "../tests/fixtures/check_media_md5_mismatch.txt"
        )));
    }

    #[test]
    fn check_media_empty_or_untested_is_not_clean() {
        assert!(!parse_check_media(""));
        assert!(!parse_check_media(
            "Media checks :        lba ,       size , quality\n\
             Media region :          0 ,       1000 , 0 untested\n"
        ));
    }

    const MOUNTS: &str = "\
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
/dev/nvme0n1p2 / ext4 rw,relatime 0 0
/dev/sr0 /run/media/user/MY\\040DISC iso9660 ro,nosuid,nodev,relatime 0 0
";

    #[test]
    fn proc_mounts_finds_device_and_unescapes() {
        assert_eq!(
            proc_mounts_mountpoint(MOUNTS, "/dev/sr0"),
            Some("/run/media/user/MY DISC".to_string())
        );
    }

    #[test]
    fn proc_mounts_misses_absent_device() {
        assert_eq!(proc_mounts_mountpoint(MOUNTS, "/dev/sr1"), None);
    }

    #[test]
    fn medium_ready_immediately_on_readable_source() {
        let (_d, p, _) = tmp_file_with(4096, 4);
        wait_medium_ready(
            &no_tools(),
            p.to_str().unwrap(),
            Duration::from_secs(4),
            &mut |_| {},
        )
        .unwrap();
    }

    #[test]
    fn medium_ready_times_out_with_one_reinsert_hint() {
        let mut hints = Vec::new();
        let res = wait_medium_ready(
            &no_tools(),
            "/definitely/not/a/device",
            Duration::from_millis(10),
            &mut |m| hints.push(m),
        );
        assert!(res.is_err());
        assert_eq!(hints.len(), 1);
    }

    #[test]
    fn eject_errors_without_binary() {
        assert!(eject(&no_tools(), "/dev/sr0").is_err());
    }

    #[test]
    fn mount_ro_errors_without_udisksctl() {
        let err = mount_ro(&no_tools(), "/dev/sr0").unwrap_err();
        assert!(err.to_string().contains("sudo mount -o ro"));
    }
}
