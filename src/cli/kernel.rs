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

use crate::builder::{kconfig, modconf};
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
        KernelAction::CheckConfig { arch } => run_check_config(arch.as_deref()),
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

// ---------------------------------------------------------------- check-config

/// Loaded kernel-tree facts for check-config (Makefile version + parsed .config).
struct KernelTreeInfo {
    version: (u32, u32, u32),
    symbols: std::collections::BTreeMap<String, kconfig::Tristate>,
}

/// Reads and validates the tree's Makefile version and .config.
fn load_kernel_tree(tree: &std::path::Path) -> anyhow::Result<KernelTreeInfo> {
    let makefile = tree.join("Makefile");
    let makefile_text = std::fs::read_to_string(&makefile).map_err(|_| {
        anyhow::anyhow!(
            "{} is not a kernel tree (no Makefile) — run `virtuoso kernel clone <url>` first",
            tree.display()
        )
    })?;
    let version = kconfig::version_from_makefile(&makefile_text)
        .ok_or_else(|| anyhow::anyhow!("cannot parse kernel version from {}", makefile.display()))?;

    let dot_config = tree.join(".config");
    let conf_text = std::fs::read_to_string(&dot_config).map_err(|_| {
        anyhow::anyhow!(
            "no .config in {} — run `virtuoso kernel defconfig` (or copy a config in) first",
            tree.display()
        )
    })?;
    Ok(KernelTreeInfo {
        version,
        symbols: kconfig::parse_dot_config(&conf_text),
    })
}

/// The single mapping from a table component name (Group::Component) to its
/// virtuoso.toml enablement state; unknown groups (hooks) stay informational.
fn component_enabled(cfg: &Config, name: &str) -> bool {
    match name {
        "agent" => cfg.agent_enabled(),
        "vfio" => cfg.vfio().is_some(),
        "pmem" => cfg.pmem_size().is_some(),
        "bpf" => cfg.bpf_enabled(),
        "numa" => cfg.numa_enabled(),
        _ => false,
    }
}

/// " → a.ko + b.ko" suffix for a requirement row (empty for bool symbols).
fn module_suffix(req: &kconfig::Req) -> String {
    if req.modules.is_empty() {
        return String::new();
    }
    let kos = req
        .modules
        .iter()
        .map(|m| format!("{m}.ko"))
        .collect::<Vec<_>>()
        .join(" + ");
    format!(" → {kos}")
}

/// Prints the findings grouped by Boot/Component; returns (failures, warnings).
fn render_findings(findings: &[kconfig::Finding], cfg: &Config) -> (usize, usize) {
    let mut failures = 0usize;
    let mut warnings = 0usize;
    let mut last_group = None;
    for f in findings {
        if last_group != Some(f.req.group) {
            last_group = Some(f.req.group);
            match f.req.group {
                kconfig::Group::Boot => println!("  Boot-critical (reach root= + console):"),
                kconfig::Group::Component(name) => {
                    let state = if component_enabled(cfg, name) { "enabled" } else { "off" };
                    println!("  Component [{name}] ({state}):");
                }
            }
        }
        let sym = format!("CONFIG_{}", f.req.symbol);
        let module = module_suffix(f.req);
        match f.status {
            kconfig::Status::Builtin => println!("    ✓ {sym}=y{module}"),
            kconfig::Status::Module => println!("    ✓ {sym}=m{module}"),
            kconfig::Status::NotYet => println!("    – {sym} n/a — {}", f.req.note),
            kconfig::Status::Missing => {
                let group_enabled = match f.req.group {
                    kconfig::Group::Component(name) => component_enabled(cfg, name),
                    kconfig::Group::Boot => false,
                };
                let need = match f.req.need {
                    kconfig::Need::Builtin => " (must be =y)",
                    kconfig::Need::BuiltinOrModule => "",
                };
                if f.is_failure(group_enabled) {
                    failures += 1;
                    println!("    ✗ {sym} not set{need} — {}", f.req.note);
                } else {
                    warnings += 1;
                    println!("    ! {sym} not set{need} — {}", f.req.note);
                }
            }
        }
    }
    (failures, warnings)
}

