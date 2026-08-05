//! Graceful shutdown on signals. A burn owns a laser writing to archival
//! media; a bare SIGTERM (terminal closed, `systemctl stop`) must not kill
//! ovenmitts and orphan the running xorriso. [`install`] arms a flag the event
//! loops poll, and [`escalate`] terminates every live tool without orphaning
//! it — the same sequence the TUI force-quit uses.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

use crate::runner::Ack;

// Hard-exit is armed only once escalate() has begun: an impatient double
// Ctrl-C before the graceful path even started must not kill ovenmitts
// dead and orphan the burn (children live in their own process groups now,
// so the terminal no longer signals them as a side effect).
fn hard_exit_flag() -> Arc<AtomicBool> {
    static HARD_EXIT: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    HARD_EXIT
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

/// Arm INT/TERM/HUP; the returned flag flips true on the first such signal.
/// A further signal AFTER the graceful escalation has begun restores the
/// default disposition - the operator's hard-exit escape hatch if the
/// escalation itself wedges. SIGQUIT is deliberately left at its default so
/// a core dump stays reachable.
pub fn install() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    for sig in [SIGINT, SIGTERM, SIGHUP] {
        // A failure to register a handler must not abort the whole run; the
        // worst case is the pre-existing behaviour (signal kills the process).
        // Registration order matters: the conditional-default action runs
        // first and only terminates once escalate() armed the hard flag.
        let _ = signal_hook::flag::register_conditional_default(sig, hard_exit_flag());
        let _ = signal_hook::flag::register(sig, Arc::clone(&flag));
    }
    flag
}

pub fn stopping(flag: &Arc<AtomicBool>) -> bool {
    flag.load(Ordering::SeqCst)
}

/// Terminate running tools without orphaning them: tell a runner parked on a
/// prompt to abort, then SIGTERM → 10 s grace → SIGKILL → 3 s every live child.
/// `is_finished` reports whether the worker thread has returned.
pub fn escalate(is_finished: impl Fn() -> bool, ack_tx: &Sender<Ack>) {
    escalate_with(is_finished, ack_tx, crate::proc::terminate_active);
}

// terminate is injected so tests can exercise the ack/grace-period logic
// without the real terminate_active, which signals every process-global child
// (and would kill children spawned by concurrent tests).
fn escalate_with(is_finished: impl Fn() -> bool, ack_tx: &Sender<Ack>, terminate: impl Fn(bool)) {
    // from here on a repeat signal may hard-exit: the graceful path is running
    hard_exit_flag().store(true, Ordering::SeqCst);
    let _ = ack_tx.send(Ack::Abort);
    terminate(false);
    wait_until(&is_finished, Duration::from_secs(10));
    if !is_finished() {
        terminate(true);
        wait_until(&is_finished, Duration::from_secs(3));
    }
}

fn wait_until(is_finished: &impl Fn() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn install_flag_starts_unset() {
        let flag = install();
        assert!(!stopping(&flag));
    }

    // hard_exit_flag is process-global; serialize the tests that touch it.
    static HARD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn hard_exit_arms_only_when_escalation_begins() {
        let _g = HARD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // before escalate(), a repeat signal must NOT hard-kill the process
        // (that would orphan the burn); afterwards it is the escape hatch
        hard_exit_flag().store(false, Ordering::SeqCst);
        assert!(!hard_exit_flag().load(Ordering::SeqCst));
        let (tx, _rx) = mpsc::channel();
        escalate_with(|| true, &tx, |_| {});
        assert!(hard_exit_flag().load(Ordering::SeqCst));
    }

    #[test]
    fn escalate_aborts_and_returns_fast_when_worker_done() {
        let _g = HARD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (tx, rx) = mpsc::channel();
        let calls = std::sync::Mutex::new(Vec::new());
        let start = Instant::now();
        // worker already finished: sends abort, one SIGTERM, no grace-period
        // wait, no SIGKILL. A recording terminator avoids signaling real
        // process-global children spawned by concurrent tests.
        escalate_with(|| true, &tx, |force| calls.lock().unwrap().push(force));
        assert!(matches!(rx.recv(), Ok(Ack::Abort)));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![false],
            "SIGTERM only, no SIGKILL"
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "must not wait out the grace period when the worker is done"
        );
    }
}
