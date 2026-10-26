//! test-bpf-ringbuf — programmatic eBPF pilot: aya loads a .bpf.o, attaches
//! a kprobe, and structured events flow back over the ringbuf.
//!
//! Each step of the proof chain gets an assertion: the BPF object built by
//! host clang is accepted by the VM kernel (load); aya (pure Rust), static
//! musl, can attach (kprobe via perf PMU); events return to user space over
//! the ringbuf and decode structurally (the minimal AI-native observability
//! loop). The trigger is the test case unlinking its own file — the kprobe
//! fires synchronously, so the event pid must equal this process's pid; no
//! timing gambles. The program itself lives in `bpf/unlink.bpf.c`.
//!
//! Host check compatibility: aya is a Linux-only dependency (bpf syscall +
//! Linux libc symbols), injected via `[target.'cfg(target_os = "linux")']`;
//! non-Linux hosts (IDE check/clippy on macOS) compile only the framework
//! shell and TESTS stays empty.

use coda::{run_and_exit, TestCase};

#[cfg(target_os = "linux")]
static TESTS: &[TestCase] = &[("bpf_ringbuf_events", bpf::bpf_ringbuf_events)];
#[cfg(not(target_os = "linux"))]
static TESTS: &[TestCase] = &[];

fn main() {
    run_and_exit(TESTS);
}

#[cfg(target_os = "linux")]
mod bpf {
    use std::time::{Duration, Instant};

    use aya::maps::RingBuf;
    use aya::programs::KProbe;

    /// eBPF object built by build.rs (unlink.bpf.o), embedded straight into
    /// this binary via include_bytes!: the .bpf.o never lands in the rootfs,
    /// so the VM has zero file dependencies.
    static BPF_OBJ: &[u8] = include_bytes!(env!("VIRTUOSO_BPF_UNLINK_BPF"));

    const KPROBE_SYM: &str = "do_unlinkat";
    const TRIGGER: &str = "/tmp/virtuoso-bpf-ringbuf.trigger";
    const EVENT_WAIT: Duration = Duration::from_secs(5);

    /// Ringbuf record layout (struct event from bpf/unlink.bpf.c,
    /// little-endian).
    struct Event {
        pid: u32,
        comm: String,
    }

    fn decode_event(rec: &[u8]) -> Option<Event> {
        if rec.len() < 20 {
            return None;
        }
        let pid = u32::from_le_bytes(rec[0..4].try_into().ok()?);
        let comm_raw = &rec[4..20];
        let end = comm_raw
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(comm_raw.len());
        Some(Event {
            pid,
            comm: String::from_utf8_lossy(&comm_raw[..end]).into_owned(),
        })
    }

    pub(super) fn bpf_ringbuf_events() -> bool {
        // 1. Parse and load: both the BTF map definitions (SEC(".maps")) and
        // the kprobe section must pass aya-obj
        let mut bpf = match aya::Ebpf::load(BPF_OBJ) {
            Ok(bpf) => {
                coda::pass!(".bpf.o loaded ({} bytes)", BPF_OBJ.len());
                bpf
            }
            Err(e) => {
                coda::fail!("load .bpf.o: {e}");
                return false;
            }
        };

        // 2. Attach the kprobe (aya: perf PMU first, tracefs kprobe_events
        // fallback)
        let prog: &mut KProbe = match bpf
            .program_mut("on_unlink")
            .expect("program on_unlink exists")
            .try_into()
        {
            Ok(prog) => prog,
            Err(e) => {
                coda::fail!("program type: {e}");
                return false;
            }
        };
        if let Err(e) = prog.load() {
            coda::fail!("prog load: {e}");
            return false;
        }
        if let Err(e) = prog.attach(KPROBE_SYM, 0) {
            coda::fail!("attach kprobe {KPROBE_SYM}: {e}");
            return false;
        }
        coda::pass!("kprobe attached: {KPROBE_SYM}");

        // 3. Trigger after attach: this process unlinks a file, so
        // do_unlinkat must be hit
        std::fs::write(TRIGGER, b"x").expect("write trigger file");
        let _ = std::fs::remove_file(TRIGGER);

        // 4. Poll the ringbuf for events, waiting for the one whose pid
        // equals this process
        let mut rb = match RingBuf::try_from(bpf.map_mut("events").expect("map events exists")) {
            Ok(rb) => rb,
            Err(e) => {
                coda::fail!("ringbuf map: {e}");
                return false;
            }
        };
        let deadline = Instant::now() + EVENT_WAIT;
        let mut got = false;
        loop {
            if let Some(item) = rb.next() {
                match decode_event(&item) {
                    Some(ev) => {
                        // Structured event line: the AI-native observability
                        // output shape (replayable from serial/artifacts)
                        coda::info!("event {{\"pid\":{},\"comm\":\"{}\"}}", ev.pid, ev.comm);
                        if ev.pid == std::process::id() {
                            got = true;
                            break;
                        }
                    }
                    None => coda::info!("event <malformed {} bytes>", item.len()),
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        coda::check!(
            got,
            "ringbuf event from self (pid {}) within {EVENT_WAIT:?}",
            std::process::id()
        )
    }
}
