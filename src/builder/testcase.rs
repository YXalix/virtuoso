//! Test case compilation and installation. C/Rust test cases share one
//! cargo workspace (`infra/testcases/`): the Rust framework is the entry
//! point and the C test body is compiled into the same binary by the test
//! case crate's build.rs (cc crate) (C over Rust). Artifacts are musl
//! static ELF — the VM has no dynamic loader, **the -static constraint
//! must not be relaxed**; cross-compilation: Rust always
//! `--target <arch musl triple>` (no rustflags file, host-OS independent);
//! the C compiler is injected via `CC_<TRIPLE>` by `cross::CrossSetup`
//! (zig cc wrapper).
//!
//! Degradation rules (compatible with the frozen no-masking-failures
//! constraint — degradation is an explicit WARN, not a swallowed error):
//! - missing directory → silently skipped (projects not using Rust test cases are unaffected);
//! - cargo missing or musl target not installed with the toolchain → WARN and skip;
//! - build failure → **bail** (never silent).

use std::path::{Path, PathBuf};

use crate::builder::cross::CrossSetup;
use crate::util::Progress;

/// Compiles the test case workspace (`infra/testcases/`) and installs into
/// `<rootfs>/tests/`. Returning the installed binary names is unnecessary —
/// init auto-discovers; this only guarantees the artifacts are complete.
/// Build log semantics: on failure bail with full output (cargo already
/// includes `error:` lines); on success print `warning:` lines.
pub(crate) fn install_rust(
    rust_dir: &Path,
    dest_tests: &Path,
    arch: crate::Arch,
    cross: &CrossSetup,
    progress: &mut Progress,
) -> anyhow::Result<()> {
    let groups = group_map(rust_dir);
    let job = crate::builder::cargo_install::CargoInstall {
        rust_dir,
        dest: dest_tests,
        triple: arch.rust_musl_triple(),
        cross,
        label: "rust testcases",
        item_prefix: "  Test:",
        groups: Some(&groups),
    };
    // Returns 0 = workspace missing or explicit WARN degradation (cargo/musl
    // target missing), not an error; build success with zero binaries is
    // bailed inside CargoInstall::run.
    job.run(progress)?;
    Ok(())
}

/// Test group map: scans workspace member crate manifests, reading
/// `package.metadata.virtuoso.group` (package name + custom `[[bin]]`
/// names → group). Crates without a declaration install flat into
/// `/tests/`; non-directory entries and bad manifests are ignored.
pub(crate) fn group_map(rust_dir: &Path) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir(rust_dir) else {
        return map;
    };
    for entry in entries.flatten() {
        let manifest = entry.path().join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(v) = text.parse::<toml::Table>() else {
            continue;
        };
        let Some(group) = v
            .get("package")
            .and_then(|p| p.get("metadata"))
            .and_then(|m| m.get("virtuoso"))
            .and_then(|x| x.get("group"))
            .and_then(|g| g.as_str())
        else {
            continue;
        };
        let mut names: Vec<String> = v
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default();
        if let Some(bins) = v.get("bin").and_then(|b| b.as_array()) {
            for bin in bins {
                if let Some(n) = bin.get("name").and_then(|x| x.as_str()) {
                    names.push(n.to_string());
                }
            }
        }
        for name in names {
            map.insert(name, group.to_string());
        }
    }
    map
}

/// Test binary check under `target/release/`: executable files, excluding byproducts such as `.d`/`.rlib`/`.so`.
pub(crate) fn rust_test_binary(p: &Path) -> Option<PathBuf> {
    let name = p.file_name()?.to_string_lossy();
    if !p.is_file() || !is_executable(p) {
        return None;
    }
    let skip = name.ends_with(".d")
        || name.ends_with(".rlib")
        || name.ends_with(".so")
        || name.ends_with(".a")
        || name.ends_with(".rmeta")
        || name.contains("build_script");
    (!skip).then(|| p.to_path_buf())
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_binary_filter_rejects_build_artifacts() {
        let dir = std::env::temp_dir().join(format!("builder-tc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str, mode: u32| {
            let p = dir.join(name);
            std::fs::write(&p, b"elf").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            p
        };
        let bin = mk("test-example", 0o755);
        let dep = mk("test-example.d", 0o644);
        let rlib = mk("libcoda.rlib", 0o644);
        let script = mk("build_script_build-xxxx", 0o755);

        assert!(rust_test_binary(&bin).is_some());
        assert!(rust_test_binary(&dep).is_none(), ".d byproducts must be filtered");
        assert!(rust_test_binary(&rlib).is_none(), "rlib must be filtered");
        assert!(
            rust_test_binary(&script).is_none(),
            "build scripts must be filtered"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn group_map_reads_metadata_and_bin_names() {
        let dir = std::env::temp_dir().join(format!("builder-gmap-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("test-net")).unwrap();
        std::fs::create_dir_all(dir.join("test-plain")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(
            dir.join("test-net/Cargo.toml"),
            "[package]\nname = \"test-net\"\n\n[[bin]]\nname = \"net-bin\"\n\n[package.metadata.virtuoso]\ngroup = \"smoke\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("test-plain/Cargo.toml"),
            "[package]\nname = \"test-plain\"\n",
        )
        .unwrap();
        let map = group_map(&dir);
        assert_eq!(map.get("test-net").map(String::as_str), Some("smoke"));
        assert_eq!(map.get("net-bin").map(String::as_str), Some("smoke"));
        assert!(!map.contains_key("test-plain"), "no declaration → flat, not in the map");
        std::fs::remove_dir_all(&dir).ok();
    }
}
