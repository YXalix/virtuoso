//! `virtuoso kernel`: containerized kernel provisioning (forge command projection).
//!
//! This module only projects config and wires processes: the container toolchain
//! workflow (clone/defconfig/build/shell) plus volume management and current
//! switching (list/use); the logic lives in forge. The output is a host-visible
//! kernel tree (`virtuoso kernel path`); pointing kernel_path at it enters the
//! host-native test main loop — the verdict pipeline is unaware of the provisioning
//! mode. Source editing goes through the VS Code devcontainer (in-container clangd
//! consumes the /ksrc raw-form compile_commands.json produced by build).

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
            full,
        } => run_clone(
            &url,
            ref_name.as_deref(),
            as_volume.as_deref(),
            arch.as_deref(),
            full,
        ),
        KernelAction::Defconfig { name, arch } => run_defconfig(name.as_deref(), arch.as_deref()),
        KernelAction::Build { jobs, arch } => run_build(jobs, arch.as_deref()),
        KernelAction::Path { volume } => run_path(volume.as_deref()),
        KernelAction::Shell { arch } => run_shell(arch.as_deref()),
        KernelAction::List => run_list(),
        KernelAction::Use { volume, arch } => run_use(&volume, arch.as_deref()),
    }
}

// ---------------------------------------------------------------- projection helpers

/// Arch resolution: CLI --arch > env KERNEL_ARCH > resolve_arch (top-level arch > host default).
/// Build the arch you test: the default naturally matches virtuoso.toml's arch.
fn kernel_arch(cfg: &Config, cli_arch: Option<&str>) -> anyhow::Result<Arch> {
    if let Some(a) = cli_arch {
        return Arch::parse(a).ok_or_else(|| anyhow::anyhow!("unknown arch {a}"));
    }
    if let Some(v) = crate::util::env_nonempty("KERNEL_ARCH") {
        return Arch::parse(v.trim())
            .ok_or_else(|| anyhow::anyhow!("invalid KERNEL_ARCH: {v} (arm64|x86_64|riscv64)"));
    }
    resolve_arch(cfg, None)
}

/// Active volume resolution: env KERNEL_VOLUME > the current state file > default volume
/// (the two-level rule lives in forge::active_volume).
fn current_volume(cfg: &Config) -> anyhow::Result<String> {
    Ok(crate::forge::active_volume(&cfg.project_root)?
        .unwrap_or_else(|| crate::forge::DEFAULT_VOLUME.to_string()))
}

/// Clone ref: CLI --ref > env KERNEL_REF > default master.
fn clone_ref(cli_ref: Option<&str>) -> String {
    cli_ref
        .map(str::to_string)
        .or_else(|| crate::util::env_nonempty("KERNEL_REF"))
        .unwrap_or_else(|| crate::forge::DEFAULT_REF.to_string())
}

fn dockerfile_dir(cfg: &Config) -> PathBuf {
    cfg.project_root.join("devkit").join("docker")
}

// ---------------------------------------------------------------- workflow commands

fn run_clone(
    url: &str,
    cli_ref: Option<&str>,
    as_volume: Option<&str>,
    cli_arch: Option<&str>,
    full: bool,
) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    // Clone always targets a fresh volume: --as > env KERNEL_VOLUME > default —
    // the current state file is deliberately not consulted (clone_kernel
    // refuses an already-populated volume anyway).
    let volume = as_volume
        .map(str::to_string)
        .or_else(crate::forge::env_volume)
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
            image: &crate::forge::resolve_image(),
            dockerfile_dir: &dockerfile_dir(&cfg),
            full,
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
    println!(
        "next: build/test now follow this volume; a fresh clone has no Image yet → virtuoso kernel defconfig && virtuoso kernel build"
    );
    Ok(0)
}

fn run_defconfig(name: Option<&str>, cli_arch: Option<&str>) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let volume = current_volume(&cfg)?;
    let arch = kernel_arch(&cfg, cli_arch)?;
    let target = name.unwrap_or("defconfig");
    println!(
        "Kernel: make {target} (volume {volume}, arch {})",
        arch.name()
    );
    crate::forge::run_streaming(
        &volume,
        &crate::forge::resolve_image(),
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
        "Kernel: make -j{jobs} {image_target} modules + static bpftool (volume {volume}, arch {})",
        arch.name()
    );
    // A kernel make failure fails the whole build (|| exit $?); a bpftool failure only
    // WARNs (see forge::bpftool_step) — the control-plane tool does not block the
    // kernel build itself.
    let script = format!(
        "make -j{jobs} {image_target} modules || exit $?\n{}",
        crate::forge::bpftool_step(jobs, arch)
    );
    crate::forge::run_streaming(
        &volume,
        &crate::forge::resolve_image(),
        &crate::forge::make_env(arch),
        &script,
    )?;
    crate::forge::cdb_generate(&volume, &crate::forge::resolve_image())?;
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
        &crate::forge::resolve_image(),
        &crate::forge::make_env(arch),
        &["/bin/bash"],
    )
}

// ---------------------------------------------------------------- volume management commands

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
    // --arch overrides the recorded arch (KERNEL_ARCH/env/top-level arch are resolved
    // uniformly by kernel_arch)
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
    println!(
        "next: build/test now follow this volume (toml kernel_path only applies without an active volume)\n      tree without an Image yet → virtuoso kernel defconfig && virtuoso kernel build"
    );
    Ok(0)
}