/// Boot manifest (infra/modules-boot.conf + component stage="boot" extras) vs
/// the tree — mirrors the builder's resolution: in-tree .ko → builtin
/// (skipped) → error. Skipped on unbuilt trees (no modules.order yet).
/// Returns the missing-module count.
fn check_boot_manifest(
    cfg: &Config,
    tree: &std::path::Path,
    version: (u32, u32, u32),
) -> anyhow::Result<usize> {
    let plan = cfg.component_plan();
    let (names, _) = modconf::boot_set(&cfg.infra_dir.join("modules-boot.conf"), &plan.boot_extra)?;
    let built = tree.join("modules.order").is_file() || tree.join("modules.builtin").is_file();
    println!();
    if !built {
        println!("Boot manifest: tree not built yet (no modules.order) — skipping .ko availability check");
        return Ok(0);
    }
    println!("Boot manifest (infra/modules-boot.conf + stage=boot requires) vs tree:");
    let builtin = modconf::builtin_modules(tree);
    // Exclude the virtuoso repo when it sits inside the kernel tree (default
    // layout): target/build staging holds .ko copies from previous builds
    // that must not satisfy the check.
    let exclude = (cfg.project_root != tree && cfg.project_root.starts_with(tree))
        .then_some(cfg.project_root.as_path());
    let kos = modconf::collect_kos_excluding(tree, exclude);
    let mut failures = 0usize;
    for name in &names {
        if builtin.contains(name) {
            println!("    ✓ {name}: builtin (skipped at build)");
            continue;
        }
        if let Some(ko) = kos.get(name) {
            let rel = ko.strip_prefix(tree).unwrap_or(ko);
            println!("    ✓ {name}.ko: {}", rel.display());
            continue;
        }
        failures += 1;
        let hint = match kconfig::req_for_module(name) {
            Some(req) => match req.since {
                Some(s) if kconfig::predates(version, s) => format!(
                    "CONFIG_{} does not exist before {}.{} — remove the entry from infra/modules-boot.conf for this kernel",
                    req.symbol, s.0, s.1
                ),
                _ => format!(
                    "CONFIG_{} is not =m in this tree — enable it (or run `virtuoso kernel build`), or drop the entry",
                    req.symbol
                ),
            },
            None => "not found in the tree — enable its CONFIG_*=m or drop the entry".to_string(),
        };
        println!("    ✗ {name}.ko missing — {hint}");
    }
    Ok(failures)
}

/// `kernel check-config`: evaluates the active kernel tree's .config against
/// the Kconfig requirement table (builder::kconfig) and cross-checks the boot
/// module manifest against the tree — the user-queryable surface for "which
/// switches does Virtuoso need, and are they on in my kernel".
fn run_check_config(cli_arch: Option<&str>) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let arch = kernel_arch(&cfg, cli_arch)?;
    let (tree, source) = cfg.kernel_path()?;
    let info = load_kernel_tree(&tree)?;

    println!("Kernel tree: {} ({source})", tree.display());
    println!(
        "Kernel version: {}.{}.{}   arch: {}",
        info.version.0,
        info.version.1,
        info.version.2,
        arch.name()
    );
    println!();
    println!("Kconfig requirements (reference: docs/usage/kernel-config.md):");

    let findings = kconfig::evaluate(&info.symbols, info.version, arch);
    let (failures, warnings) = render_findings(&findings, &cfg);
    let manifest_failures = check_boot_manifest(&cfg, &tree, info.version)?;

    println!();
    let gaps = failures + manifest_failures;
    if gaps == 0 {
        println!("check-config: OK ({warnings} warning(s)) — kernel {}.{}.{} satisfies Virtuoso requirements for {}", info.version.0, info.version.1, info.version.2, arch.name());
        Ok(0)
    } else {
        println!("check-config: {gaps} gap(s), {warnings} warning(s) — see docs/usage/kernel-config.md");
        Ok(1)
    }
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
