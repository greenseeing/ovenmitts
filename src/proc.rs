//! Shared process-execution layer. Every external tool is spawned through
//! [`command`] so its output is locale-independent: `LC_ALL=C` forces `.`
//! decimal separators and English keywords, without which the progress and
//! capacity parsers (`38.2%` vs `38,2%`, `23.3g`, `MB written`) silently
//! misread under a non-C locale — and a misread capacity feeds the fit gate.
//!
//! It also owns child-process lifetime: [`Reaper`] guarantees a spawned child
//! is killed and reaped even on an error or a panic (so a burn is never
//! orphaned), and [`terminate_active`] lets an interactive force-quit signal
//! every live tool.

use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Short one-shot tool invocations get this hard deadline; a hung probe or
/// mediainfo must not hang ovenmitts forever. Streaming burns use the
/// configurable inactivity watchdog instead (they legitimately run for an hour).
pub(crate) const SHORT_OP_DEADLINE: Duration = Duration::from_secs(120);

/// A `Command` for `bin` with a C locale, no stdin, and its own process
/// group. Callers add args and choose how to capture output.
///
/// The dedicated group means kill(-pid) takes any grandchildren a tool forks
/// along with it, so a watchdog kill or force-quit can never orphan one that
/// would keep the output pipes open. It also detaches children from the
/// terminal's foreground group - deliberate: signal delivery to tools is
/// ovenmitts's job (shutdown::escalate), never the terminal driver's.
pub(crate) fn command(bin: &Path) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(bin);
    // LC_ALL overrides LANG and every LC_* category, so one variable is enough.
    cmd.env("LC_ALL", "C").stdin(Stdio::null()).process_group(0);
    cmd
}

// Live child PIDs of external tools, so an interactive force-quit can terminate
// them instead of orphaning a burn in progress.
static ACTIVE_CHILDREN: Mutex<Vec<i32>> = Mutex::new(Vec::new());

fn register(pid: i32) {
    if let Ok(mut v) = ACTIVE_CHILDREN.lock() {
        v.push(pid);
    }
}

fn deregister(pid: i32) {
    if let Ok(mut v) = ACTIVE_CHILDREN.lock() {
        if let Some(i) = v.iter().position(|p| *p == pid) {
            v.swap_remove(i);
        }
    }
}

/// Signal every registered child's process group (force-quit path); best
/// effort. Children are group leaders (command() uses process_group(0)), so
/// -pid addresses the tool and any grandchildren it forked.
pub fn terminate_active(force: bool) {
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    if let Ok(v) = ACTIVE_CHILDREN.lock() {
        for pid in v.iter() {
            unsafe { libc::kill(-*pid, sig) };
        }
    }
}

// ETXTBSY at exec is a transient fork/exec race (a concurrent fork briefly
// inherits a write fd to the binary); it also fires when we exec a binary that
// another process — e.g. an in-progress `ovenmitts update` — still holds open
// for write. Retry briefly rather than fail the whole operation.
fn spawn_command_retrying(cmd: &mut Command, bin: &Path) -> Result<Child> {
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e).with_context(|| format!("spawn {}", bin.display())),
        }
    }
}

pub(crate) fn spawn_retrying(bin: &Path, args: &[String]) -> Result<Child> {
    let mut cmd = command(bin);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    spawn_command_retrying(&mut cmd, bin)
}

/// Owns a spawned child and guarantees it is reaped. `wait` deregisters the PID
/// *before* blocking (a PID cannot be recycled until it is reaped, so
/// unregister-then-wait closes the window where a force-quit could SIGKILL a
/// recycled PID). On drop of a still-running child — an error path or an unwind
/// — it SIGTERMs, waits briefly, SIGKILLs, and reaps, so no tool is orphaned.
pub(crate) struct Reaper {
    child: Child,
    pid: i32,
    reaped: bool,
}

impl Reaper {
    pub(crate) fn spawn(bin: &Path, args: &[String]) -> Result<Self> {
        Ok(Self::adopt(spawn_retrying(bin, args)?))
    }

    pub(crate) fn adopt(child: Child) -> Self {
        let pid = child.id() as i32;
        register(pid);
        Self {
            child,
            pid,
            reaped: false,
        }
    }

