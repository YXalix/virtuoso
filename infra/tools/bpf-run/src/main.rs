//! bpf-run — VM 常驻 eBPF runner（tools.img /bin，默认随盘）：程序态观测入口。
//!
//! 与 bpftrace（脚本态、临时探索）分工：bpf-run 消费**宿主编译好的现成
//! .bpf.o**（rootfs.d drop-in 随 rootfs 分发、或测例内嵌），加载、按段
//! 挂接、把 ringbuf 事件解码成 JSONL 到 stdout——结构化事件流经 probe
//! 通道回宿主，落 agent-events.jsonl，即 AI-native 观测闭环。
//!
//! 支持段：kprobe/<sym>、kretprobe/<sym>、tracepoint/<cat>/<name>、
//! raw_tracepoint/<name>（其余段 warn 跳过）。事件解码契约：--struct NAME
//! 指向对象 .BTF 里的同名 struct/union（含 typedef 别名），记录长度匹配
//! 即按字段解码，否则 hex 兜底（aya 拒绝 ringbuf 的 values 注解成员，
//! 对象内声明解码类型不可行，显式 --struct 是唯一通道，见 src/btf.rs）。
//!
//! stdout 恒为纯事件 JSONL（每行 flush）；进度/诊断走 stderr；退出码：
//! 0 正常（含零事件）、1 加载/挂接失败（错误链含 verifier 日志）、2 用法错。
//!
//! 宿主 check 兼容：aya 是 Linux-only 依赖，经
//! `[target.'cfg(target_os = "linux")']` 注入；非 Linux 宿主编裸桩。


