//! bpf-run — resident in-VM eBPF runner (tools.img /bin, on the disk by
//! default): the programmatic observability entry point.
//!
//! Division of labor vs bpftrace (script mode, ad-hoc exploration): bpf-run
//! consumes **ready-made .bpf.o objects built on the host** (distributed
//! with the rootfs via rootfs.d drop-ins, or embedded in test cases), loads
//! them, attaches per section, and decodes ringbuf events to JSONL on
//! stdout — the structured event stream returns to the host over the probe
//! channel and lands in agent-events.jsonl: the AI-native observability
//! loop.
//!
//! Supported sections: kprobe/<sym>, kretprobe/<sym>,
//! tracepoint/<cat>/<name>, raw_tracepoint/<name> (other sections are
//! skipped with a warning). Event decoding contract: --struct NAME points
//! at a same-named struct/union (including typedef aliases) in the object's
//! .BTF; records of matching length decode field-by-field, otherwise hex
//! fallback (aya rejects the values-annotated members of ringbufs, so
//! declaring decode types inside the object is not possible and an explicit
//! --struct is the only channel; see src/btf.rs).
//!
//! stdout is always pure event JSONL (flushed per line); progress and
//! diagnostics go to stderr; exit codes: 0 normal (including zero events),
//! 1 load/attach failure (error chain includes the verifier log), 2 usage
//! error.
//!
//! Host check compatibility: aya is a Linux-only dependency, injected via
//! `[target.'cfg(target_os = "linux")']`; non-Linux hosts build a bare stub.


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

        // ELF metadata: program sections (symbols = aya program names) + .BTF
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

        // Load + attach. A single program failure warn-and-continues (an
        // object may hold several programs; one unattachable point must not
        // swallow the rest of the observability surface); only everything
        // failing is a total error.
        // A load failure's error chain carries the verifier log — the kernel
        // BPF development feedback surface.
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

        // ringbuf discovery (BTF .maps datasec) + decode types (--struct,
        // matched by record length). Decode candidates: explicit --struct;
        // default auto mode = all named structs (a record length hit decodes
        // field-by-field; equal-length candidates take the first in BTF
        // order).
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

        // Event loop: poll every ringbuf; records whose length matches a
        // --struct decode field-by-field
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
