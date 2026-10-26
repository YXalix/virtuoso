//! probe — the AI interaction channel: sends a command batch through the virtio-serial
//! agent (tools/virtuoso-agent) and streams structured events back. Build → agent
//! serial launch → ping handshake → send each command → reap. Run artifacts:
//! serial.log + agent-events.jsonl in the run directory.
//!
//! Exit codes follow the crate::judge contract: 0 = all commands exited 0;
//! 124 = watchdog timeout; anything else = failure (protocol interruption / non-zero command exit).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{json, Value};

/// Underlying timeout for a single protocol read (poll granularity; determines how
/// quickly the watchdog KILL is noticed).
const IO_TICK: Duration = Duration::from_millis(500);

pub fn run_probe(
    cli_arch: Option<&str>,
    cli_timeout: Option<u64>,
    cmds: &[String],
    cmd_file: Option<&Path>,
    json: bool,
) -> anyhow::Result<i32> {
    let cfg = crate::config::Config::load()?;
    let arch = super::resolve_arch(&cfg, cli_arch)?;
    let topo = super::resolve_topology(&cfg)?;
    let timeout_secs = match cli_timeout {
        Some(0) => anyhow::bail!("probe timeout must be > 0"),
        Some(t) => t,
        None => 300,
    };

    // ---- build (probe always enables the agent channel: forcibly merge virtio_console
    // into the runtime module list, not depending on the [components.agent] switch; the
    // agent ships in tools.img into /bin, skipped with a WARN when the musl target is
    // missing — a later handshake timeout gives clear guidance) ----
    super::build::build_pair_for(&cfg, None, &["virtio_console".to_string()])?;

    let commands = collect_commands(cmds, cmd_file)?;
    anyhow::ensure!(
        !commands.is_empty(),
        "no commands: use --cmd <shell> (repeatable) or --cmd-file <file>"
    );

    let run = crate::runs::create_run_dir(&cfg.project_root, arch.name())?;
    println!("[RUN] artifacts: {}", run.path.display());

    // ---- launch (agent serial always on: probe bypasses the component switch,
    // socket given directly) ----
    let sock_path = run.path.join("agent.sock");
    let _ = std::fs::remove_file(&sock_path); // QEMU does not clean up an existing socket path
    let inv = super::build_invocation(&super::LaunchPlan {
        cfg: &cfg,
        arch,
        topo,
        accel: crate::launcher::Accel::Tcg,
        auto_test: false,
        only_tests: Vec::new(),
        agent_socket: Some(sock_path.clone()),
    })?;
    println!("[LAUNCH] {}", inv.command_line()?);
    let (mut child, mut sup) = inv.spawn_supervised(true)?;

    // The serial pump runs on a worker thread (blocking reads); the main thread runs
    // the agent protocol. When the total timeout hits, the watchdog KILLs the process
    // group → serial and socket both EOF, and the two threads join naturally.
    let timed_out = Arc::new(AtomicBool::new(false));
    let watchdog =
        crate::launcher::spawn_watchdog(sup.pgid(), timeout_secs, Arc::clone(&timed_out));
    let serial_log = run.path.join("serial.log");
    let stderr_log = run.path.join("qemu-stderr.log");
    let pump = std::thread::spawn(move || {
        let outcome = crate::runs::pump_child(&mut child, &serial_log, &stderr_log);
        (child, outcome)
    });

    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_secs);
    let outcome = session(&sock_path, &commands, json, &run.path, deadline);

    // Regardless of outcome: tear down the VM, join the threads, settle the watchdog
    // (take the snapshot before setting the exit flag, which the watchdog polls to
    // exit — same order as test_once, avoiding a spurious timeout flag)
    let timed_out_now = timed_out.load(Ordering::SeqCst);
    sup.kill_now();
    timed_out.store(true, Ordering::SeqCst);
    let _ = watchdog.join();
    let (mut child, pumped) = pump
        .join()
        .map_err(|_| anyhow::anyhow!("serial pump thread panicked"))?;
    let _ = child.wait();
    sup.finish();

    let exit_code = match &outcome {
        Ok(all_ok) if !timed_out_now => {
            if *all_ok {
                0
            } else {
                1
            }
        }
        _ if timed_out_now => crate::judge::EXIT_TIMEOUT,
        _ => 1,
    };
    // Same separated presentation as test: on failure show the QEMU stderr tail, on
    // success stay quiet.
    if let Ok(pumped) = &pumped {
        pumped.report_tail_on_failure(exit_code);
    }
    match &outcome {
        Ok(true) => println!("[RUN] probe: all commands passed"),
        Ok(false) => println!("[RUN] probe: some commands failed"),
        Err(e) => eprintln!("ERROR: probe session: {e:#}"),
    }
    println!(
        "[RUN] probe finished ({:.1}s, exit {exit_code}) — events: {}/agent-events.jsonl",
        started.elapsed().as_secs_f32(),
        run.path.display()
    );
    crate::runs::prune(&cfg.project_root, crate::runs::RUNS_KEEP);
    Ok(exit_code)
}