    pub(crate) fn stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    pub(crate) fn stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.stderr.take()
    }

    /// Terminate the child's process group now (SIGTERM, brief grace,
    /// SIGKILL) and reap the child. Used by the inactivity watchdog. Group
    /// signaling means a grandchild holding the output pipes dies too -
    /// otherwise the pump threads would block on the open pipe forever.
    pub(crate) fn kill_now(&mut self) {
        if self.reaped {
            return;
        }
        unsafe { libc::kill(-self.pid, libc::SIGTERM) };
        for _ in 0..200 {
            if matches!(self.reap_if_exited(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe { libc::kill(-self.pid, libc::SIGKILL) };
        // SIGKILL cannot be ignored: deregister-then-wait keeps the invariant.
        deregister(self.pid);
        self.reaped = true;
        let _ = self.child.wait();
    }

    pub(crate) fn wait(&mut self) -> Result<std::process::ExitStatus> {
        // Deregister before blocking: the PID can't be recycled until reaped.
        deregister(self.pid);
        self.reaped = true;
        self.child.wait().context("wait for child")
    }

    /// Nonblocking exit check that keeps the deregister-before-reap invariant:
    /// a WNOWAIT peek leaves the child a zombie (its PID cannot be recycled),
    /// so the PID is deregistered before the actual reap ever frees it. A
    /// plain try_wait would reap first and leave a recyclable PID registered
    /// for a moment - the exact window wait() documents as closed.
    pub(crate) fn reap_if_exited(&mut self) -> Result<Option<std::process::ExitStatus>> {
        if self.reaped {
            bail!("child already reaped");
        }
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("waitid");
        }
        // WNOHANG with no state change leaves si_pid untouched (zeroed).
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        deregister(self.pid);
        self.reaped = true;
        self.child.wait().map(Some).context("wait for child")
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        self.kill_now();
    }
}

// Poll cadence for the inactivity watchdog, and how long a tool may be silent
// before we surface a reassuring "still working" note (not a kill).
const WATCH_POLL: Duration = Duration::from_secs(5);
const WARN_AFTER: Duration = Duration::from_secs(120);

/// The one streaming implementation: spawn `bin`, split both pipes on \r and
/// \n (libburn and par2 rewrite progress in place with '\r'), and feed every
/// line to `on_line` with an is-stderr flag. `stall` kills the child after
/// that long with no output at all (Duration::ZERO disables) — healthy tools
/// emit keepalives every second, so only a genuinely wedged drive stays
/// silent that long. Returns the exit status; success policy is the caller's.
pub(crate) fn stream_lines(
    bin: &Path,
    args: &[String],
    current_dir: Option<&Path>,
    stall: Duration,
    on_line: &mut dyn FnMut(bool, &str),
) -> Result<ExitStatus> {
    let mut cmd = command(bin);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = current_dir {
        cmd.current_dir(dir);
    }
    let mut reaper = Reaper::adopt(spawn_command_retrying(&mut cmd, bin)?);
    let stdout = reaper.stdout().context("no stdout pipe")?;
    let stderr = reaper.stderr().context("no stderr pipe")?;
    let (tx, rx) = mpsc::channel::<(bool, String)>();
    let tx_err = tx.clone();
    let t_out = std::thread::spawn(move || pump(stdout, false, tx));
    let t_err = std::thread::spawn(move || pump(stderr, true, tx_err));

    let mut last_activity = Instant::now();
    let mut last_warn = Instant::now();
    let mut stalled = false;
    loop {
        match rx.recv_timeout(WATCH_POLL) {
            Ok((is_err, line)) => {
                last_activity = Instant::now();
                on_line(is_err, &line);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let quiet = last_activity.elapsed();
                if quiet >= WARN_AFTER && last_warn.elapsed() >= Duration::from_secs(60) {
                    on_line(false, &format!("(no tool output for {}s)", quiet.as_secs()));
                    last_warn = Instant::now();
                }
                if !stall.is_zero() && quiet >= stall {
                    reaper.kill_now();
                    stalled = true;
                    break;
                }
            }
        }
    }
    let _ = t_out.join();
    let _ = t_err.join();
    if stalled {
        bail!(
            "{}: no output for {}s - terminated (wedged drive?)",
            bin.display(),
            stall.as_secs()
        );
    }
    reaper.wait()
}

const STDERR_TAIL: usize = 12;

// xorriso keeps emitting UPDATE keepalives to stderr while aborting; without
// severity filtering they bury (or evict) the FATAL/FAILURE cause.
fn is_diagnostic(line: &str) -> bool {
    [
        " FATAL : ",
        " FAILURE : ",
        " SORRY : ",
        " ABORT : ",
        " : aborting :",
    ]
    .iter()
    .any(|needle| line.contains(needle))
}

fn push_capped(buf: &mut VecDeque<String>, line: String) {
    if buf.len() == STDERR_TAIL {
        buf.pop_front();
    }
    buf.push_back(line);
}

/// stream_lines with the common success policy: nonzero exit fails with the
/// diagnostic stderr lines (or the raw tail when no severity markers appear).
pub(crate) fn run_streaming(
    bin: &Path,
    args: &[String],
    stall: Duration,
    on_line: &mut dyn FnMut(&str),
) -> Result<()> {
    let mut tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL);
    let mut diags: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL);
    let status = stream_lines(bin, args, None, stall, &mut |is_err, line| {
        if is_err {
            if is_diagnostic(line) {
                push_capped(&mut diags, line.to_string());
            }
            push_capped(&mut tail, line.to_string());
        }
        on_line(line);
    })?;
    if !status.success() {
        let lines: Vec<String> = if diags.is_empty() { tail } else { diags }.into();
        bail!("{} failed ({status}): {}", bin.display(), lines.join("\n"));
    }
    Ok(())
}

