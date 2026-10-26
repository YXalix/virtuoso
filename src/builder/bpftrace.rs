//! bpftrace provisioning ([components.bpf], off by default): official
//! AppImage from a GitHub Release → download cache → unpacked in a
//! container into a Nix closure tree tar → run directly from the tools
//! disk at `/tools/nix`.
//!
//! Release assets (`bpftrace-aarch64` etc.) are AppImages: an ELF
//! runtime + an appended squashfs payload, where the payload is a Nix
//! closure with dynamically linked LLVM/clang — the official "statically
//! built" claim means a self-contained closure, not a static ELF. A bare
//! VM without FUSE cannot run it; extraction goes through a container:
//! `--appimage-extract` is pure userspace extraction, independent of
//! FUSE/kernel squashfs.
//!
//! Extraction must happen inside a **container overlay** (Linux ext4,
//! case-sensitive) and stream back to the host as a single-file tar: the
//! Nix tree contains case-collision pairs such as Eterm/eterm (ncurses
//! terminfo); extracting directly into a macOS host directory dies midway
//! with EACCES on OrbStack virtiofs. Collision pairs reduce to decorative
//! entries merged at load time (host tar landing on APFS silently
//! overwrites same-named paths) and do not affect bpftrace operation.
//! Symbolic links pass through tar verbatim and enter ext4 unchanged via
//! mke2fs -d; Nix absolute-path symlinks (interpreter/RPATH =
//! /nix/store/...) are satisfied inside the VM by the wrapper's
//! `ln -s /tools/nix /nix`: zero runtime unpacking, zero tmpfs.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow};

use crate::Arch;
use crate::util::Progress;

pub(crate) const VERSION: &str = "0.27.0";
const REPO: &str = "bpftrace/bpftrace";
/// Container image for extraction: only sh + tar + the static AppImage
/// runtime are needed, alpine suffices (`docker run` auto-pulls when the
/// local image is missing).
const EXTRACT_IMAGE: &str = "alpine:latest";

/// Architecture suffix of release assets (official releases ship aarch64/x86_64 only).
fn release_arch(arch: Arch) -> anyhow::Result<&'static str> {
    match arch {
        Arch::Arm64 => Ok("aarch64"),
        Arch::X86_64 => Ok("x86_64"),
        Arch::Riscv64 => Err(anyhow!(
            "official bpftrace releases have no riscv64 asset; components.bpf does not support this arch yet"
        )),
    }
}

/// Provisioning cache root: `target/build/bpftrace/bpftrace-<version>/` (version is part of the cache key).
fn cache_dir(build_dir: &Path) -> PathBuf {
    build_dir.join("bpftrace").join(format!("bpftrace-{VERSION}"))
}

/// Ensures the extracted-tree cache is ready and returns the cached tar path. Any failure is an Err (build aborts).
pub(crate) fn ensure(
    build_dir: &Path,
    arch: Arch,
    progress: &mut Progress,
) -> anyhow::Result<PathBuf> {
    let suffix = release_arch(arch)?;
    let dir = cache_dir(build_dir);
    let tar = dir.join("nix-tree.tar.gz");
    if tar.is_file() {
        progress.line(&format!("bpftrace: cached v{VERSION}"));
        return Ok(tar);
    }
    std::fs::create_dir_all(&dir)?;

    let appimage = dir.join(format!("bpftrace-{VERSION}-{suffix}.appimage"));
    if !appimage.is_file() {
        let asset = format!("bpftrace-{suffix}");
        let url = format!("https://github.com/{REPO}/releases/download/v{VERSION}/{asset}");
        let tmp = dir.join(".download.tmp");
        progress.line(&format!("Fetching bpftrace v{VERSION} ({suffix})"));
        if !crate::builder::busybox::fetch(&url, &tmp) || !crate::util::is_elf(&tmp) {
            let _ = std::fs::remove_file(&tmp);
            anyhow::bail!(
                "bpftrace {asset} download failed ({url}).\n  \
                 check network/proxy, or manually place the cache file {appimage}",
                appimage = appimage.display()
            );
        }
        std::fs::rename(&tmp, &appimage)?;
        progress.line("bpftrace: downloaded");
    }

    progress.line("bpftrace: extracting AppImage (container) ...");
    extract_to_tar(&dir, &appimage, &tar)
        .context("bpftrace: failed to extract AppImage in container (components.bpf needs docker, same dependency as kernel provisioning)")?;
    progress.line(&format!("bpftrace: v{VERSION} ready"));
    Ok(tar)
}

