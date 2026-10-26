//! coda-scaffold — scaffold for test case build.rs files (build-dep
//! companion to the coda test framework).
//!
//! Every test case crate's build.rs is fixed at one line:
//!
//! ```ignore
//! fn main() { coda_scaffold::run() }
//! ```
//!
//! Responsibilities: compile the test case's `c/*.c` into the test binary
//! (C over Rust), and write the resolved compile commands to the crate
//! root's `compile_commands.json` (gitignored) for clangd. When `c/` is
//! missing or empty, an empty `run_c_tests` stub is compiled in — the C
//! section symbol always exists, so `coda::run_and_exit` can call it
//! unconditionally without a link error.
//!
//! Compiler selection is one explicit path: `CC_<TRIPLE>` (the zig cc
//! wrapper injected by builder::cross), then `CC_<triple with hyphens>`,
//! then `TARGET_CC`, then `CC`, then the cc crate's host default. **Cross
//! builds (TARGET != HOST) with no CC takeover fail fast** — this prevents
//! the cc crate from silently falling back to the host cc and producing
//! glibc object code linked against musl libc (an ABI hazard); that is
//! exactly why zig is unified here.
//!
//! Convention: test case crates are direct subdirectories of
//! `infra/testcases/` (coda lives in `../coda/include`). Objects are linked
//! directly (rustc-link-arg-bins) instead of going through cc's compile():
//! in cross scenarios the host ar/ranlib cannot build a symbol index for
//! ELF objects (undefined symbol observed on macOS), while command-line
//! object files are accepted unconditionally by the linker, with no such
//! problem.

use std::env;
use std::path::{Path, PathBuf};
use std::process::exit;

/// Entry point for a test case build.rs (reads CARGO_MANIFEST_DIR / TARGET /
/// HOST and the other build script env vars).
pub fn run() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest
        .parent()
        .expect("crate dir always has a parent")
        .to_path_buf();
    let pkg = env::var("CARGO_PKG_NAME").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    let host = env::var("HOST").unwrap_or_default();
    let cross = !target.is_empty() && target != host;
    let c_dir = manifest.join("c");
    let coda_include = workspace.join("coda").join("include");

    let compiler = compiler_env(&target);
    if cross && compiler.is_none() {
        let upper = target.to_uppercase().replace('-', "_");
        eprintln!("{pkg} build.rs: cross build {target} needs a C compiler takeover (CC_{upper})");
        eprintln!("  normal path: `virtuoso build` (builder injects the zig cc wrapper)");
        eprintln!("  own toolchain: CC_{upper}=<cross-cc>");
        exit(1);
    }

    let mut build = cc::Build::new();
    if let Some(cc_bin) = &compiler {
        build.compiler(cc_bin);
    }
    build.warnings(true).include(&c_dir).include(&coda_include);

    let sources = c_sources(&c_dir);
    // Directory-level rerun: covers add/remove/modify (per-file granularity
    // would instead miss "newly added .c not recompiled")
    println!("cargo:rerun-if-changed={}", c_dir.display());
    println!(
        "cargo:rerun-if-changed={}",
        coda_include.join("coda.h").display()
    );
    for src in &sources {
        build.file(src);
    }
    if sources.is_empty() {
        // Empty stub: for test cases without a C section, the stub holds the
        // run_c_tests symbol
        let stub = PathBuf::from(env::var("OUT_DIR").unwrap()).join("coda_c_stub.c");
        std::fs::write(&stub, "void run_c_tests(void) {}\n").expect("failed to write OUT_DIR");
        build.file(&stub);
    }
    // Link objects directly: see the module docs for the ar/ranlib rationale
    for obj in &build.compile_intermediates() {
        println!("cargo:rustc-link-arg-bins={}", obj.display());
    }

    write_compile_commands(&manifest, &c_dir, &coda_include, compiler.as_deref());
}

/// Compiler env probing in the cc crate's order, made explicit (no reliance
/// on cc's internal resolution): target-specific keys first, then global
/// keys.
fn compiler_env(target: &str) -> Option<PathBuf> {
    [
        format!("CC_{}", target),
        format!("CC_{}", target.to_uppercase().replace('-', "_")),
        "TARGET_CC".to_string(),
        "CC".to_string(),
    ]
    .into_iter()
    .find_map(|k| env::var_os(&k).map(PathBuf::from))
}

fn c_sources(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().map(|e| e == "c").unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// Write this C compilation's entries to the crate root `compile_commands.json`.
/// Entries are the minimal form clangd needs (compiler + includes + -std);
/// they need not match the cc crate's full argv verbatim. clangd picks the
/// nearest CDB walking up from the source file, routing naturally per crate:
/// parallel test case builds each write their own file, no shared write
/// point. Host checks triggered by rust-analyzer produce a host variant,
/// exactly the shape the IDE wants; the previous scheme (a shared CDB at the
/// workspace root, where later crates overwrote earlier crates' entries
/// wholesale) is retired, and stale files are cleaned up here.
fn write_compile_commands(
    manifest: &Path,
    c_dir: &Path,
    coda_include: &Path,
    compiler: Option<&Path>,
) {
    let workspace = manifest.parent().expect("crate always has a parent");
    let _ = std::fs::remove_file(workspace.join("compile_commands.json"));

    let sources = c_sources(c_dir);
    if sources.is_empty() {
        return;
    }
    let compiler = compiler
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "cc".to_string());

    let entries: Vec<String> = sources
        .iter()
        .map(|file| {
            let args = [
                compiler.clone(),
                "-std=c11".to_string(),
                format!("-I{}", c_dir.display()),
                format!("-I{}", coda_include.display()),
                "-Wall".to_string(),
                "-c".to_string(),
                file.display().to_string(),
                "-o".to_string(),
                "/dev/null".to_string(),
            ];
            let args_json: Vec<String> = args.iter().map(|a| json_str(a.as_str())).collect();
            format!(
                "{{\"directory\": {}, \"arguments\": [{}], \"file\": {}}}",
                json_str(&manifest.display().to_string()),
                args_json.join(", "),
                json_str(&file.display().to_string())
            )
        })
        .collect();
    let out = manifest.join("compile_commands.json");
    let _ = std::fs::write(&out, format!("[{}]\n", entries.join(",\n")));
}

/// Minimal JSON string escaping (paths and compiler commands contain no
/// control characters; this suffices).
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
