//! `virtuoso kernel`：容器化内核供给（forge 命令投影）。
//!
//! 本模块只做配置投影与进程接线：容器工具链工作流（clone/defconfig/build/
//! shell）+ 卷管理与 current 切换（list/use），逻辑在 forge。产出 = 宿主可见
//! 内核树（`virtuoso kernel path`），kernel_path 指向它即进入宿主原生测试主
//! 循环——verdict 管线不感知供给模式。源码编辑走 VS Code devcontainer（容器
//! 内 clangd 吃 build 产出的 /ksrc 原始形态 compile_commands.json）。

use std::path::PathBuf;

use crate::Arch;

use crate::util::Progress;

use super::resolve_arch;
use crate::config::Config;
use crate::KernelAction;

pub fn run_kernel(action: KernelAction) -> anyhow::Result<i32> {
    match action {
        KernelAction::Clone {
            url,
            ref_name,
            as_volume,
            arch,
        } => run_clone(
            &url,
            ref_name.as_deref(),
            as_volume.as_deref(),
            arch.as_deref(),
        ),
        KernelAction::Defconfig { name, arch } => run_defconfig(name.as_deref(), arch.as_deref()),
        KernelAction::Build { jobs, arch } => run_build(jobs, arch.as_deref()),
        KernelAction::Path { volume } => run_path(volume.as_deref()),
        KernelAction::Shell { arch } => run_shell(arch.as_deref()),
        KernelAction::List => run_list(),
        KernelAction::Use { volume, arch } => run_use(&volume, arch.as_deref()),
    }
}

// ---------------------------------------------------------------- 投影 helpers

/// 架构解析：CLI --arch > env KERNEL_ARCH > resolve_arch（顶层 arch > 宿主缺省）。
/// —— 测哪个架构就编哪个架构：缺省与 virtuoso.toml 的 arch 天然一致。
fn kernel_arch(cfg: &Config, cli_arch: Option<&str>) -> anyhow::Result<Arch> {
    if let Some(a) = cli_arch {
        return Arch::parse(a).ok_or_else(|| anyhow::anyhow!("unknown arch {a}"));
    }
    if let Ok(v) = std::env::var("KERNEL_ARCH") {
        if !v.trim().is_empty() {
            return Arch::parse(v.trim())
                .ok_or_else(|| anyhow::anyhow!("invalid KERNEL_ARCH: {v} (arm64|x86_64|riscv64)"));
        }
    }
    resolve_arch(cfg, None)
}

/// 活动卷解析：env KERNEL_VOLUME > 状态文件 current > 缺省卷。
fn current_volume(cfg: &Config) -> anyhow::Result<String> {
    if let Ok(v) = std::env::var("KERNEL_VOLUME") {
        if !v.trim().is_empty() {
            return Ok(v);
        }
    }
    if let Some(cur) = crate::forge::read_current(&cfg.project_root)? {
        return Ok(cur.volume);
    }
    Ok(crate::forge::DEFAULT_VOLUME.to_string())
}

/// 工具链镜像：env KERNEL_TOOLCHAIN_IMAGE > ghcr 发布镜像（pull 失败回落本地
/// 构建 devkit/docker/Dockerfile.kernel）。
fn toolchain_image() -> String {
    crate::forge::resolve_image()
}

/// clone ref：CLI --ref > env KERNEL_REF > 缺省 master。
fn clone_ref(cli_ref: Option<&str>) -> String {
    cli_ref
        .map(str::to_string)
        .or_else(|| {
            std::env::var("KERNEL_REF")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| crate::forge::DEFAULT_REF.to_string())
}

fn dockerfile_dir(cfg: &Config) -> PathBuf {
    cfg.project_root.join("devkit").join("docker")
}

// ---------------------------------------------------------------- 工作流命令

fn run_clone(
    url: &str,
    cli_ref: Option<&str>,
    as_volume: Option<&str>,
    cli_arch: Option<&str>,
) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let volume = as_volume
        .map(str::to_string)
        .or_else(|| {
            std::env::var("KERNEL_VOLUME")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| crate::forge::DEFAULT_VOLUME.to_string());
    let arch = kernel_arch(&cfg, cli_arch)?;
    let ref_name = clone_ref(cli_ref);
    let mut progress = Progress::stdout();
    let view = crate::forge::clone_kernel(
        &crate::forge::CloneJob {
            project_root: &cfg.project_root,
            volume_name: &volume,
            arch,
            url,
            ref_name: &ref_name,
            image: &toolchain_image(),
            dockerfile_dir: &dockerfile_dir(&cfg),
        },
        &mut progress,
    )?;
    println!(
        "Kernel: cloned {url}@{ref_name} → volume {volume} (.clangd rendered for {})",
        arch.name()
    );
    println!("Kernel: current → {volume} ({})", arch.name());
    println!("Host-visible path (AI/editor cwd): {}", view.display());
    println!(
        "Kernel: devcontainer rendered → .devcontainer/ (open this repo in VS Code → Reopen in Container to enter /ksrc)"
    );
    println!("next: virtuoso kernel defconfig && virtuoso kernel build");
    Ok(0)
}

fn run_defconfig(name: Option<&str>, cli_arch: Option<&str>) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let volume = current_volume(&cfg)?;
    let arch = kernel_arch(&cfg, cli_arch)?;
    let target = name.unwrap_or("defconfig");
    println!(
        "Kernel: make {target}（volume {volume}，arch {}）",
        arch.name()
    );
    crate::forge::run_streaming(
        &volume,
        &toolchain_image(),
        &crate::forge::make_env(arch),
        &format!("make {}", crate::util::quote(target)),
    )?;
    Ok(0)
}

