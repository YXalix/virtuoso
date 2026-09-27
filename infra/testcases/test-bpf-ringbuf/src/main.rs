//! test-bpf-ringbuf —— 程序态 eBPF 试点：aya 加载 .bpf.o → kprobe 挂接 →
//! ringbuf 结构化事件回流。
//!
//! 证明链三步各有断言：宿主 clang 产的 BPF 目标码能被 VM 内核接受（加载）；
//! aya（纯 Rust）musl 静态可完成挂接（kprobe 走 perf PMU）；事件经 ringbuf
//! 回到用户态且可结构化解码（AI-native 观测的最小闭环）。触发即本用例
//! 自己 unlink 一个文件——kprobe 同步触发，事件 pid 必等于本进程 pid，
//! 无时序赌博。程序本体见 `bpf/unlink.bpf.c`。
//!
//! 宿主 check 兼容：aya 是 Linux-only 依赖（bpf syscall + Linux libc 符号），
//! 经 `[target.'cfg(target_os = "linux")']` 注入；非 Linux 宿主（macOS 上的
//! IDE check/clippy）只编框架壳，TESTS 走空。

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

    /// build.rs 编出的 eBPF 目标码（unlink.bpf.o），include_bytes! 直接嵌进
    /// 本二进制：.bpf.o 不落 rootfs，VM 内零文件依赖。
    static BPF_OBJ: &[u8] = include_bytes!(env!("VIRTUOSO_BPF_UNLINK_BPF"));

    const KPROBE_SYM: &str = "do_unlinkat";
    const TRIGGER: &str = "/tmp/virtuoso-bpf-ringbuf.trigger";
    const EVENT_WAIT: Duration = Duration::from_secs(5);

    /// ringbuf 记录布局（bpf/unlink.bpf.c 的 struct event，小端）。
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
        // 1. 解析加载：BTF map 定义（SEC(".maps")）+ kprobe 段都要过 aya-obj
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

        // 2. 挂接 kprobe（aya：perf PMU 优先，tracefs kprobe_events 兜底）
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

        // 3. 挂接后触发：本进程 unlink 一个文件，do_unlinkat 必命中
        std::fs::write(TRIGGER, b"x").expect("write trigger file");
        let _ = std::fs::remove_file(TRIGGER);

        // 4. 轮询 ringbuf 收事件，等 pid == 本进程的那条
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
                        // 结构化事件行：AI-native 观测的输出形态（串口/工件可回放）
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
