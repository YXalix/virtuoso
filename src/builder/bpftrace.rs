//! bpftrace 供给（[components.bpf]，默认关）：GitHub Release 官方 AppImage
//! → 下载缓存 → 容器内解包成 Nix 闭包树 tar → tools 盘 `/tools/nix` 直跑。
//!
//! Release 资产（`bpftrace-aarch64` 等）是 AppImage：ELF runtime + 追加的
//! squashfs 载荷，载荷内是动态链接 LLVM/clang 的 Nix 闭包 —— 官方所称
//! "statically built" 指自含闭包，并非静态 ELF。VM 无 FUSE 裸跑不了；
//! 解包走容器：`--appimage-extract` 是纯用户态解包，不依赖 FUSE/内核
//! squashfs。
//!
//! 解包必须在**容器 overlay**（Linux ext4，大小写敏感）里完成、以单文件
//! tar 流回宿主：Nix 树含 Eterm/eterm 等大小写碰撞对（ncurses terminfo），
//! 直接解到 macOS 宿主目录会在 OrbStack virtiofs 上以 EACCES 中途夭折。
//! 碰撞对仅剩装饰性条目在装载时合并（host tar 落 APFS 静默同名覆盖），
//! 不影响 bpftrace 运行。符号链接经 tar 原样保留、mke2fs -d 原样进 ext4；
//! Nix 绝对路径符号链接（interpreter/RPATH = /nix/store/...）在 VM 内由
//! wrapper `ln -s /tools/nix /nix` 兑现：零运行时解包、零 tmpfs。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow};

use crate::Arch;
use crate::util::Progress;

pub(crate) const VERSION: &str = "0.27.0";
const REPO: &str = "bpftrace/bpftrace";
/// 解包容器镜像：只需 sh + tar + 静态 AppImage runtime，alpine 足够
/// （`docker run` 缺本地镜像时自动拉取）。
const EXTRACT_IMAGE: &str = "alpine:latest";

/// Release 资产的架构后缀（官方只发 aarch64/x86_64）。
fn release_arch(arch: Arch) -> anyhow::Result<&'static str> {
    match arch {
        Arch::Arm64 => Ok("aarch64"),
        Arch::X86_64 => Ok("x86_64"),
        Arch::Riscv64 => Err(anyhow!(
            "bpftrace 官方 Release 无 riscv64 资产，components.bpf 暂不支持该架构"
        )),
    }
}

/// 供给缓存根：`target/build/bpftrace/bpftrace-<version>/`（版本进缓存键）。
fn cache_dir(build_dir: &Path) -> PathBuf {
    build_dir.join("bpftrace").join(format!("bpftrace-{VERSION}"))
}

/// 确保解包树缓存就绪，返回缓存 tar 路径。失败即 Err（构建中止）。
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
                "bpftrace {asset} 下载失败（{url}）。\n  \
                 检查网络/代理，或手动放置缓存 {appimage}",
                appimage = appimage.display()
            );
        }
        std::fs::rename(&tmp, &appimage)?;
        progress.line("bpftrace: downloaded");
    }

    progress.line("bpftrace: extracting AppImage (container) ...");
    extract_to_tar(&dir, &appimage, &tar)
        .context("bpftrace: AppImage 容器内解包失败（components.bpf 需要 docker，与 kernel 供给同依赖）")?;
    progress.line(&format!("bpftrace: v{VERSION} ready"));
    Ok(tar)
}

/// 装入 tools 盘 staging：缓存 tar 解开 → `/tools/nix`，wrapper → `/tools/bin/bpftrace`。
pub(crate) fn install(tools_dir: &Path, tar: &Path, progress: &mut Progress) -> anyhow::Result<()> {
    let status = Command::new("tar")
        .args(["-xzf"])
        .arg(tar)
        .arg("-C")
        .arg(tools_dir)
        .status()
        .context("host tar 不可用")?;
    anyhow::ensure!(
        status.success(),
        "bpftrace: 缓存 tar 解包失败（{}）",
        tar.display()
    );
    let bin = store_bin(tools_dir).context("bpftrace: 解包树内未找到 store 二进制")?;
    let rel = bin.strip_prefix(tools_dir)?;
    std::fs::create_dir_all(tools_dir.join("bin"))?;
    let wrapper = tools_dir.join("bin/bpftrace");
    std::fs::write(&wrapper, wrapper_script(&format!("/tools/{}", rel.display())))?;
    crate::util::set_executable(&wrapper)?;
    progress.line("bpftrace: /tools/nix + /tools/bin/bpftrace");
    Ok(())
}

/// 解包树内真二进制（nix/store/<hash>-bpftrace/bin/bpftrace）。
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

/// wrapper：VM 内自举两个链接后直跑 store 二进制。
/// `/dev/fd` —— devtmpfs 不建此约定链接，clang 前端要写 /dev/fd/*；
/// `/nix` —— Nix 闭包的 interpreter/RPATH 是绝对路径，闭包随 tools 盘
/// 落位 /tools/nix。
fn wrapper_script(bin_in_tools: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # bpftrace v{VERSION}（components.bpf）—— Nix 闭包随 tools 盘直跑\n\
         [ -e /dev/fd ] || ln -sfn /proc/self/fd /dev/fd 2>/dev/null\n\
         [ -e /nix ] || ln -sfn /tools/nix /nix 2>/dev/null\n\
         exec {bin_in_tools} \"$@\"\n"
    )
}

/// 容器内解包：AppImage 复制进 overlay（大小写敏感）解全量，`tar cz` 把
/// nix/ 子树从 stdout 流回宿主 —— Nix 树只经 tar 字节流触达 macOS，
/// 碰撞对在装载期由 host tar 落盘时按 APFS 语义合并。
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
        .map_err(|e| anyhow!("docker 不可用：{e}"))?;
    let mut stdout = child.stdout.take().context("docker stdout 不可读")?;
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
        "docker 解包失败：\n{}",
        err.trim()
    );
    let size = tmp.metadata()?.len();
    anyhow::ensure!(size > 10 * 1024 * 1024, "docker 解包产物过小（{size}B）");
    std::fs::rename(&tmp, tar_out)?;
    Ok(())
}
