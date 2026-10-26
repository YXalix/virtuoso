//! Cross-compilation wiring (any host → guest Linux static ELF).
//!
//! Single mechanism: a `zig cc -target <triple>` wrapper script. zig ships
//! its own musl sysroot and lld, so any host can produce static Linux
//! ELFs; one dependency (`brew install zig` / distro zig package) covers
//! both the C and Rust lines (testcases are all on cargo now, see
//! testcase.rs):
//! - C test bodies: the wrapper script is injected into the cc crate via
//!   `CC_<TRIPLE>` (the test case crate's build.rs probes that key
//!   explicitly);
//! - Rust test cases / tools: injected via `CARGO_TARGET_<TRIPLE>_LINKER`;
//!   when zig takes over linking it pairs with `-C link-self-contained=no`
//!   (rustc hands over its self-contained crt1/libc, supplied by zig's
//!   musl sysroot, otherwise `_start` is defined twice).
//!
//! Per-platform differences live only in the link channel: Linux hosts do
//! not inject LINKER/rustflags (rustc's self-contained musl link path is
//! unchanged; tools behave identically on Linux CI), but the CC channel is
//! always attempted — C test bodies need a musl compiler, and mixing host
//! gcc glibc object code with musl libc risks ABI breakage, so silent
//! fallback is not allowed (the test case build.rs fails fast on
//! cross-builds without the wiring; verify's host_tools table lists zig as
//! a hard prerequisite).
//! When `CC` / `CARGO_TARGET_*_LINKER` / `CC_<TRIPLE>` env vars are already
//! set, the corresponding wiring yields (a user-supplied toolchain wins).

use std::path::Path;

use crate::{Arch, HostOs};

/// Cross wiring for one build (value semantics; each injection channel yields independently).
#[derive(Debug, Default)]
pub(crate) struct CrossSetup {
    /// cc crate compiler injection for C test bodies (key like CC_AARCH64_UNKNOWN_LINUX_MUSL).
    pub cc_env: Option<(String, String)>,
    /// cargo linker injection (key like CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER;
    /// non-Linux hosts only).
    pub cargo_linker: Option<(String, String)>,
    /// cargo rustflags injection (key like CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS;
    /// non-Linux hosts only, always paired with `-C link-self-contained=no`).
    pub cargo_rustflags: Option<(String, String)>,
    /// Human-facing note (progress line / diagnostics).
    pub note: Option<String>,
}

impl CrossSetup {
    /// Applies the CC / linker / rustflags injections to a cargo command (no-op when unwired).
    pub(crate) fn apply_to_cargo(&self, cmd: &mut std::process::Command) {
        if let Some((k, v)) = &self.cc_env {
            cmd.env(k, v);
        }
        if let Some((k, v)) = &self.cargo_linker {
            cmd.env(k, v);
        }
        if let Some((k, v)) = &self.cargo_rustflags {
            cmd.env(k, v);
        }
    }
}

/// Resolves the cross wiring for the current host. Non-Linux host without
/// zig → explicit error (fail fast; without zig no guest artifacts can be
/// produced); a Linux host missing zig skips the CC channel injection so C
/// test bodies get a clear error from the test case build.rs (verify's
/// host_tools table is the upstream gate).
pub(crate) fn setup(arch: Arch, build_dir: &Path) -> anyhow::Result<CrossSetup> {
    let triple = arch.zig_triple().to_string();
    let upper = arch.rust_musl_triple().to_uppercase().replace('-', "_");
    let linker_key = format!("CARGO_TARGET_{upper}_LINKER");
    let rustflags_key = format!("CARGO_TARGET_{upper}_RUSTFLAGS");
    let cc_key = format!("CC_{upper}");
    let non_linux = HostOs::current() != HostOs::Linux;

    if std::env::var_os("CC").is_some() && (!non_linux || std::env::var_os(&linker_key).is_some()) {
        // User-supplied full toolchain: CC goes to C test bodies (build.rs
        // probes CC itself), linking goes to the user linker
        return Ok(CrossSetup {
            note: Some("cross: CC / linker taken over by environment".into()),
            ..Default::default()
        });
    }
    let Some(_zig) = crate::util::which_path("zig") else {
        if non_linux {
            anyhow::bail!(
                "building guest assets on a non-Linux host requires zig (brew install zig)\n  \
                 or bring your own toolchain: CC=<cross-cc> + CARGO_TARGET_{linker_key}=<linker>"
            );
        }
        return Ok(CrossSetup {
            note: Some(
                "cross: zig missing — C test bodies will error at build time (`virtuoso doctor --verbose` catches this earlier)"
                    .into(),
            ),
            ..Default::default()
        });
    };
    let _ = _zig; // a PATH hit is enough; invoked by name inside the script

    let dir = build_dir.join("cross");
    std::fs::create_dir_all(&dir)?;
    let wrapper = dir.join(format!("zig-cc-{}.sh", arch.name()));
    // -target is appended after "$@": with the clang driver the last one
    // wins. When crossing, the cc crate passes through a rust-style
    // --target=<rust triple> (zig reports UnknownOperatingSystem, observed
    // on zig 0.16); appending the embedded target reliably overrides it.
    std::fs::write(&wrapper, render_wrapper(&triple))?;
    crate::util::set_executable(&wrapper)?;

    let cc_env = if std::env::var_os(&cc_key).is_none() {
        Some((cc_key, wrapper.display().to_string()))
    } else {
        None
    };
    let (cargo_linker, cargo_rustflags) = if !non_linux {
        (None, None)
    } else {
        (
            if std::env::var_os(&linker_key).is_none() {
                Some((linker_key.clone(), wrapper.display().to_string()))
            } else {
                None
            },
            if std::env::var_os(&rustflags_key).is_none() {
                Some((rustflags_key, "-C link-self-contained=no".into()))
            } else {
                None
            },
        )
    };
    Ok(CrossSetup {
        cc_env,
        cargo_linker,
        cargo_rustflags,
        note: Some(format!("cross: zig cc -target {triple}")),
    })
}

