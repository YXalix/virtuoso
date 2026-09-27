//! bpftool 供给（kernel build 顺带产物随 tools 盘）：`virtuoso kernel build`
//! 在内核树内构建完全静态的 bpftool（in-tree libbpf 同树静态链入，版本与
//! 被测内核严格匹配）——内核态 BTF/prog/map 检视的控制面。builder 只做
//! 搬运：内核树 `tools/bpf/bpftool/bpftool` → tools 盘 `/bin/bpftool`
//! （PATH 已由 tools 盘 hook 注入）。
//!
//! 树内无产物（未跑过 kernel build，或目标 arch 与容器不同构——静态
//! libelf/z/zstd 只按容器原生 arch 装包）→ WARN 跳过，不致命：与
//! tools workspace 的降级语义一致。

use std::path::Path;

use anyhow::Context;

use crate::util::Progress;

/// 搬运内核树内静态 bpftool 进 tools 盘 staging。返回 `Ok(true)` = 已装入。
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

        // 树内无产物 → WARN 跳过（false），staging 不落文件
        assert!(!install(&ksrc, &bin, &mut Progress::stdout()).unwrap());
        // 非 ELF（树内半成品）→ 跳过
        std::fs::create_dir_all(ksrc.join("tools/bpf/bpftool")).unwrap();
        std::fs::write(ksrc.join("tools/bpf/bpftool/bpftool"), b"not an elf").unwrap();
        assert!(!install(&ksrc, &bin, &mut Progress::stdout()).unwrap());
        // ELF 产物 → 搬运 + 可执行位
        std::fs::write(ksrc.join("tools/bpf/bpftool/bpftool"), b"\x7fELFpayload").unwrap();
        assert!(install(&ksrc, &bin, &mut Progress::stdout()).unwrap());
        assert_eq!(
            std::fs::read(bin.join("bpftool")).unwrap(),
            b"\x7fELFpayload"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
