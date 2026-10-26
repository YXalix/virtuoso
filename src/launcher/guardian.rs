//! guardian — the guardian: RAII governance of process groups and the terminal foreground.
//!
//! `ProcessGroupGuard` replaces the PID-file + `kill -- -PGID` hack of the early
//! Makefile-driven scheme: the guard holds the process group while alive, and `Drop`
//! (covering panic unwinds, early error returns, and the Ctrl-C exit path) guarantees
//! reaping, leaving no stray QEMU child on the host. `Supervised` layers an active
//! process-group registry on top (reaped by the Ctrl-C guardian); the registry and the
//! wall-clock watchdog live in the same file (the registry section). `TerminalHandover`
//! covers the terminal foreground side: interactive sessions hand the controlling
//! terminal's foreground to the QEMU process group (the handover completes before the
//! child execs, see qemu::spawn); finish/Drop restores it to the caller's process group.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Signals a process group. sig accepts "KILL" / "TERM"; "0" is a liveness probe
/// (no signal sent, only existence and permission checks). pgid=0 is always rejected:
/// killpg(0) would hit the caller's own process group. Exposed for callers such as the
/// watchdog thread that cannot hold a guard.
/// Returns false = group missing, no permission, or unsupported signal name.
pub(crate) fn signal_group(pgid: u32, sig: &str) -> bool {
    if pgid == 0 {
        return false;
    }
    let signum = match sig {
        "KILL" => libc::SIGKILL,
        "TERM" => libc::SIGTERM,
        "0" => 0,
        _ => return false,
    };
    // SAFETY: killpg only sends signum to the target process group; pgid=0 was rejected above.
    unsafe { libc::killpg(pgid as libc::pid_t, signum) == 0 }
}

/// Process-group reaping guard. For a child spawned with `process_group(0)` the pid is
/// the pgid; dropping without `disarm` KILLs the whole process group.
pub(crate) struct ProcessGroupGuard {
    pgid: u32,
    armed: bool,
}

impl ProcessGroupGuard {
    /// Adopts a process group.
    pub(crate) fn adopt(pgid: u32) -> Self {
        Self {
            pgid,
            armed: pgid != 0,
        }
    }

    /// KILLs the whole process group (idempotent; the guard disarms itself after reaping).
    pub(crate) fn kill_now(&mut self) {
        if self.armed {
            signal_group(self.pgid, "KILL");
            self.armed = false;
        }
    }

    /// Releases the reaping duty (call once the process group is known to have exited).
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            signal_group(self.pgid, "KILL");
        }
    }
}

/// Terminal foreground handover guard: records the caller's current foreground process
/// group and restores it when the session ends. The restoration covers cases beyond a
/// job-control shell reclaiming it on its own (an ancestor script without job control
/// never reclaims — a dangling foreground group freezes later processes reading the tty).
pub(crate) struct TerminalHandover {
    saved_pgid: libc::pid_t,
}

impl TerminalHandover {
    /// Records the current terminal foreground group. fd0 not a controlling terminal →
    /// Ok(None) (piped path / tty-less environment: QEMU reads a pipe or /dev/null, no
    /// SIGTTIN issue, no handover needed).
    pub(crate) fn capture() -> anyhow::Result<Option<Self>> {
        // SAFETY: isatty only queries the fd type; no side effects.
        if unsafe { libc::isatty(0) } != 1 {
            return Ok(None);
        }
        // SAFETY: tcgetpgrp only reads the terminal foreground owner.
        let saved_pgid = unsafe { libc::tcgetpgrp(0) };
        if saved_pgid < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOTTY) {
                // fd0 is a tty but not this session's controlling terminal (isatty already
                // passed): reading or writing such a terminal raises no SIGTTIN/SIGTTOU;
                // no handover needed, treat as tty-less.
                return Ok(None);
            }
            anyhow::bail!("tcgetpgrp(0) failed: {err}");
        }
        Ok(Some(Self { saved_pgid }))
    }
}

impl Drop for TerminalHandover {
    fn drop(&mut self) {
        // After QEMU (the foreground group) exits, the caller is back "in the background";
        // the kernel response to a background group calling tcsetpgrp is SIGTTOU (default
        // action stop), so the signal must be blocked for the restoration.
        let mut blocked: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut saved_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: sigset_t zeroed + sigemptyset before use; the mask change affects this thread only.
        unsafe {
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGTTOU);
            if libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut saved_mask) != 0 {
                return; // without the mask, leave the foreground alone: better dangling than risking self-stop
            }
            libc::tcsetpgrp(0, self.saved_pgid);
            libc::pthread_sigmask(libc::SIG_SETMASK, &saved_mask, std::ptr::null_mut());
        }
    }
}

