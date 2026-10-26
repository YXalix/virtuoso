//! QEMU process launch: spawn an independent process group; reaping is delegated to the
//! guardian (Supervised registration). The interactive path (stdio inherited) additionally
//! hands the controlling terminal's foreground to the QEMU process group before exec;
//! TerminalHandover restores it when the session ends — an independent process group is a
//! "background group" relative to the terminal, so touching the controlling terminal
//! (tcsetattr into raw mode / reading stdin) gets frozen by SIGTTIN/SIGTTOU; without the
//! handover you get zero serial output plus a fake Ctrl+C exit.

use anyhow::Context;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use crate::launcher::guardian::{Supervised, TerminalHandover};

use super::QemuInvocation;

impl QemuInvocation {
    /// Spawns QEMU: independent process group (pgid = the returned child pid, handed to
    /// the guardian module for reaping). With `piped` = true stdout/stderr are piped (the
    /// test path captures the serial); false inherits the host stdio (interactive shell /
    /// debug) and returns a terminal foreground handover guard.
    pub(crate) fn spawn(
        self,
        piped: bool,
    ) -> anyhow::Result<(Child, u32, Option<TerminalHandover>)> {
        for (what, path) in [
            ("Kernel image", &self.kernel),
            ("Initramfs", &self.initrd),
            ("Rootfs image", &self.rootfs),
        ]
        .into_iter()
        .chain(self.data_disks.iter().map(|d| ("Data disk image", &d.path)))
        {
            if !path.is_file() {
                anyhow::bail!(
                    "{what} not found at {} (build the kernel / run `virtuoso build` first)",
                    path.display()
                );
            }
        }
        let args = self.argv()?;
        let mut cmd = Command::new(self.qemu_bin());
        cmd.args(&args).process_group(0);
        if piped {
            cmd.stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .stdin(Stdio::null());
        }
        let tty = if !piped { TerminalHandover::capture()? } else { None };
        if tty.is_some() {
            // SAFETY: the closure only makes async-signal-safe syscalls (the legal fork→exec set)
            unsafe { cmd.pre_exec(hand_over_terminal_foreground) };
        }
        let child = cmd.spawn().with_context(|| {
            format!(
                "{} not found; run `virtuoso doctor --verbose` for install hints",
                self.qemu_bin()
            )
        })?;
        let pgid = child.id();
        Ok((child, pgid, tty))
    }

    /// Spawns and registers supervision: registry + reaping guard + terminal foreground
    /// restoration in one step (replacing the caller's four-step boilerplate).
    pub(crate) fn spawn_supervised(self, piped: bool) -> anyhow::Result<(Child, Supervised)> {
        let (child, pgid, tty) = self.spawn(piped)?;
        Ok((child, Supervised::adopt(pgid).with_tty(tty)))
    }
}

/// Terminal foreground handover before exec: forms our own process group and points the
/// controlling terminal's foreground at it. Contains only async-signal-safe syscalls —
/// the only legal call set between fork and exec. Must complete before exec: deferring
/// it to a post-spawn fixup by the parent has a race; QEMU touching the terminal first
/// gets frozen. SIGTTOU is blocked meanwhile: at this moment our group is still
/// "background" relative to the terminal, and the kernel response to a background group
/// calling tcsetpgrp is SIGTTOU (default action stop); the mask is restored before exec,
/// giving QEMU a clean starting point.
fn hand_over_terminal_foreground() -> io::Result<()> {
    set_terminal_foreground(0)
}

/// fd-parameterized kernel of `hand_over_terminal_foreground`: the production path always
/// uses fd 0 (stdio inherited); the parameterization is kept for reuse in future pty scenarios.
fn set_terminal_foreground(tty_fd: libc::c_int) -> io::Result<()> {
    // SAFETY: setpgid(0,0) forms our own process group; idempotent with std's
    // process_group(0), with no ordering dependency between the two (tcsetpgrp's target
    // group must already exist).
    if unsafe { libc::setpgid(0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut blocked: libc::sigset_t = unsafe { std::mem::zeroed() };
    let mut saved: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: sigset_t zeroed + sigemptyset before use; mask change affects this thread only (single thread after fork).
    unsafe {
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut saved) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: tcsetpgrp only changes the terminal's foreground process group owner.
    let rc = unsafe { libc::tcsetpgrp(tty_fd, libc::getpgrp()) };
    // SAFETY: restores the entry mask; after exec QEMU starts with a clean mask.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &saved, std::ptr::null_mut()) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOTTY) {
            // fd is a tty but not this session's controlling terminal: under POSIX,
            // reading or writing such a terminal raises no SIGTTIN/SIGTTOU; no handover
            // needed, treat as tty-less.
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}