fn run_build(jobs: Option<usize>, cli_arch: Option<&str>) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let volume = current_volume(&cfg)?;
    let arch = kernel_arch(&cfg, cli_arch)?;
    let jobs = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8)
    });
    let image_target = crate::forge::make_image_target(arch);
    println!(
        "Kernel: make -j{jobs} {image_target} modules + static bpftool（volume {volume}，arch {}）",
        arch.name()
    );
    // kernel make 失败即整体失败（|| exit $?）；bpftool 失败只 WARN（见
    // forge::bpftool_step）——控制面工具不挡内核构建本身。
    let script = format!(
        "make -j{jobs} {image_target} modules || exit $?\n{}",
        crate::forge::bpftool_step(jobs, arch)
    );
    crate::forge::run_streaming(
        &volume,
        &toolchain_image(),
        &crate::forge::make_env(arch),
        &script,
    )?;
    crate::forge::cdb_generate(&volume, &toolchain_image())?;
    println!(
        "Kernel: built {image_target} + modules; compile_commands.json generated (raw form under /ksrc, consumed by clangd in the devcontainer)"
    );
    Ok(0)
}

fn run_path(volume_flag: Option<&str>) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let volume = match volume_flag {
        Some(v) => v.to_string(),
        None => current_volume(&cfg)?,
    };
    let view = crate::forge::host_view(&volume)?;
    if !view.is_dir() {
        if std::env::consts::OS == "macos" {
            anyhow::bail!(
                "{} is unreachable — OrbStack is not installed or not running (the view only exists while OrbStack runs).\n  Start OrbStack and retry.",
                view.display()
            );
        }
        anyhow::bail!("volume {volume} does not exist or is unreachable — run `virtuoso kernel clone <git-url>` first.");
    }
    println!("{}", view.display());
    Ok(0)
}

fn run_shell(cli_arch: Option<&str>) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let volume = current_volume(&cfg)?;
    let arch = kernel_arch(&cfg, cli_arch)?;
    crate::forge::run_tty(
        &volume,
        &toolchain_image(),
        &crate::forge::make_env(arch),
        &["/bin/bash"],
    )
}

// ---------------------------------------------------------------- 卷管理命令

fn run_list() -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let volumes = crate::forge::list()?;
    if volumes.is_empty() {
        println!("(no volumes yet — create one with `virtuoso kernel clone <git-url>`)");
        return Ok(0);
    }
    let current = crate::forge::read_current(&cfg.project_root)?;
    for v in volumes {
        let view = crate::forge::host_view(&v).ok();
        let label = match view.as_ref().map(|p| crate::forge::status(p)) {
            Some(s) => s.label(),
            None => "unreachable",
        };
        let mark = if current.as_ref().is_some_and(|c| c.volume == v) {
            "*"
        } else {
            " "
        };
        let note = match current.as_ref() {
            Some(c) if c.volume == v => format!("← current (arch {})", c.arch),
            _ => String::new(),
        };
        println!("{mark} {v:<32} {label:<12} {}", note);
    }
    Ok(0)
}

fn run_use(volume: &str, cli_arch: Option<&str>) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    anyhow::ensure!(
        crate::forge::exists(volume),
        "volume {volume} does not exist (see `virtuoso kernel list`, or create it with `virtuoso kernel clone <git-url> --as {volume}`)"
    );
    // --arch 覆盖记录的架构（KERNEL_ARCH/env/顶层 arch 由 kernel_arch 统一解析）
    let arch = kernel_arch(&cfg, cli_arch)?;
    crate::forge::write_current(
        &cfg.project_root,
        &crate::forge::Current {
            volume: volume.to_string(),
            arch: arch.name().to_string(),
        },
    )?;
    println!("Kernel: current → {volume} ({})", arch.name());
    println!(
        "Kernel: devcontainer rendered → .devcontainer/ (open this repo in VS Code → Reopen in Container to enter /ksrc)"
    );
    println!("next: virtuoso kernel path  # host-visible path for kernel_path (QEMU consumes it)");
    Ok(0)
}