/// Combined handle of registry registration + reaping guard: `adopt` once after spawn,
/// `finish` once at teardown, replacing the caller's four-step register / adopt /
/// unregister / disarm boilerplate.
pub(crate) struct Supervised {
    pgid: u32,
    guard: ProcessGroupGuard,
    tty: Option<TerminalHandover>,
}

impl Supervised {
    /// Adopts a process group and registers it as active (visible to the Ctrl-C guardian).
    pub(crate) fn adopt(pgid: u32) -> Self {
        register(pgid);
        Self {
            pgid,
            guard: ProcessGroupGuard::adopt(pgid),
            tty: None,
        }
    }

    /// Attaches terminal foreground restoration duty (interactive sessions only). The
    /// guard is declared before tty: on an exceptional Drop path QEMU is reaped before
    /// the foreground is restored.
    pub(crate) fn with_tty(mut self, tty: Option<TerminalHandover>) -> Self {
        self.tty = tty;
        self
    }

    pub(crate) fn pgid(&self) -> u32 {
        self.pgid
    }

    /// KILLs the whole process group (idempotent; disarms itself after reaping).
    pub(crate) fn kill_now(&mut self) {
        self.guard.kill_now();
    }

    /// Unregisters from the registry and releases the reaping duty (call once the process
    /// group has exited naturally); the terminal foreground is restored to the caller's
    /// process group at the same point.
    pub(crate) fn finish(&mut self) {
        clear();
        self.guard.disarm();
        self.tty = None; // Drop restores the foreground
    }
}

// ---------------------------------------------------------------- registry and watchdog

// Active process-group registry: global state shared by the Ctrl-C guardian and the
// wall-clock watchdog.
//
// At most one wrapped VM process group at a time; `Supervised::adopt` registers it and
// `finish` unregisters; the Ctrl-C guardian reaps accordingly, replacing scattered
// static-variable boilerplate at call sites.

static ACTIVE_PGID: AtomicU32 = AtomicU32::new(0);

/// Registers the currently wrapped process group (the Ctrl-C guardian reaps accordingly).
pub(crate) fn register(pgid: u32) {
    ACTIVE_PGID.store(pgid, Ordering::SeqCst);
}

/// Unregisters (call after the process group exits naturally or has been reaped).
pub(crate) fn clear() {
    ACTIVE_PGID.store(0, Ordering::SeqCst);
}

/// Current active process group (0 = none; consumed only by this module's guardian/watchdog).
fn active() -> u32 {
    ACTIVE_PGID.load(Ordering::SeqCst)
}

/// Installs the Ctrl-C guardian: KILLs the active process group, then exits 130 (matching
/// shell interrupt semantics; the constant is pinned in crate::util, with judge as the
/// single exit-code semantics table).
pub(crate) fn install_ctrlc_guard() {
    let _ = ctrlc::set_handler(|| {
        let pgid = active();
        if pgid != 0 {
            signal_group(pgid, "KILL");
        }
        std::process::exit(crate::util::EXIT_INTERRUPTED);
    });
}

/// Wall-clock watchdog: at the deadline KILLs the process group and sets quit (equivalent
/// to timeout --signal=KILL's 124 semantics; callers read the quit flag to tell timeout
/// from natural exit).
pub(crate) fn spawn_watchdog(
    pgid: u32,
    timeout_secs: u64,
    quit: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            if quit.load(Ordering::SeqCst) {
                return;
            }
            if Instant::now() >= deadline {
                quit.store(true, Ordering::SeqCst);
                signal_group(pgid, "KILL");
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn spawn_sleep_group(secs: &str) -> (std::process::Child, u32) {
        let child = Command::new("sleep")
            .arg(secs)
            .process_group(0)
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pgid = child.id();
        (child, pgid)
    }

    fn group_alive(pgid: u32) -> bool {
        signal_group(pgid, "0")
    }

    #[test]
    fn drop_kills_process_group() {
        let (mut child, pgid) = spawn_sleep_group("60");
        {
            let _guard = ProcessGroupGuard::adopt(pgid);
            assert!(group_alive(pgid));
        }
        let _ = child.wait();
        assert!(
            !group_alive(pgid),
            "process group must not survive guard drop"
        );
    }

    #[test]
    fn kill_now_is_idempotent_and_disarms() {
        let (mut child, pgid) = spawn_sleep_group("60");
        let mut guard = ProcessGroupGuard::adopt(pgid);
        guard.kill_now();
        guard.kill_now(); // second call: no panic, no signal
        let _ = child.wait();
        assert!(!group_alive(pgid));
    }

    #[test]
    fn adopt_zero_is_inert() {
        // pgid=0 must be inert: signal_group rejects -0 (otherwise it would kill the current process group)
        let mut guard = ProcessGroupGuard::adopt(0);
        guard.kill_now();
        guard.disarm();
        assert!(
            !group_alive(0),
            "guard must never signal the caller's own group"
        );
    }
}
