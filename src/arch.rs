//! Target arch matrix and host platform — the single source of truth replacing
//! the tables scattered across the original 3 shell scripts.

/// Target architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arch {
    Arm64,
    X86_64,
    Riscv64,
}

impl Arch {
    /// Same alias normalization logic as verify.sh / run-qemu.sh.
    pub(crate) fn parse(s: &str) -> Option<Arch> {
        match s {
            "aarch64" | "arm64" => Some(Arch::Arm64),
            "x86_64" | "amd64" => Some(Arch::X86_64),
            "riscv64" => Some(Arch::Riscv64),
            _ => None,
        }
    }

    /// Host architecture (the `uname -m` equivalent from the scripts).
    pub(crate) fn host_default() -> Option<Arch> {
        Self::parse(std::env::consts::ARCH)
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Arch::Arm64 => "arm64",
            Arch::X86_64 => "x86_64",
            Arch::Riscv64 => "riscv64",
        }
    }

    pub(crate) fn qemu_bin(self) -> &'static str {
        match self {
            Arch::Arm64 => "qemu-system-aarch64",
            Arch::X86_64 => "qemu-system-x86_64",
            Arch::Riscv64 => "qemu-system-riscv64",
        }
    }

    /// Kernel image path relative to the kernel tree.
    pub(crate) fn kernel_img(self) -> &'static str {
        match self {
            Arch::Arm64 => "arch/arm64/boot/Image",
            Arch::X86_64 => "arch/x86/boot/bzImage",
            Arch::Riscv64 => "arch/riscv/boot/Image",
        }
    }

    pub(crate) fn console(self) -> &'static str {
        match self {
            Arch::Arm64 => "ttyAMA0",
            Arch::X86_64 | Arch::Riscv64 => "ttyS0",
        }
    }

    /// Machine type. **Frozen baseline (original run-qemu.sh behavior):
    /// `virt` for all three arches** (the scripts never used q35; this stays
    /// the single entry point — introducing q35 later only changes here).
    #[allow(unused_variables)]
    pub(crate) fn machine(self) -> &'static str {
        "virt"
    }

    /// CPU model under TCG (the CPU_TCG column of run-qemu.sh).
    pub(crate) fn cpu_tcg(self) -> &'static str {
        match self {
            Arch::Arm64 => "cortex-a72",
            Arch::X86_64 => "qemu64",
            Arch::Riscv64 => "rv64",
        }
    }

    /// Rust static musl target triple (for building the tools workspace;
    /// crt-static links self-contained, producing a static ELF with no
    /// dynamic dependencies — aligned with the VM having no dynamic loader).
    pub(crate) fn rust_musl_triple(self) -> &'static str {
        match self {
            Arch::Arm64 => "aarch64-unknown-linux-musl",
            Arch::X86_64 => "x86_64-unknown-linux-musl",
            Arch::Riscv64 => "riscv64gc-unknown-linux-musl",
        }
    }

    /// `zig cc -target` triple (cross-compiling from non-Linux hosts; zig
    /// bundles a musl sysroot, and the musl target is static by nature).
    pub(crate) fn zig_triple(self) -> &'static str {
        match self {
            Arch::Arm64 => "aarch64-linux-musl",
            Arch::X86_64 => "x86_64-linux-musl",
            Arch::Riscv64 => "riscv64-linux-musl",
        }
    }

}

// ---------------------------------------------------------------- Host platform

// Host platform — the single source of truth for QEMU argument platform branches.
//
// The project targets Linux and macOS (Apple Silicon: QEMU uses HVF
// acceleration and has no memfd memory backend). The frozen argv baseline is
// kept once per host platform: the launcher's `argv_*` unit tests pin
// `HostOs` explicitly; the host platform only affects default values, never
// the frozen text.

/// Host operating system (the minimal classification of QEMU capability differences).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostOs {
    Linux,
    Darwin,
}

impl HostOs {
    /// Compile-time host platform; unrecognized platforms fall back to Linux semantics.
    pub(crate) fn current() -> Self {
        if cfg!(target_os = "macos") {
            HostOs::Darwin
        } else {
            HostOs::Linux
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            HostOs::Linux => "linux",
            HostOs::Darwin => "darwin",
        }
    }
}

#[cfg(test)]
mod host_os_tests {
    use super::HostOs;

    #[test]
    fn current_matches_compile_target() {
        // Zero deps and no cross-compiling in this crate; current() is decided by the compile target alone
        let expect = if cfg!(target_os = "macos") {
            HostOs::Darwin
        } else {
            HostOs::Linux
        };
        assert_eq!(HostOs::current(), expect);
    }
}
