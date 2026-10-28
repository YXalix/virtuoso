//! Run artifacts (the runs domain): run directories (lifecycle/retention
//! policy), the output pump, and verdict.json persistence/readback.
//!
//! Responsibility boundaries:
//! - This module does **IO only**; judging semantics live in judge (marker
//!   protocol parsing + verdict determination + schema) and are not
//!   reimplemented here;
//! - verdict.json's schema is defined in a single place,
//!   `crate::judge::VerdictReport` (referenced here); exit code semantics
//!   live solely in `crate::judge` (EXIT_TIMEOUT/normalize); verdict
//!   presentation lives solely in the `test` closing line + `verdict.json`
//!   (the sole machine-readable surface), with no second presentation
//!   command. Normal finalization and the Ctrl-C fallback share the same
//!   construction path.

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};

use anyhow::Context;

use crate::util::unix_ms;
pub(crate) use crate::judge::RunMeta;
use crate::judge::{Artifacts, Audit, Event, EventKind, VerdictReport};

pub const RUNS_KEEP: usize = 20;

// ---------------------------------------------------------------- Run directories

pub fn runs_root(project_root: &Path) -> PathBuf {
    project_root.join("target/runs")
}

pub struct RunDir {
    pub id: String,
    pub path: PathBuf,
}

/// Create a directory named `<unix_ms>-<arch>` (fixed-width timestamp;
/// sorting directory names sorts by time).
pub fn create_run_dir(project_root: &Path, arch: &str) -> anyhow::Result<RunDir> {
    let id = format!("{}-{arch}", unix_ms());
    let path = runs_root(project_root).join(&id);
    std::fs::create_dir_all(&path)
        .with_context(|| format!("create run dir {} failed", path.display()))?;
    Ok(RunDir { id, path })
}

pub fn list_run_dirs(project_root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(runs_root(project_root))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs.reverse();
    dirs
}

/// Keep only the most recent `keep` runs (fixed-width timestamps in
/// directory names; lexicographic order is time order).
pub fn prune(project_root: &Path, keep: usize) {
    for stale in list_run_dirs(project_root).into_iter().skip(keep) {
        let _ = std::fs::remove_dir_all(stale);
    }
}

// ---------------------------------------------------------------- Output pump

/// Upper bound on QEMU stderr tail lines echoed on failure.
pub const QEMU_TAIL_LINES: usize = 40;

/// Separate presentation of the two output streams: the guest serial
/// (stdout) is echoed to the terminal line by line and persisted to
/// serial.log; QEMU's own output (stderr) goes only to qemu-stderr.log
/// without flooding the screen — only a bounded tail excerpt is kept and
/// echoed when the run fails (the scene of an early QEMU death or rejected
/// args is right in those lines).
#[derive(Debug)]
pub struct PumpOutcome {
    pub status: ExitStatus,
    /// Total number of QEMU stderr lines.
    pub qemu_lines: usize,
    /// At most the last `QEMU_TAIL_LINES` lines (without trailing newlines).
    pub qemu_tail: Vec<String>,
}

impl PumpOutcome {
    /// On nonzero exit (failed/timeout) with QEMU output, print the stderr
    /// tail to our stderr; on success stay quiet — the full text is in
    /// qemu-stderr.log, whose path the verdict.json artifacts section records.
    pub fn report_tail_on_failure(&self, code: i32) {
        if code == 0 || self.qemu_lines == 0 {
            return;
        }
        let omitted = self.qemu_lines - self.qemu_tail.len();
        let head = if omitted > 0 {
            format!("(first {omitted} lines omitted)")
        } else {
            String::new()
        };
        eprintln!(
            "---------- QEMU stderr tail ({} lines){head} ----------",
            self.qemu_tail.len()
        );
        for line in &self.qemu_tail {
            eprintln!("{line}");
        }
    }
}

/// Pump the two streams of an already spawned child separately (appended to
/// disk; only stdout is echoed to the terminal).
/// The child process is provided by crate::launcher::QemuInvocation::spawn(piped=true).
pub fn pump_child(
    child: &mut Child,
    out_log: &Path,
    err_log: &Path,
) -> std::io::Result<PumpOutcome> {
    let out = child.stdout.take().expect("stdout piped");
    let err = child.stderr.take().expect("stderr piped");

    fn pump_out(pipe: impl Read, log: &Path) -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let mut reader = std::io::BufReader::new(pipe);
        let stdout = std::io::stdout();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                break;
            }
            {
                let mut lock = stdout.lock();
                lock.write_all(&buf)?;
                lock.flush()?;
            }
            file.write_all(&buf)?;
        }
        Ok(())
    }

    // stderr: persisted only; the tail excerpt goes into a bounded shared
    // buffer for PumpOutcome to carry back.
    fn pump_err(
        pipe: impl Read,
        log: &Path,
        collected: &std::sync::Mutex<(usize, Vec<String>)>,
    ) -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let mut reader = std::io::BufReader::new(pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf)?;
            let mut state = collected.lock().unwrap();
            state.0 += 1;
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end_matches(['\n', '\r']);
            if state.1.len() >= QEMU_TAIL_LINES {
                state.1.remove(0);
            }
            state.1.push(line.to_string());
        }
        Ok(())
    }

    let out_log = out_log.to_path_buf();
    let err_log = err_log.to_path_buf();
    let collected = std::sync::Arc::new(std::sync::Mutex::new((0usize, Vec::new())));
    let t_out = std::thread::spawn(move || pump_out(out, &out_log));
    let t_err = {
        let collected = std::sync::Arc::clone(&collected);
        std::thread::spawn(move || pump_err(err, &err_log, &collected))
    };

    let status = child.wait()?;
    let _ = t_out.join();
    let _ = t_err.join();
    let (qemu_lines, qemu_tail) = collected.lock().unwrap().clone();
    Ok(PumpOutcome {
        status,
        qemu_lines,
        qemu_tail,
    })
}

