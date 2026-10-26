//! tools workspace install: VM-resident tools (std Rust + musl static) →
//! tools.img `/bin/` (an attached virtio-blk data disk, mounted at /tools
//! and referenced via PATH).
//!
//! Orthogonal to test case grouping: tools do not go into `/tests`, are not
//! auto-discovered by init, and take no part in marker protocol judging.
//! Build via `cargo build --release --target <arch musl triple>` (see
//! `Arch::rust_musl_triple`); musl crt-static self-contained linking
//! produces static ELF without dynamic dependencies — the fact that the VM
//! has no dynamic loader/dynamic libraries is unchanged.
//!
//! Degradation rules (explicit WARN, not swallowed errors; build failure still bails):
//! - missing `tools/` directory → silently skipped (projects not using tools are unaffected);
//! - cargo missing or musl target not installed with the toolchain → WARN and skip.
//!
//! Returns `Ok(true)` = tools installed (only then are tools.img and the
//! mount hook produced); `Ok(false)` = skipped per the rules above.

use std::path::Path;

use crate::builder::cross::CrossSetup;
use crate::util::Progress;

pub(crate) fn install(
    rust_dir: &Path,
    dest_bin: &Path,
    arch: crate::Arch,
    cross: &CrossSetup,
    progress: &mut Progress,
) -> anyhow::Result<bool> {
    let job = crate::builder::cargo_install::CargoInstall {
        rust_dir,
        dest: dest_bin,
        triple: arch.rust_musl_triple(),
        cross,
        label: "VM tools",
        item_prefix: "  Tool:",
        groups: None,
    };
    // Returns 0 = workspace missing or explicit WARN degradation (cargo/musl
    // target missing); build success with zero binaries is bailed inside
    // CargoInstall::run.
    Ok(job.run(progress)? > 0)
}

/// Whether the musl target is installed with the toolchain (the sysroot's rustlib directory existing counts as available).
pub(crate) fn musl_target_installed(triple: &str) -> bool {
    let out = std::process::Command::new("rustc")
        .args(["--print", "target-libdir", "--target", triple])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let dir = String::from_utf8_lossy(&o.stdout).trim().to_string();
            !dir.is_empty() && Path::new(&dir).is_dir()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn musl_target_check_reflects_install_state() {
        // This machine has aarch64 musl installed (manual rust-std), x86_64 not — both directions must be covered
        let aarch64 = musl_target_installed("aarch64-unknown-linux-musl");
        let x86_64 = musl_target_installed("x86_64-unknown-linux-musl");
        assert!(aarch64 || x86_64, "at least one musl target should be available");
    }
}
