//! Test case build script: the generic part lives in coda-scaffold (this
//! test case has no `c/`, so an empty stub is compiled in); additionally
//! compiles `bpf/*.bpf.c` into eBPF objects with the **host** clang
//! (-target bpf). Artifacts go straight into the test binary via
//! include_bytes! — the .bpf.o never lands in the rootfs, so the VM has
//! zero file dependencies. BPF cross-compilation and the C test body's zig
//! cc cross-compilation are two different things: BPF bytecode is
//! independent of host/target architecture; any host with clang (LLVM ships
//! the bpf target by default) can produce it, no CC_<TRIPLE> injection
//! involved.

use std::path::PathBuf;

fn main() {
    coda_scaffold::run();
    compile_bpf_objects();
}

fn compile_bpf_objects() {
    use std::process::Command;

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let bpf_dir = manifest.join("bpf");
    // Directory-level rerun: covers add/remove/modify under bpf/ (same
    // convention as coda-scaffold's c/)
    println!("cargo:rerun-if-changed={}", bpf_dir.display());

    for src in bpf_sources(&bpf_dir) {
        let obj = out_dir.join(src.file_name().unwrap()).with_extension("o");
        // -g: emits BTF (the SEC(".maps") BTF map definitions need it to be
        // parsed by aya-obj)
        // -O2: the BPF backend requires optimization; unoptimized code's
        // stack layout fails to compile
        let status = Command::new("clang")
            .args(["-target", "bpf", "-g", "-O2", "-Wall", "-c"])
            .arg(&src)
            .arg("-o")
            .arg(&obj)
            .status()
            .unwrap_or_else(|e| panic!("bpf: failed to launch clang ({e}) -- building BPF objects requires clang on the host (LLVM ships the bpf target by default)"));
        if !status.success() {
            panic!("bpf: failed to compile {}", src.display());
        }
        println!("cargo:rustc-env=VIRTUOSO_BPF_{}={}", obj_name(&src), obj.display());
    }
}

fn bpf_sources(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                // Convention: BPF-side sources are always *.bpf.c (never
                // confused with the C test bodies in c/)
                .filter(|p| p.to_string_lossy().ends_with(".bpf.c"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn obj_name(src: &std::path::Path) -> String {
    src.file_stem()
        .unwrap()
        .to_string_lossy()
        .to_uppercase()
        .replace(['.', '-'], "_")
}
