//! virtuoso-agent — resident in-VM agent.
//!
//! Transport: a virtio-serial port (QEMU-side virtserialport
//! name=virtuoso-agent, host-side chardev unix socket). Protocol: JSON lines
//! (one object per line).
//!
//! - On startup sends `{"type":"hello","agent":"virtuoso-agent","proto":1}`;
//! - Request `{"id":N,"cmd":"<shell command>"}`: executed via /bin/sh -c;
//!   stdout/stderr lines stream back one by one as
//!   `{"id":N,"type":"out"|"err","line":"..."}`, and when done it sends
//!   `{"id":N,"type":"exit","code":C}` (signal deaths normalize to 128+sig);
//! - Reserved command `ping` returns `{"id":N,"type":"pong"}` (host
//!   handshake);
//! - Unparseable lines get `{"type":"error","reason":"..."}` (no id).
//!
//! Device discovery: prefer an existing node, then scan the sysfs class and
//! fall back to mknod from the dev attribute (the observed openEuler quirk
//! of devtmpfs leaving /dev empty).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};
use std::os::unix::process::ExitStatusExt;

const PORT_NAME: &str = "virtuoso-agent";

fn main() {
    // The std runtime ignores SIGPIPE by default; no extra signal setup here.
    let Some(port) = open_agent_port() else {
        // No port = no virtio-serial device attached or driver not loaded:
        // exit silently (init starts us in guarded mode)
        eprintln!("virtuoso-agent: no {PORT_NAME} virtio-serial port found");
        std::process::exit(1);
    };
    if send(
        &port,
        json!({"type": "hello", "agent": PORT_NAME, "proto": 1}),
    )
    .is_err()
    {
        return; // port went dead immediately (host disconnected before connecting): exit
    }
    let reader = match port.try_clone() {
        Ok(f) => BufReader::new(f),
        Err(_) => return,
    };
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if handle_request(&port, &line).is_err() {
            break; // port write failed: host has disconnected
        }
    }
}

/// Handle one request line; Err = port not writable.
fn handle_request(port: &File, line: &str) -> std::io::Result<()> {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return send(
                port,
                json!({"type": "error", "reason": format!("bad request: {e}")}),
            );
        }
    };
    let id = req.get("id").and_then(Value::as_u64).unwrap_or(0);
    let Some(cmd) = req.get("cmd").and_then(Value::as_str) else {
        return send(
            port,
            json!({"id": id, "type": "error", "reason": "missing cmd"}),
        );
    };
    if cmd == "ping" {
        return send(port, json!({"id": id, "type": "pong"}));
    }
    run_command(port, id, cmd)
}

/// fork+exec /bin/sh -c and stream output back line by line. One thread per
/// read end; line events funnel through a channel to the main thread, which
/// writes the port serially (the port is a single-writer stream and cannot
/// be written concurrently).
fn run_command(port: &File, id: u64, cmd: &str) -> std::io::Result<()> {
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return send(
                port,
                json!({"id": id, "type": "error", "reason": format!("spawn /bin/sh: {e}")}),
            );
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel::<(&'static str, String)>();
    let mut handles = Vec::new();
    if let Some(out) = stdout {
        handles.push(relay(out, "out", tx.clone()));
    }
    if let Some(err) = stderr {
        handles.push(relay(err, "err", tx.clone()));
    }
    drop(tx);
    // rx closing = both relay threads have hit EOF (all senders dropped); no
    // sentinel event needed
    for ev in rx {
        send(port, json!({"id": id, "type": ev.0, "line": ev.1}))?;
    }
    for h in handles {
        let _ = h.join();
    }
    let code = match child.wait() {
        Ok(st) => match (st.code(), st.signal()) {
            (Some(c), _) => c,
            (None, Some(s)) => 128 + s,
            (None, None) => 127,
        },
        Err(e) => {
            return send(
                port,
                json!({"id": id, "type": "error", "reason": format!("wait: {e}")}),
            );
        }
    };
    send(port, json!({"id": id, "type": "exit", "code": code}))
}

/// Pump one pipe into the channel line by line; at EOF the sender is dropped
/// as the thread ends (closing the channel).
fn relay(
    pipe: impl Read + Send + 'static,
    kind: &'static str,
    tx: std::sync::mpsc::Sender<(&'static str, String)>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let reader = BufReader::new(pipe);
        for line in reader.lines().map_while(Result::ok) {
            if tx.send((kind, line)).is_err() {
                return;
            }
        }
    })
}

/// Write one JSON line to the port (appends \n; write_all resumes short
/// writes).
fn send(port: &File, v: Value) -> std::io::Result<()> {
    let mut buf = v.to_string();
    buf.push('\n');
    (&*port).write_all(buf.as_bytes())
}

/// Locate the virtserialport named PORT_NAME and open it read-write.
fn open_agent_port() -> Option<File> {
    // 1) Existing node (created normally by devtmpfs / udev-named directory)
    let direct = [
        format!("/dev/virtio-ports/{PORT_NAME}"),
        "/dev/vport0p0".to_string(),
        "/dev/vport0p1".to_string(),
    ];
    for path in &direct {
        if let Ok(f) = OpenOptions::new().read(true).write(true).open(path) {
            return Some(f);
        }
    }
    // 2) sysfs scan: /sys/class/virtio-ports/vportNpM/{name,dev}
    let class = "/sys/class/virtio-ports";
    let entries = std::fs::read_dir(class).ok()?;
    let mut names: Vec<_> = entries.flatten().collect();
    names.sort_by_key(|e| e.file_name());
    for entry in names {
        let vport = entry.file_name().to_string_lossy().to_string();
        let name_path = format!("{class}/{vport}/name");
        let Ok(name) = std::fs::read_to_string(&name_path) else {
            continue;
        };
        if name.trim() != PORT_NAME {
            continue;
        }
        let node = format!("/dev/{vport}");
        if let Ok(f) = OpenOptions::new().read(true).write(true).open(&node) {
            return Some(f);
        }
        // 3) mknod fallback: create the node from the dev attribute when
        // devtmpfs has not (same as the quirk table)
        let dev_attr = std::fs::read_to_string(format!("{class}/{vport}/dev")).ok()?;
        let (maj, min) = dev_attr.trim().split_once(':')?;
        let (maj, min): (u32, u32) = (maj.parse().ok()?, min.parse().ok()?);
        let cpath = std::ffi::CString::new(node.as_bytes()).ok()?;
        // SAFETY: cpath points to a valid C string; mknodat(AT_FDCWD, ...) is
        // a standard syscall.
        let rc = unsafe {
            libc::mknodat(
                libc::AT_FDCWD,
                cpath.as_ptr(),
                libc::S_IFCHR | 0o600,
                libc::makedev(maj, min),
            )
        };
        if rc != 0 {
            eprintln!(
                "virtuoso-agent: mknod {node} failed: {}",
                std::io::Error::last_os_error()
            );
            continue;
        }
        return OpenOptions::new().read(true).write(true).open(&node).ok();
    }
    None
}