// ---------------------------------------------------------------- Artifact persistence/readback

fn read_serial_audit(serial: &Path) -> Audit {
    let text = std::fs::read_to_string(serial).unwrap_or_default();
    crate::judge::parse(&text)
}

/// After a run finishes, write events.jsonl + verdict.json and return the
/// verdict string.
pub fn finalize_run(run: &RunDir, meta: &RunMeta) -> anyhow::Result<String> {
    let audit = read_serial_audit(&run.path.join("serial.log"));
    let verdict = meta.verdict_of(&audit);

    let mut events = audit.events.clone();
    events.push(Event {
        line_no: None,
        kind: EventKind::RunEnd,
        text: String::new(),
        seq: events.len(),
        extra: serde_json::json!({
            "exit_code": meta.exit_code,
            "duration_ms": meta.duration_ms,
            "verdict": verdict.as_str(),
        }),
    });
    let mut ev_body = String::new();
    for e in &events {
        ev_body.push_str(&serde_json::to_string(e).context("serialize events.jsonl")?);
        ev_body.push('\n');
    }
    std::fs::write(run.path.join("events.jsonl"), ev_body)?;

    let mut report = VerdictReport::build(&audit, meta);
    report.artifacts = Some(Artifacts {
        serial: run.path.join("serial.log").display().to_string(),
        qemu_stderr: run.path.join("qemu-stderr.log").display().to_string(),
        build_log: run.path.join("build.log").display().to_string(),
        events: run.path.join("events.jsonl").display().to_string(),
    });
    std::fs::write(
        run.path.join("verdict.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    Ok(verdict.as_str().to_string())
}

// ---------------------------------------------------------------- Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempRoot;
    use std::process::{Command, Stdio};

    /// Spawn a real child and pump its streams separately: stdout goes to
    /// out_log and is echoed; stderr goes only to err_log; PumpOutcome
    /// carries back the line count and tail excerpt.
    fn pump_shell(script: &str, out_log: &Path, err_log: &Path) -> PumpOutcome {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn /bin/sh");
        pump_child(&mut child, out_log, err_log).expect("pump_child")
    }

    #[test]
    fn pump_routes_streams_to_their_own_logs() {
        let dir = TempRoot::new("virtuoso-pump");
        let (out_log, err_log) = (dir.join("serial.log"), dir.join("qemu-stderr.log"));

        let outcome = pump_shell(
            "echo guest-boot; echo qemu-noise >&2; echo guest-test",
            &out_log,
            &err_log,
        );
        assert_eq!(outcome.status.code(), Some(0));
        assert_eq!(outcome.qemu_lines, 1);
        assert_eq!(outcome.qemu_tail, ["qemu-noise"]);
        let serial = std::fs::read_to_string(&out_log).unwrap();
        assert!(serial.contains("guest-boot") && serial.contains("guest-test"));
        let err = std::fs::read_to_string(&err_log).unwrap();
        assert_eq!(err, "qemu-noise\n");
    }

    #[test]
    fn qemu_tail_is_bounded_and_keeps_the_last_lines() {
        let dir = TempRoot::new("virtuoso-tail");
        let (out_log, err_log) = (dir.join("serial.log"), dir.join("qemu-stderr.log"));

        let outcome = pump_shell(
            "i=1; while [ $i -le 60 ]; do echo \"noise-$i\" >&2; i=$((i+1)); done",
            &out_log,
            &err_log,
        );
        assert_eq!(outcome.qemu_lines, 60);
        assert_eq!(outcome.qemu_tail.len(), QEMU_TAIL_LINES);
        assert_eq!(
            outcome.qemu_tail[0],
            format!("noise-{}", 60 - QEMU_TAIL_LINES + 1)
        );
        assert_eq!(
            outcome.qemu_tail.last().map(String::as_str),
            Some("noise-60")
        );
    }
}