/// Installs into the tools disk staging: cached tar unpacked → `/tools/nix`, wrapper → `/tools/bin/bpftrace`.
pub(crate) fn install(tools_dir: &Path, tar: &Path, progress: &mut Progress) -> anyhow::Result<()> {
    let status = Command::new("tar")
        .args(["-xzf"])
        .arg(tar)
        .arg("-C")
        .arg(tools_dir)
        .status()
        .context("host tar unavailable")?;
    anyhow::ensure!(
        status.success(),
        "bpftrace: failed to unpack cached tar ({})",
        tar.display()
    );
    let bin = store_bin(tools_dir).context("bpftrace: store binary not found in extracted tree")?;
    let rel = bin.strip_prefix(tools_dir)?;
    std::fs::create_dir_all(tools_dir.join("bin"))?;
    let wrapper = tools_dir.join("bin/bpftrace");
    std::fs::write(&wrapper, wrapper_script(&format!("/tools/{}", rel.display())))?;
    crate::util::set_executable(&wrapper)?;
    progress.line("bpftrace: /tools/nix + /tools/bin/bpftrace");
    Ok(())
}

/// The real binary inside the extracted tree (nix/store/<hash>-bpftrace/bin/bpftrace).
fn store_bin(tools_dir: &Path) -> Option<PathBuf> {
    let store = tools_dir.join("nix/store");
    let mut entries: Vec<_> = std::fs::read_dir(&store).ok()?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        if name.to_str()?.ends_with("-bpftrace") {
            let bin = e.path().join("bin/bpftrace");
            if bin.is_file() {
                return Some(bin);
            }
        }
    }
    None
}

/// Wrapper: bootstraps two links inside the VM, then runs the store binary directly.
/// `/dev/fd` — devtmpfs does not create this conventional link; the clang
/// frontend writes /dev/fd/*;
/// `/nix` — the Nix closure's interpreter/RPATH are absolute paths; the
/// closure lands at /tools/nix with the tools disk.
fn wrapper_script(bin_in_tools: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # bpftrace v{VERSION} (components.bpf) — Nix closure runs directly from the tools disk\n\
         [ -e /dev/fd ] || ln -sfn /proc/self/fd /dev/fd 2>/dev/null\n\
         [ -e /nix ] || ln -sfn /tools/nix /nix 2>/dev/null\n\
         exec {bin_in_tools} \"$@\"\n"
    )
}

/// Extraction inside the container: the AppImage is copied into the overlay
/// (case-sensitive) and fully extracted; `tar cz` streams the nix/ subtree
/// back to the host via stdout — the Nix tree reaches macOS only as a tar
/// byte stream; collision pairs are merged at load time by host tar per
/// APFS semantics.
fn extract_to_tar(dir: &Path, appimage: &Path, tar_out: &Path) -> anyhow::Result<()> {
    let script = "\
set -e
mkdir -p /x
cp /src/appimage /x/appimage
chmod 755 /x/appimage
cd /x
./appimage --appimage-extract >/dev/null
rm -f appimage
tar cz -C squashfs-root nix
";
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{}:/src/appimage:ro", appimage.display()),
            EXTRACT_IMAGE,
            "sh",
            "-c",
            script,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("docker unavailable: {e}"))?;
    let mut stdout = child.stdout.take().context("docker stdout unreadable")?;
    let tmp = dir.join(".tree.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    std::io::copy(&mut stdout, &mut f)?;
    let out = child.wait()?;
    let mut err = String::new();
    if let Some(mut e) = child.stderr {
        let _ = e.read_to_string(&mut err);
    }
    anyhow::ensure!(
        out.success(),
        "docker extraction failed:\n{}",
        err.trim()
    );
    let size = tmp.metadata()?.len();
    anyhow::ensure!(size > 10 * 1024 * 1024, "docker extraction output too small ({size}B)");
    std::fs::rename(&tmp, tar_out)?;
    Ok(())
}
