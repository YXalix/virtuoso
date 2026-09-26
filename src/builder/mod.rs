//! builder — 构建器。Phase 2 从 `build-initrd.sh` / `fetch-busybox.sh`
//! 接管：BusyBox 供给、C 用例编译、initramfs/rootfs 组装（行为等价移植，
//! 消息文本与退出码语义对齐 shell 基线）。

pub(crate) mod bpftrace;
pub(crate) mod busybox;
pub(crate) mod cargo_install;
pub(crate) mod cpio;
pub(crate) mod cross;
pub(crate) mod image;
pub(crate) mod modconf;
pub(crate) mod testcase;
pub(crate) mod tools;
pub(crate) mod verify;

use std::path::Path;

use anyhow::Context;

use crate::util::Progress;

/// VM 内 init 的声明式注入钩子：片段插入 mount 之后、insmod 之前。
/// 对应 rootfs 内的 `/init-hooks.sh`（缺省不存在，init 侧有守卫 source）。
#[derive(Debug, Clone)]
pub(crate) struct InitHook {
    pub name: String,
    pub script: String,
}

impl InitHook {
    pub(crate) fn shell(name: impl Into<String>, script: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            script: script.into(),
        }
    }
}

/// tools.img 挂载 hook（rootfs 内 `/init-hooks.sh` 的生成片段）：init 在
/// devtmpfs 挂载后、insmod/agent 拉起前 source——virtio_blk 已由 initramfs
/// 的 modules-boot.conf 加载，此处直接挂 /dev/vdb 并把工具目录注入 PATH。
const TOOLS_DISK_HOOK: &str = r#"mkdir -p /tools
if mount -t ext4 /dev/vdb /tools 2>/dev/null; then
    export PATH="/tools/bin:$PATH"
else
    LOG_WARN "tools disk not mounted (/dev/vdb missing or not ext4); tools unavailable"
fi"#;

