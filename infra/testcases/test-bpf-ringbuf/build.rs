//! 用例构建脚本：通用部分在 coda-scaffold（本用例无 `c/`，编入空桩）；
//! 额外把 `bpf/*.bpf.c` 用**宿主** clang 编成 eBPF 目标码（-target bpf），
//! 产物经 include_bytes! 直接进用例二进制——.bpf.o 不落 rootfs，VM 内
//! 零文件依赖。BPF 交叉与 C 测试体的 zig cc 交叉是两码事：BPF 字节码
//! 与宿主/目标架构无关，任何装了 clang（LLVM 默认含 bpf target）的宿主
//! 都能产，不走 CC_<TRIPLE> 注入。

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
    // 目录级 rerun：覆盖 bpf/ 下增/删/改（与 coda-scaffold 的 c/ 约定一致）
    println!("cargo:rerun-if-changed={}", bpf_dir.display());

    for src in bpf_sources(&bpf_dir) {
        let obj = out_dir.join(src.file_name().unwrap()).with_extension("o");
        // -g：产 BTF（SEC(".maps") 的 BTF map 定义靠它被 aya-obj 解析）
        // -O2：BPF 后端要求优化，未优化代码栈布局过不了编译
        let status = Command::new("clang")
            .args(["-target", "bpf", "-g", "-O2", "-Wall", "-c"])
            .arg(&src)
            .arg("-o")
            .arg(&obj)
            .status()
            .unwrap_or_else(|e| panic!("bpf: 启动 clang 失败（{e}）——BPF 目标码编译需要宿主安装 clang（LLVM 默认含 bpf target）"));
        if !status.success() {
            panic!("bpf: 编译 {} 失败", src.display());
        }
        println!("cargo:rustc-env=VIRTUOSO_BPF_{}={}", obj_name(&src), obj.display());
    }
}

fn bpf_sources(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                // 约定：BPF 侧源文件一律 *.bpf.c（与 c/ 的 C 测试体互不混淆）
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
