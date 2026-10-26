//! Accelerator selection: KVM (Linux) / HVF (macOS) are available only when the host and
//! target architectures match (validated at argv assembly); cross-arch falls back to TCG.

use crate::{Arch, HostOs};

/// Accelerator: KVM (Linux) / HVF (macOS) are available only when host and target match
/// (caller-validated); cross-arch falls back to TCG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Accel {
    Tcg,
    Kvm,
    Hvf,
}

impl Accel {
    /// Display name in the verdict.json run fingerprint.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Accel::Tcg => "TCG",
            Accel::Kvm => "KVM",
            Accel::Hvf => "HVF",
        }
    }

    /// Platform-default accelerator: on macOS, host arch == target arch → HVF (near-native
    /// on Apple Silicon); everything else (including all Linux cases) → TCG. KVM on Linux
    /// is still enabled explicitly via `shell --kvm`; macOS forces pure emulation with
    /// `--tcg`. The host arch is passed in explicitly (runtime `Arch::host_default()`)
    /// rather than self-inspected inside the function — inferring from the compilation
    /// target would make unit tests pinning another platform's behavior always red on
    /// CI's other architecture.
    pub(crate) fn default_for(arch: Arch, host: HostOs, host_arch: Option<Arch>) -> Accel {
        if host == HostOs::Darwin && host_arch == Some(arch) {
            Accel::Hvf
        } else {
            Accel::Tcg
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_accel_only_hvf_on_darwin_same_arch() {
        assert_eq!(
            Accel::default_for(Arch::Arm64, HostOs::Darwin, Some(Arch::Arm64)),
            Accel::Hvf,
            "Apple Silicon host running an arm64 guest defaults to HVF"
        );
        assert_eq!(
            Accel::default_for(Arch::X86_64, HostOs::Darwin, Some(Arch::Arm64)),
            Accel::Tcg,
            "Apple Silicon host cross-running an x86_64 guest falls back to TCG"
        );
        assert_eq!(
            Accel::default_for(Arch::Arm64, HostOs::Darwin, None),
            Accel::Tcg,
            "unknown host arch falls back to TCG"
        );
        assert_eq!(
            Accel::default_for(Arch::Arm64, HostOs::Linux, Some(Arch::Arm64)),
            Accel::Tcg,
            "Linux defaults to TCG in every case (KVM still enabled explicitly via --kvm)"
        );
    }
}