/// Protocol session: connect → hello/ping handshake → commands one by one.
/// Ok(true) = all passed.
fn session(
    sock_path: &Path,
    commands: &[String],
    json: bool,
    run_dir: &Path,
    deadline: Instant,
) -> anyhow::Result<bool> {
    let sock = connect(sock_path, deadline).context("agent socket connect")?;
    sock.set_read_timeout(Some(IO_TICK)).ok();

    let events_path = run_dir.join("agent-events.jsonl");
    let mut events = std::fs::File::create(&events_path)
        .with_context(|| format!("create {}", events_path.display()))?;
    let mut wire = LineSock::new(sock);

    let hello = wire.next_line(deadline)?.context(
        "EOF before agent hello (virtio_console module not loaded, or agent not started? see serial.log)",
    )?;
    log_event(&mut events, &hello, json)?;
    if !hello.contains("\"hello\"") {
        anyhow::bail!("unexpected first line from agent: {hello}");
    }
    // ping handshake: confirm the protocol round trip works (modules.conf fully
    // loaded, agent ready)
    let pong = exchange(&mut wire, &mut events, json, 0, "ping", |v| {
        v.get("type").and_then(Value::as_str) == Some("pong")
    })?;
    anyhow::ensure!(
        pong.lines.iter().any(|l| l.contains("\"pong\"")),
        "ping handshake failed"
    );

    let mut all_ok = true;
    for (idx, cmd) in commands.iter().enumerate() {
        let id = (idx + 1) as u64;
        if !json {
            println!("[{id}] $ {cmd}");
        }
        let reply = exchange(&mut wire, &mut events, json, id, cmd, |v| {
            v.get("type").and_then(Value::as_str) == Some("exit")
        })?;
        let exit_code = reply.exit_code;
        let had_error = reply.had_error;
        match exit_code {
            Some(0) => {}
            Some(c) => {
                all_ok = false;
                if !json {
                    println!("[{id}] exit {c}");
                }
            }
            None => {
                all_ok = false;
                if !json {
                    println!("[{id}] no exit event (protocol interrupted)");
                }
            }
        }
        if had_error {
            all_ok = false;
        }
    }
    Ok(all_ok)
}

struct Reply {
    /// All received event lines (JSON strings)
    lines: Vec<String>,
    /// The exit event's code (None = not received)
    exit_code: Option<i64>,
    /// Whether an error event appeared
    had_error: bool,
}

/// Send one request and collect events until a predicate fires (commands await exit;
/// ping awaits pong — ping has no exit event, so it cannot share the commands'
/// "await exit" termination condition).
fn exchange(
    wire: &mut LineSock,
    events: &mut std::fs::File,
    json: bool,
    id: u64,
    cmd: &str,
    done: impl Fn(&Value) -> bool,
) -> anyhow::Result<Reply> {
    let req = json!({"id": id, "cmd": cmd}).to_string();
    wire.send_line(&req)?;
    let mut reply = Reply {
        lines: Vec::new(),
        exit_code: None,
        had_error: false,
    };
    loop {
        let line = wire
            .next_line(Instant::now() + Duration::from_secs(600))?
            .context(
                "EOF while waiting for agent reply (VM died? watchdog fired? see serial.log)",
            )?;
        log_event(events, &line, json)?;
        let ev: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        if done(&ev) {
            if let Some("exit") = ev.get("type").and_then(Value::as_str) {
                reply.exit_code = ev.get("code").and_then(Value::as_i64);
            }
            reply.lines.push(line);
            return Ok(reply);
        }
        match ev.get("type").and_then(Value::as_str) {
            Some("error") => {
                reply.had_error = true;
                reply.lines.push(line);
            }
            _ => reply.lines.push(line),
        }
    }
}