/// Renders the zig cc wrapper script. rustc appends
/// `-Wl,--fix-cortex-a53-843419` by default for aarch64 linux targets (the
/// Cortex-A53 843419 erratum workaround); zig's lld does not recognize it
/// and errors with unsupported linker arg. The test harness does not tune
/// for the A53 microarchitecture, so filtering the flag per-argument is
/// enough (POSIX sh idiom: `for` snapshots the argument list, `shift`
/// consumes the head, `set --` re-appends allowlisted items).
fn render_wrapper(triple: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # generated by builder::cross — zig cc wrapper for {triple}\n\
         for arg; do\n\
         \x20 case \"$arg\" in\n\
         \x20   -Wl,--fix-cortex-a53-843419) ;;\n\
         \x20   *) set -- \"$@\" \"$arg\" ;;\n\
         \x20 esac\n\
         \x20 shift\n\
         done\n\
         exec zig cc \"$@\" -target {triple}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_filters_erratum_flag_and_appends_target() {
        let s = render_wrapper("aarch64-linux-musl");
        assert!(s.starts_with("#!/bin/sh"));
        // The erratum flag is filtered by the allowlist (rustc stable 1.98+ passes it explicitly for aarch64)
        assert!(s.contains("-Wl,--fix-cortex-a53-843419) ;;"));
        // -target is still appended after "$@" (overrides the rust triple passed through by the cc crate)
        assert!(s.ends_with("exec zig cc \"$@\" -target aarch64-linux-musl\n"));
    }

    #[test]
    fn env_key_shape() {
        // Mirrors the concatenation rule in setup(), guarding against spelling drift
        let upper = Arch::Arm64
            .rust_musl_triple()
            .to_uppercase()
            .replace('-', "_");
        assert_eq!(
            format!("CARGO_TARGET_{upper}_LINKER"),
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER"
        );
        assert_eq!(format!("CC_{upper}"), "CC_AARCH64_UNKNOWN_LINUX_MUSL");
    }

    #[test]
    fn linux_host_has_no_linker_injection() {
        // This test always runs on Linux (both CI and dev machines are
        // Linux); the Darwin branch is covered by acceptance on real
        // hardware. The Linux host link channel is always empty (rustc is
        // self-contained musl); the CC channel depends on whether zig is on
        // PATH and is not asserted here.
        if HostOs::current() == HostOs::Linux {
            let s = setup(Arch::Arm64, Path::new("/tmp")).unwrap();
            assert!(s.cargo_linker.is_none());
            assert!(s.cargo_rustflags.is_none());
        }
    }

    #[test]
    fn apply_to_cargo_sets_only_wired_keys() {
        let s = CrossSetup {
            cc_env: Some(("CC_X".into(), "wrapper".into())),
            ..Default::default()
        };
        let mut cmd = std::process::Command::new("true");
        s.apply_to_cargo(&mut cmd);
        assert_eq!(cmd.get_envs().count(), 1);
        assert_eq!(cmd.get_envs().next().unwrap().0, "CC_X");
    }
}