fn pump(mut r: impl Read, is_err: bool, tx: mpsc::Sender<(bool, String)>) {
    let mut buf = [0u8; 8192];
    let mut acc: Vec<u8> = Vec::new();
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                if !acc.is_empty() {
                    let _ = tx.send((is_err, String::from_utf8_lossy(&acc).into_owned()));
                    acc.clear();
                }
            } else {
                acc.push(b);
            }
        }
    }
    if !acc.is_empty() {
        let _ = tx.send((is_err, String::from_utf8_lossy(&acc).into_owned()));
    }
}

/// Run a short command to completion with a hard deadline, capturing all
/// output. On timeout the child is killed and an error returned, so a hung
/// probe/mediainfo/veracrypt can never hang ovenmitts.
pub(crate) fn output_deadline(bin: &Path, args: &[String], deadline: Duration) -> Result<Output> {
    let mut reaper = Reaper::spawn(bin, args)?;
    let stdout = reaper.stdout().context("no stdout pipe")?;
    let stderr = reaper.stderr().context("no stderr pipe")?;
    let out_h = std::thread::spawn(move || read_all(stdout));
    let err_h = std::thread::spawn(move || read_all(stderr));

    let start = Instant::now();
    loop {
        match reaper.reap_if_exited() {
            Ok(Some(status)) => {
                let stdout = out_h.join().unwrap_or_default();
                let stderr = err_h.join().unwrap_or_default();
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if start.elapsed() >= deadline {
                    reaper.kill_now();
                    bail!("{} timed out after {}s", bin.display(), deadline.as_secs());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e).with_context(|| format!("wait for {}", bin.display())),
        }
    }
}

fn read_all(mut r: impl Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ACTIVE_CHILDREN is process-global, and terminate_active signals every
    // registered child; serialize the tests that spawn/register or env-mutate
    // so one test can't kill another's child or race LC_ALL.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn command_forces_c_locale() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // A child that echoes its own LC_ALL must see "C" regardless of the
        // caller's environment.
        std::env::set_var("LC_ALL", "de_DE.UTF-8");
        let out = command(Path::new("/bin/sh"))
            .arg("-c")
            .arg("printf %s \"$LC_ALL\"")
            .output()
            .unwrap();
        std::env::remove_var("LC_ALL");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "C");
    }

    #[test]
    fn output_deadline_captures_output() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let out = output_deadline(
            Path::new("/bin/sh"),
            &["-c".into(), "printf hi; printf err >&2".into()],
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "err");
    }

    #[test]
    fn output_deadline_kills_a_hung_child() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let start = Instant::now();
        // sleep well past any plausible scheduling delay: returning at all
        // proves the deadline killed it rather than waiting it out.
        let err = output_deadline(
            Path::new("/bin/sh"),
            &["-c".into(), "exec sleep 120".into()],
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "deadline did not kill the child"
        );
    }

    #[test]
    fn reaper_kills_on_drop() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let reaper =
            Reaper::spawn(Path::new("/bin/sh"), &["-c".into(), "sleep 30".into()]).unwrap();
        let pid = reaper.pid;
        drop(reaper);
        // The PID must be dead (kill(pid, 0) fails) and deregistered.
        std::thread::sleep(Duration::from_millis(50));
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "child pid {pid} still alive after Reaper drop");
    }
    // terminate_active is intentionally not unit-tested in isolation: it signals
    // every process-global registered child, so a dedicated test would race with
    // children spawned by concurrent tests in other modules. The kill primitive
    // is covered by reaper_kills_on_drop and the shutdown flow by shutdown::tests.
}