/// Persist events to disk + terminal rendering (json mode passes through verbatim;
/// human mode prints summary lines).
fn log_event(events: &mut std::fs::File, line: &str, json: bool) -> std::io::Result<()> {
    events.write_all(line.as_bytes())?;
    events.write_all(b"\n")?;
    events.flush()?;
    if json {
        println!("{line}");
        return Ok(());
    }
    let Ok(ev) = serde_json::from_str::<Value>(line) else {
        println!("raw: {line}");
        return Ok(());
    };
    // human mode noise reduction: out/err streams and heartbeats only land in
    // agent-events.jsonl; the terminal only reports actionable events (error);
    // exit is reported uniformly by session
    let id = ev.get("id").and_then(Value::as_u64);
    if ev.get("type").and_then(Value::as_str) == Some("error") {
        println!(
            "{}error: {}",
            tag(id),
            ev.get("reason").and_then(Value::as_str).unwrap_or("?")
        );
    }
    Ok(())
}

fn tag(id: Option<u64>) -> String {
    id.map(|i| format!("[{i}] "))
        .unwrap_or_else(|| "[agent] ".into())
}

/// Connect to the agent socket: QEMU listens immediately; the guest agent only speaks
/// after boot completes, but the chardev connection itself succeeds right after
/// startup — connected ≠ agent ready; readiness is confirmed by the hello line.
fn connect(sock_path: &Path, deadline: Instant) -> std::io::Result<UnixStream> {
    loop {
        match UnixStream::connect(sock_path) {
            Ok(s) => return Ok(s),
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::ConnectionRefused =>
            {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(IO_TICK);
            }
            Err(e) => return Err(e),
        }
    }
}

fn collect_commands(cmds: &[String], cmd_file: Option<&Path>) -> anyhow::Result<Vec<String>> {
    let mut all: Vec<String> = cmds.to_vec();
    if let Some(f) = cmd_file {
        let text = std::fs::read_to_string(f).with_context(|| format!("read {}", f.display()))?;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            all.push(line.to_string());
        }
    }
    Ok(all)
}

/// Unix socket line reader: self-managed buffer + polled timeout (BufReader can
/// swallow a buffered partial line on TimedOut; here bytes are moved one by one
/// to avoid loss).
struct LineSock {
    sock: UnixStream,
    buf: Vec<u8>,
    pos: usize,
    fill: usize,
}

impl LineSock {
    fn new(sock: UnixStream) -> Self {
        Self {
            sock,
            buf: vec![0; 8192],
            pos: 0,
            fill: 0,
        }
    }

    fn send_line(&mut self, line: &str) -> std::io::Result<()> {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        self.sock.write_all(&bytes)
    }

    /// Read one line (without the trailing \n). Returns Err if no complete line arrives
    /// within the deadline; Ok(None) on EOF with no residue.
    fn next_line(&mut self, deadline: Instant) -> anyhow::Result<Option<String>> {
        let mut line = Vec::new();
        loop {
            if self.pos < self.fill {
                if let Some(nl) = self.buf[self.pos..self.fill]
                    .iter()
                    .position(|&b| b == b'\n')
                {
                    line.extend_from_slice(&self.buf[self.pos..self.pos + nl]);
                    self.pos += nl + 1;
                    return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
                }
                line.extend_from_slice(&self.buf[self.pos..self.fill]);
                self.pos = self.fill;
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "timeout waiting for agent line ({} bytes buffered)",
                    line.len()
                );
            }
            match self.sock.read(&mut self.buf) {
                Ok(0) => {
                    return if line.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(String::from_utf8_lossy(&line).into_owned()))
                    };
                }
                Ok(n) => {
                    self.buf.copy_within(0..n, 0);
                    self.pos = 0;
                    self.fill = n;
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_file_skips_comments_and_blank_lines() {
        let dir = std::env::temp_dir().join(format!("probe-cmds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("cmds.txt");
        std::fs::write(&f, "# comment\n\nuname -a\n  dmesg | tail -2  \n").unwrap();
        let cmds = collect_commands(&["echo hi".to_string()], Some(&f)).unwrap();
        assert_eq!(cmds, ["echo hi", "uname -a", "dmesg | tail -2"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_refuses_before_socket_exists() {
        let missing =
            std::env::temp_dir().join(format!("probe-absent-{}.sock", std::process::id()));
        let started = Instant::now();
        let r = connect(&missing, started + Duration::from_millis(1200));
        assert!(r.is_err());
        assert!(started.elapsed() >= Duration::from_millis(1100));
    }
}
