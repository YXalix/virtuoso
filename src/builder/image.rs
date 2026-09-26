//! 镜像打包（build-initrd.sh 尾段 + cpio2ext4.sh 的 Rust 接管）。
//! initramfs：原生 newc cpio + gzip（`builder::cpio`，无 GNU 工具依赖）；
//! rootfs：`du -sm + 2` 自动定容的 ext4（mke2fs -d，见 `find_mke2fs`）。

use std::path::{Path, PathBuf};

use anyhow::Context;

/// 定位 mke2fs：PATH → Homebrew e2fsprogs keg 路径（keg-only 不进 PATH，
/// Apple Silicon = /opt/homebrew，Intel = /usr/local）。
pub(crate) fn find_mke2fs() -> Option<PathBuf> {
    if let Some(p) = crate::util::which_path("mke2fs") {
        return Some(p);
    }
    [
        "/opt/homebrew/opt/e2fsprogs/sbin",
        "/usr/local/opt/e2fsprogs/sbin",
    ]
    .iter()
    .map(|d| Path::new(d).join("mke2fs"))
    .find(|p| p.is_file())
}

/// `mke2fs -q -F -t ext4 -b 4096 -O ^has_journal -L <label> -d <dir> <img> <size>M`，
/// size = du -sm + du/2 + 2（50% 余量兜底，不做精细账）。
/// `-O ^has_journal`：这些是每次 build 整体重生成的 appliance 镜像，无需
/// 恢复语义；且 journal 有尺寸悬崖 —— 缺省在 fs 越过某阈值时创建（约占
/// 一半数据区），小镜像差一个模块就坠崖（populate 报 Could not allocate
/// block），去掉后任意体量线性可控。
/// `-b 4096` 固定 4K 块：mke2fs 对小镜像缺省 1K 块，rootfs.d 大文件
/// drop-in 时元数据开销剧增、运行时空闲被保留块线吞掉（mkdir 报 ENOSPC）。
pub(crate) fn make_ext4(dir: &Path, out: &Path, label: &str) -> anyhow::Result<()> {
    let du = std::process::Command::new("du")
        .arg("-sm")
        .arg(dir)
        .output()
        .context("failed to run du")?;
    if !du.status.success() {
        anyhow::bail!("du -sm {} failed", dir.display());
    }
    let mb: u64 = String::from_utf8_lossy(&du.stdout)
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .context("failed to parse du output")?;
    let size_mb = mb + mb / 2 + 2;

    let mke2fs = find_mke2fs().ok_or_else(|| {
        anyhow::anyhow!(
            "mke2fs not found (Linux: e2fsprogs package; macOS: brew install e2fsprogs)"
        )
    })?;
    let _ = std::fs::remove_file(out);
    let status = std::process::Command::new(&mke2fs)
        .args([
            "-q", "-F", "-t", "ext4", "-b", "4096", "-O", "^has_journal", "-L", label, "-d",
        ])
        .arg(dir)
        .arg(out)
        .arg(format!("{size_mb}M"))
        .status()
        .with_context(|| format!("failed to run {} (install e2fsprogs)", mke2fs.display()))?;
    if !status.success() {
        anyhow::bail!("{label} ext4 build failed (mke2fs exit {status})");
    }
    Ok(())
}
