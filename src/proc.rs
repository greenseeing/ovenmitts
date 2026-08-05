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

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Short one-shot tool invocations get this hard deadline; a hung probe or
/// mediainfo must not hang ovenmitts forever. Streaming burns use the
/// configurable inactivity watchdog instead (they legitimately run for an hour).
pub(crate) const SHORT_OP_DEADLINE: Duration = Duration::from_secs(120);

/// A `Command` for `bin` with a C locale and no stdin. Callers add args and
/// choose how to capture output (`.output()`, `.status()`, piped streaming).
pub(crate) fn command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    // LC_ALL overrides LANG and every LC_* category, so one variable is enough.
    cmd.env("LC_ALL", "C").stdin(Stdio::null());
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

/// Signal every registered child (force-quit path); best effort.
pub fn terminate_active(force: bool) {
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    if let Ok(v) = ACTIVE_CHILDREN.lock() {
        for pid in v.iter() {
            unsafe { libc::kill(*pid, sig) };
        }
    }
}

// ETXTBSY at exec is a transient fork/exec race (a concurrent fork briefly
// inherits a write fd to the binary); it also fires when we exec a binary that
// another process — e.g. an in-progress `ovenmitts update` — still holds open
// for write. Retry briefly rather than fail the whole operation.
pub(crate) fn spawn_retrying(bin: &Path, args: &[String]) -> Result<Child> {
    let mut tries = 0;
    loop {
        match command(bin)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e).with_context(|| format!("spawn {}", bin.display())),
        }
    }
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

    /// Terminate the child now (SIGTERM, brief grace, SIGKILL) and reap it.
    /// Used by the inactivity watchdog.
    pub(crate) fn kill_now(&mut self) {
        if self.reaped {
            return;
        }
        unsafe { libc::kill(self.pid, libc::SIGTERM) };
        for _ in 0..200 {
            if matches!(self.reap_if_exited(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
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