fn main() {
    #[cfg(target_os = "linux")]
    imp::main();
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("bpf-run: linux-only tool (aya bpf syscall); host build is a stub for IDE check");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, bail, Context};
    use aya::maps::{MapData, RingBuf};
    use aya::programs::{KProbe, RawTracePoint, TracePoint};
    use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};
    use serde_json::{json, Value};

    use bpf_run::btf::{hex, Btf};

    const DEFAULT_WAIT_SECS: f64 = 5.0;

    #[derive(Clone)]
    enum Attach {
        Kprobe(String),
        Kretprobe(String),
        TracePoint { cat: String, name: String },
        RawTracepoint(String),
    }

    impl Attach {
        fn label(&self) -> String {
            match self {
                Attach::Kprobe(s) => format!("kprobe/{s}"),
                Attach::Kretprobe(s) => format!("kretprobe/{s}"),
                Attach::TracePoint { cat, name } => format!("tracepoint/{cat}/{name}"),
                Attach::RawTracepoint(s) => format!("raw_tracepoint/{s}"),
            }
        }
    }

    fn parse_section(name: &str) -> Option<Attach> {
        if let Some(s) = name.strip_prefix("kprobe/") {
            return Some(Attach::Kprobe(s.into()));
        }
        if let Some(s) = name.strip_prefix("kretprobe/") {
            return Some(Attach::Kretprobe(s.into()));
        }
        if let Some(s) = name.strip_prefix("tracepoint/") {
            let (cat, tp) = s.split_once('/')?;
            return Some(Attach::TracePoint {
                cat: cat.into(),
                name: tp.into(),
            });
        }
        name.strip_prefix("raw_tracepoint/")
            .map(|s| Attach::RawTracepoint(s.into()))
    }

    fn usage() -> &'static str {
        "usage: bpf-run <obj.bpf.o> [--struct NAME]... [--program NAME]... [--wait SECS]"
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if let Err(e) = run(&args) {
            eprintln!("bpf-run: {e:#}");
            std::process::exit(1);
        }
    }

    fn parse_args(args: &[String]) -> anyhow::Result<(String, Vec<String>, Vec<String>, f64)> {
        let mut obj: Option<String> = None;
        let mut structs = Vec::new();
        let mut programs = Vec::new();
        let mut wait = DEFAULT_WAIT_SECS;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--struct" => structs.push(arg_value(&mut it, a)?),
                "--program" => programs.push(arg_value(&mut it, a)?),
                "--wait" => {
                    let v = arg_value(&mut it, a)?;
                    wait = v.parse().map_err(|_| anyhow!("--wait needs seconds, got {v}"))?
                }
                "-h" | "--help" => {
                    println!("{}", usage());
                    std::process::exit(0);
                }
                _ if a.starts_with('-') => bail!("unknown arg {a}\n{}", usage()),
                a => {
                    if obj.replace(a.to_string()).is_some() {
                        bail!("exactly one object expected\n{}", usage());
                    }
                }
            }
        }
        let Some(obj) = obj else { bail!("object path required\n{}", usage()) };
        if !(wait.is_finite() && wait >= 0.0) {
            bail!("--wait must be >= 0");
        }
        Ok((obj, structs, programs, wait))
    }

    fn arg_value<'a, I>(it: &mut I, flag: &str) -> anyhow::Result<String>
    where
        I: Iterator<Item = &'a String>,
    {
        it.next()
            .cloned()
            .ok_or_else(|| anyhow!("{flag} missing value"))
    }

    fn run(args: &[String]) -> anyhow::Result<()> {
        let (obj_path, structs, programs, wait) = parse_args(args)?;
        let bytes = std::fs::read(&obj_path).context("read object")?;

        // ELF 元数据：程序段（符号 = aya 的 program 名）+ .BTF
        let elf = object::File::parse(&bytes[..]).context("parse ELF")?;
        let mut sections: Vec<(usize, Attach)> = Vec::new();
        for sec in elf.sections() {
            let Ok(name) = sec.name() else { continue };
            if let Some(att) = parse_section(name) {
                sections.push((sec.index().0, att));
            }
        }
        let mut progs: Vec<(String, Attach)> = Vec::new();
        for s in elf.symbols() {
            if s.kind() != SymbolKind::Text || !s.is_definition() {
                continue;
            }
            let Some(si) = s.section_index() else { continue };
            let Some((_, att)) = sections.iter().find(|(i, _)| *i == si.0) else {
                continue;
            };
            let Ok(n) = s.name() else { continue };
            if !n.is_empty() {
                progs.push((n.to_string(), att.clone()));
            }
        }
        if progs.is_empty() {
            bail!(
                "no attachable programs (supported sections: kprobe/, kretprobe/, \
                 tracepoint/, raw_tracepoint/)"
            );
        }

        let btf = match elf.section_by_name(".BTF") {
            Some(sec) => Some(
                    Btf::parse(sec.data()?)
                        .map_err(|e| anyhow!("parse .BTF: {e}"))?,
                ),
            None => None,
        };

        // 加载 + 挂接。单程序失败 warn-and-continue（对象可含多程序，
        // 一处不可挂不吞其余观测面）；全部失败才整体报错。
        // load 失败的错误链带 verifier 日志——内核 BPF 开发反馈面。
        let mut bpf = aya::Ebpf::load(&bytes).map_err(|e| anyhow!("load: {e:#}"))?;
        let mut attached = 0usize;
        let mut failed = 0usize;
        for (sym, att) in &progs {
            if !programs.is_empty() && !programs.contains(sym) {
                continue;
            }
            let mut attach = || -> anyhow::Result<()> {
                let prog = bpf
                    .program_mut(sym)
                    .ok_or_else(|| anyhow!("program {sym} missing"))?;
                match att {
                    Attach::Kprobe(t) | Attach::Kretprobe(t) => {
                        let p: &mut KProbe = prog.try_into().map_err(|e| anyhow!("{e}"))?;
                        p.load().map_err(|e| anyhow!("load {sym}: {e:#}"))?;
                        p.attach(t, 0)?;
                    }
                    Attach::TracePoint { cat, name } => {
                        let p: &mut TracePoint = prog.try_into().map_err(|e| anyhow!("{e}"))?;
                        p.load().map_err(|e| anyhow!("load {sym}: {e:#}"))?;
                        p.attach(cat, name)?;
                    }
                    Attach::RawTracepoint(t) => {
                        let p: &mut RawTracePoint = prog.try_into().map_err(|e| anyhow!("{e}"))?;
                        p.load().map_err(|e| anyhow!("load {sym}: {e:#}"))?;
                        p.attach(t)?;
                    }
                }
                Ok(())
            };
            match attach() {
                Ok(()) => {
                    attached += 1;
                    eprintln!("bpf-run: attached {}", att.label());
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("bpf-run: {} failed: {e:#}", att.label());
                }
            }
        }
        if attached == 0 {
            bail!("no programs attached ({failed} failed)");
        }

        // ringbuf 发现（BTF .maps datasec）+ 解码类型（--struct，按记录长度匹配）
        // 解码候选：--struct 显式指定；缺省自动模式 = 全部具名 struct
        // （记录长度命中即按字段解码，多候选同长取 BTF 序首个）。
        let mut decoders: Vec<(u32, usize)> = Vec::new();
        if !structs.is_empty() && btf.is_none() {
            bail!("--struct needs a .BTF section in the object");
        }
        if let Some(b) = &btf {
            if structs.is_empty() {
                decoders = b.named_comps();
            } else {
                for name in &structs {
                    let (id, size) = b
                        .comp_by_name(name)
                        .ok_or_else(|| anyhow!("--struct {name}: not found in BTF"))?;
                    decoders.push((id, size as usize));
                }
            }
        }
        let mut rings: Vec<(String, RingBuf<MapData>)> = Vec::new();
        match &btf {
            Some(b) => {
                for name in b.ringbuf_maps() {
                    let Some(map) = bpf.take_map(&name) else { continue };
                    match RingBuf::try_from(map) {
                        Ok(rb) => rings.push((name, rb)),
                        Err(e) => eprintln!("bpf-run: ringbuf {name}: {e}"),
                    }
                }
            }
            None => eprintln!("bpf-run: no .BTF section; ringbuf discovery unavailable"),
        }
        if rings.is_empty() {
            eprintln!("bpf-run: no ringbuf maps; events (bpf_printk) go to trace_pipe");
        }

        // 事件循环：轮询全部 ringbuf，记录长度匹配 --struct 即字段解码
        let mut events: u64 = 0;
        let deadline = Instant::now() + Duration::from_secs_f64(wait);
        loop {
            for (name, rb) in &mut rings {
                while let Some(item) = rb.next() {
                    let rec: &[u8] = &item;
                    let value: Value = match (&btf, decoders.iter().find(|(_, sz)| *sz == rec.len()))
                    {
                        (Some(b), Some((id, _))) => b.decode(*id, rec, 0),
                        _ => hex(rec),
                    };
                    println!("{}", json!({"map": name, "data": value}));
                    events += 1;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!(
            "bpf-run: {events} event(s) from {} ringbuf(s) in {wait:.1}s",
            rings.len()
        );
        Ok(())
    }
}