/// 构建两段式引导对（build-initrd.sh 的 Rust 接管）：
/// `target/artifacts/initrd.img`（initramfs：busybox + modules-boot.conf
/// 基础集 + 组件 boot 附加 + init-initramfs）与 `target/artifacts/rootfs.img`
/// （ext4：busybox + 组件 require 生成的 modules.conf + init + /tests）。
/// 另产出 `target/artifacts/tools.img`（ext4：/bin 常驻工具，VM 内挂 /tools）——
/// 工具被跳过时不产出。源资产读 `infra_dir`，暂存目录与 busybox 缓存放
/// `build_dir`（target/build）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_boot_pair(
    infra_dir: &Path,
    build_dir: &Path,
    artifacts_dir: &Path,
    kernel_path: &Path,
    arch: crate::Arch,
    supply: &busybox::Supply,
    hooks: &[InitHook],
    modules: &modconf::Modules,
    rootfs_d_dir: &Path,
    bpf_enabled: bool,
    progress: &mut Progress,
) -> anyhow::Result<()> {
    // 与脚本一致的先决检查
    if !kernel_path.join("arch").is_dir() {
        anyhow::bail!(
            "Cannot find kernel source directory at {}\nSet KERNEL_PATH environment variable to specify the location:\n  KERNEL_PATH=/path/to/kernel virtuoso build",
            kernel_path.display()
        );
    }
    if image::find_mke2fs().is_none() {
        anyhow::bail!("mke2fs not found (Linux: e2fsprogs package; macOS: brew install e2fsprogs)");
    }
    std::fs::create_dir_all(build_dir)?;
    std::fs::create_dir_all(artifacts_dir)?;

    // 交叉接线（全宿主：CC 通道供 C 测试体；LINKER/rustflags 仅非 Linux）
    let cross_setup = cross::setup(arch, build_dir)?;
    if let Some(note) = &cross_setup.note {
        progress.line(note);
    }
    let busybox_bin = busybox::ensure(build_dir, arch, supply, progress)?;
    let version = supply
        .version
        .clone()
        .unwrap_or_else(|| busybox::DEFAULT_VERSION.into());
    let applets = busybox::applet_names(infra_dir, &version, &busybox_bin)?;

    // ---------- initrd.img: minimal initramfs ----------
    progress.line("Building initrd.img (minimal initramfs)...");
    let initramfs_dir = build_dir.join("initramfs");
    assemble_busybox_tree(&initramfs_dir, &busybox_bin, &applets)?;
    std::fs::copy(infra_dir.join("init-initramfs"), initramfs_dir.join("init"))
        .context("copy init-initramfs failed")?;
    crate::util::set_executable(&initramfs_dir.join("init"))?;
    std::fs::create_dir_all(initramfs_dir.join("mnt"))?;
    std::fs::create_dir_all(initramfs_dir.join("lib/modules"))?;
    let (boot_names, boot_conf_text) =
        modconf::boot_set(&infra_dir.join("modules-boot.conf"), &modules.boot_extra)?;
    modconf::copy_module_list(
        &initramfs_dir.join("lib/modules"),
        &boot_names,
        &boot_conf_text,
        "modules-boot.conf",
        kernel_path,
        infra_dir,
        progress,
    )?;
    crate::util::if_changed(
        &initramfs_dir,
        "cpio pack_dir_gzip",
        &artifacts_dir.join("initrd.img"),
        "initrd.img",
        progress,
        || cpio::pack_dir_gzip(&initramfs_dir, &artifacts_dir.join("initrd.img")),
    )?;

    // ---------- rootfs.img: ext4 rootfs with tests ----------
    progress.line("Building rootfs.img (ext4 rootfs)...");
    let rootfs_dir = build_dir.join("rootfs");
    assemble_busybox_tree(&rootfs_dir, &busybox_bin, &applets)?;
    std::fs::copy(infra_dir.join("init"), rootfs_dir.join("init")).context("copy init failed")?;
    crate::util::set_executable(&rootfs_dir.join("init"))?;
    std::fs::create_dir_all(rootfs_dir.join("lib/modules"))?;
    let (runtime_names, runtime_conf): (Vec<String>, String) = (
        modules
            .runtime
            .iter()
            .map(|l| modconf::module_name(l).to_string())
            .filter(|n| !n.is_empty())
            .collect(),
        modconf::runtime_conf_text(&modules.runtime),
    );
    modconf::copy_module_list(
        &rootfs_dir.join("lib/modules"),
        &runtime_names,
        &runtime_conf,
        "modules.conf",
        kernel_path,
        infra_dir,
        progress,
    )?;
    testcase::install_rust(
        &infra_dir.join("testcases"),
        &rootfs_dir.join("tests"),
        arch,
        &cross_setup,
        progress,
    )?;
    // tools workspace（常驻工具）→ tools.img 的 /bin：与用例分类正交，见
    // tools::install。装入成功才产出 tools.img 并注入挂载 hook（降级语义）。
    let tools_dir = build_dir.join("tools");
    let tools_installed = tools::install(
        &infra_dir.join("tools"),
        &tools_dir.join("bin"),
        arch,
        &cross_setup,
        progress,
    )?;
    // components.bpf（默认关）：bpftrace 官方 AppImage → 下载缓存 + 容器
    // 解包（需要 docker，与 kernel 供给同依赖）→ 解包树进 tools 盘 /tools/nix，
    // wrapper 落 /tools/bin/bpftrace（PATH 已由 tools 盘 hook 注入）。
    let bpf_tar = if bpf_enabled {
        Some(bpftrace::ensure(build_dir, arch, progress)?)
    } else {
        None
    };
    if let Some(tar) = &bpf_tar {
        bpftrace::install(&tools_dir, tar, progress)?;
    }
    let tools_installed = tools_installed || bpf_tar.is_some();
    let mut hooks = hooks.to_vec();
    if tools_installed {
        crate::util::if_changed(
            &tools_dir,
            "mke2fs ext4 -b 4096 -O ^has_journal",
            &artifacts_dir.join("tools.img"),
            "tools.img",
            progress,
            || image::make_ext4(&tools_dir, &artifacts_dir.join("tools.img"), "tools"),
        )?;
        hooks.push(InitHook::shell("tools-disk", TOOLS_DISK_HOOK));
    }
    write_hooks(&rootfs_dir, &hooks)?;
    apply_rootfs_d(&rootfs_dir, rootfs_d_dir, progress)?;
    crate::util::if_changed(
        &rootfs_dir,
        "mke2fs ext4 -b 4096 -O ^has_journal",
        &artifacts_dir.join("rootfs.img"),
        "rootfs.img",
        progress,
        || image::make_ext4(&rootfs_dir, &artifacts_dir.join("rootfs.img"), "rootfs"),
    )?;

    progress.line("");
    progress.line("Done:");
    let mut built = vec!["initrd.img", "rootfs.img"];
    if tools_installed {
        built.push("tools.img");
    }
    for f in built {
        let p = artifacts_dir.join(f);
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        progress.line(&format!("  {} {}", f, crate::util::human_size_ls(size)));
    }
    Ok(())
}

/// busybox 用户land 组装：静态二进制 + applet 符号链接 + 骨架目录 + root 账户。
/// 符号链接由名单驱动（`busybox::applet_names`），不执行 guest 二进制 ——
/// 交叉组装（含 macOS 宿主）无需宿主同构。
pub(crate) fn assemble_busybox_tree(
    dest: &Path,
    busybox_bin: &Path,
    applets: &[String],
) -> anyhow::Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    for d in [
        "bin",
        "sbin",
        "usr/bin",
        "usr/sbin",
        "proc",
        "sys",
        "dev",
        "tmp",
        "mnt",
        "etc/init.d",
        "var/run",
        "root",
    ] {
        std::fs::create_dir_all(dest.join(d))?;
    }
    std::fs::copy(busybox_bin, dest.join("bin/busybox"))?;
    crate::util::set_executable(&dest.join("bin/busybox"))?;

    // applet → bin/<name> 相对符号链接（目标恒 "busybox"，无需事后改写）
    for name in applets {
        let link = dest.join("bin").join(name);
        if link.exists() || link.is_symlink() {
            continue;
        }
        std::os::unix::fs::symlink("busybox", &link)
            .with_context(|| format!("symlink {} failed", link.display()))?;
    }

    std::fs::write(dest.join("etc/passwd"), "root:x:0:0:root:/root:/bin/sh\n")?;
    std::fs::write(dest.join("etc/group"), "root:x:0:\n")?;
    Ok(())
}

