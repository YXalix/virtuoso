//! Image packing (Rust takeover of the tail of build-initrd.sh + cpio2ext4.sh).
//! initramfs: native newc cpio + gzip (`builder::cpio`, no GNU tool dependency);
//! rootfs: ext4 sized automatically as `du -sm + 2` (mke2fs -d, see `find_mke2fs`).

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Locates mke2fs: PATH → Homebrew e2fsprogs keg paths (keg-only, not on
/// PATH; Apple Silicon = /opt/homebrew, Intel = /usr/local).
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

/// `mke2fs -q -F -t ext4 -b 4096 -O ^has_journal -L <label> -d <dir> <img> <size>M`,
/// size = du -sm + du/2 + 2 (50% headroom fallback, no fine-grained accounting).
/// `-O ^has_journal`: these appliance images are fully regenerated on every
/// build and need no recovery semantics; the journal also has a size cliff —
/// by default it is created once the fs crosses a threshold (about half the
/// data area), and a small image falls off the cliff over a single extra
/// module (populate fails with Could not allocate block); without it, any
/// footprint scales linearly and predictably.
/// `-b 4096` fixes 4K blocks: mke2fs defaults to 1K blocks for small
/// images, where rootfs.d large-file drop-ins explode metadata overhead and
/// runtime free space gets eaten by reserved blocks (mkdir fails with ENOSPC).
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
