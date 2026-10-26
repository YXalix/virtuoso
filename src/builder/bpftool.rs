//! bpftool provisioning (a kernel build byproduct shipped on the tools
//! disk): `virtuoso kernel build` builds a fully static bpftool inside the
//! kernel tree (in-tree libbpf statically linked from the same tree,
//! version strictly matching the kernel under test) — the control plane
//! for kernel-side BTF/prog/map inspection. builder only ferries the
//! binary: kernel tree `tools/bpf/bpftool/bpftool` → tools disk
//! `/bin/bpftool` (PATH already injected by the tools disk hook).
//!
//! No artifact in the tree (kernel build never ran, or the target arch
//! differs from the container arch — static libelf/z/zstd packages are
//! installed for the container-native arch only) → WARN and skip,
//! non-fatal: consistent with the tools workspace degradation semantics.

use std::path::Path;

use anyhow::Context;

use crate::util::Progress;

/// Copies the in-tree static bpftool into the tools disk staging. Returns `Ok(true)` if installed.
pub(crate) fn install(
    kernel_path: &Path,
    dest_bin: &Path,
    progress: &mut Progress,
) -> anyhow::Result<bool> {
    let src = kernel_path.join("tools/bpf/bpftool/bpftool");
    if !src.is_file() {
        progress.line(
            "WARN: bpftool not in kernel tree — `virtuoso kernel build` supplies it (static, with in-tree libbpf); skipped for this tools.img",
        );
        return Ok(false);
    }
    if !crate::util::is_elf(&src) {
        progress.line(&format!(
            "WARN: {} is not an ELF binary — bpftool skipped",
            src.display()
        ));
        return Ok(false);
    }
    std::fs::create_dir_all(dest_bin)?;
    let dest = dest_bin.join("bpftool");
    std::fs::copy(&src, &dest)
        .with_context(|| format!("copy bpftool from {} failed", src.display()))?;
    crate::util::set_executable(&dest)?;
    progress.line("bpftool: /tools/bin/bpftool (static, kernel-tree built)");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_elf_and_skips_missing_or_non_elf() {
        let tmp =
            std::env::temp_dir().join(format!("virtuoso-bpftool-{}", std::process::id()));
        let ksrc = tmp.join("ksrc");
        let bin = tmp.join("tools/bin");
        let _ = std::fs::remove_dir_all(&tmp);

        // No artifact in tree → WARN and skip (false), nothing written to staging
        assert!(!install(&ksrc, &bin, &mut Progress::stdout()).unwrap());
        // Non-ELF (partial artifact in tree) → skip
        std::fs::create_dir_all(ksrc.join("tools/bpf/bpftool")).unwrap();
        std::fs::write(ksrc.join("tools/bpf/bpftool/bpftool"), b"not an elf").unwrap();
        assert!(!install(&ksrc, &bin, &mut Progress::stdout()).unwrap());
        // ELF artifact → copy + executable bit
        std::fs::write(ksrc.join("tools/bpf/bpftool/bpftool"), b"\x7fELFpayload").unwrap();
        assert!(install(&ksrc, &bin, &mut Progress::stdout()).unwrap());
        assert_eq!(
            std::fs::read(bin.join("bpftool")).unwrap(),
            b"\x7fELFpayload"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