fn write_hooks(rootfs_dir: &Path, hooks: &[InitHook]) -> anyhow::Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }
    let mut body = String::from("# generated by builder (InitHook)\n");
    for h in hooks {
        body.push_str(&format!("# --- hook: {} ---\n{}\n", h.name, h.script));
    }
    std::fs::write(rootfs_dir.join("init-hooks.sh"), body)?;
    Ok(())
}

/// rootfs.d 增量并入：仓库根 `rootfs.d/` 树映射到 rootfs 根（用户内容作为
/// 源随每次构建进入 rootfs——只增不覆盖）。目录级并集（同名目录自由合并），
/// 文件级必须新增：任何与组装产物同路径的文件都是构建期错误，没有覆盖
/// 语义。缺省（无此目录）静默跳过。返回并入的文件数。
pub(crate) fn apply_rootfs_d(
    rootfs_dir: &Path,
    rootfs_d_dir: &Path,
    progress: &mut Progress,
) -> anyhow::Result<usize> {
    if !rootfs_d_dir.is_dir() {
        return Ok(0);
    }
    let mut count = 0usize;
    merge_tree(rootfs_dir, rootfs_d_dir, rootfs_d_dir, &mut count)?;
    // 空目录与缺失同义（doctor 会建出空目录）：静默，不打 0 行噪音
    if count > 0 {
        progress.line(&format!("rootfs.d: {count} file(s) added"));
    }
    Ok(count)
}

fn merge_tree(
    dest_root: &Path,
    base: &Path,
    dir: &Path,
    count: &mut usize,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let src = entry.path();
        let rel = src.strip_prefix(base).context("rootfs.d: strip prefix failed")?;
        let dst = dest_root.join(rel);
        let rel_display = rel.display();
        if src.is_dir() {
            if dst.exists() && !dst.is_dir() {
                anyhow::bail!(
                    "rootfs.d: directory {rel_display} collides with a file in the assembled rootfs"
                );
            }
            std::fs::create_dir_all(&dst)?;
            merge_tree(dest_root, base, &src, count)?;
        } else if dst.exists() {
            anyhow::bail!(
                "rootfs.d: {rel_display} already exists in the assembled rootfs \
                 (additions only — builder-assembled paths cannot be shadowed)"
            );
        } else {
            std::fs::copy(&src, &dst).with_context(|| format!("copy {} failed", src.display()))?;
            *count += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("builder-rd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mkfile(path: &std::path::Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn rootfs_d_merges_dirs_and_adds_files() {
        let staging = tmp("merge-staging");
        let add = tmp("merge-add");
        mkfile(&staging.join("etc/passwd"), "base");
        mkfile(&staging.join("tests/test-example"), "bin");
        mkfile(&add.join("tests/selfcheck/run.sh"), "#!/bin/sh\nexit 0");
        mkfile(&add.join("etc/motd"), "hello");
        mkfile(&add.join("root/profile"), "export FOO=1");
        let mut progress = Progress::stdout();
        let n = apply_rootfs_d(&staging, &add, &mut progress).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            std::fs::read_to_string(staging.join("tests/selfcheck/run.sh")).unwrap(),
            "#!/bin/sh\nexit 0"
        );
        assert_eq!(std::fs::read_to_string(staging.join("etc/motd")).unwrap(), "hello");
        // 同名目录合并不破坏既有文件
        assert_eq!(std::fs::read_to_string(staging.join("etc/passwd")).unwrap(), "base");
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(&add);
    }

    #[test]
    fn rootfs_d_file_collision_bails() {
        let staging = tmp("coll-staging");
        let add = tmp("coll-add");
        mkfile(&staging.join("etc/passwd"), "base");
        mkfile(&add.join("etc/passwd"), "shadow");
        let mut progress = Progress::stdout();
        assert!(apply_rootfs_d(&staging, &add, &mut progress).is_err());
        // 失败即停：既有文件未被覆盖
        assert_eq!(std::fs::read_to_string(staging.join("etc/passwd")).unwrap(), "base");
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(&add);
    }

    #[test]
    fn rootfs_d_dir_over_file_bails() {
        let staging = tmp("dircoll-staging");
        let add = tmp("dircoll-add");
        mkfile(&staging.join("thing"), "file");
        std::fs::create_dir_all(add.join("thing")).unwrap();
        let mut progress = Progress::stdout();
        assert!(apply_rootfs_d(&staging, &add, &mut progress).is_err());
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(&add);
    }

    #[test]
    fn rootfs_d_missing_is_noop() {
        let staging = tmp("noop-staging");
        let mut progress = Progress::stdout();
        let n = apply_rootfs_d(&staging, &staging.join("nonexistent"), &mut progress).unwrap();
        assert_eq!(n, 0);
        let _ = std::fs::remove_dir_all(&staging);
    }
}
