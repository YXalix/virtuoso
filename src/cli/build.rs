//! Build and asset management commands: build / clean / skill.
//! Actual build logic lives in builder; this module only projects config and wires processes.

use std::path::Path;

use super::resolve_arch;
use crate::config::{Config, KernelPathSource};
use crate::SkillAction;

pub fn run_build(busybox_only: bool) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    if busybox_only {
        // Prepare only the static BusyBox for the current arch (four-layer provisioning
        // chain; builder takes over)
        let arch = resolve_arch(&cfg, None)?;
        let supply = cfg.busybox_supply();
        let mut progress = crate::util::Progress::stdout();
        crate::builder::busybox::ensure(&cfg.build_dir, arch, &supply, &mut progress)?;
        return Ok(0);
    }
    build_pair_for(&cfg, None, &[]).map(|_| 0)
}

/// Build the two-stage boot pair (the test path carries progress output in the run
/// directory's build.log). `extra_runtime` holds runtime module entries the caller
/// forces in (probe always enables the agent channel, forcibly merging in
/// virtio_console without relying on component switches).
pub(crate) fn build_pair_for(
    cfg: &Config,
    log_path: Option<&Path>,
    extra_runtime: &[String],
) -> anyhow::Result<()> {
    let arch = resolve_arch(cfg, None)?;
    let (kernel_path, _) = cfg.kernel_path()?;
    let supply = cfg.busybox_supply();
    let mut plan = cfg.component_plan();
    for line in extra_runtime {
        if !plan.runtime.iter().any(|e| e == line) {
            plan.runtime.push(line.clone());
        }
    }
    let mut progress = match log_path {
        Some(p) => crate::util::Progress::with_log(p)?,
        None => crate::util::Progress::stdout(),
    };
    progress.line("Rebuilding initrd.img + rootfs.img (two-stage boot pair)...");
    crate::builder::build_boot_pair(
        crate::builder::BootPairInputs {
            infra_dir: &cfg.infra_dir,
            build_dir: &cfg.build_dir,
            artifacts_dir: &cfg.artifacts_dir,
            kernel_path: &kernel_path,
            arch,
            supply: &supply,
            modules: crate::builder::modconf::Modules {
                boot_extra: plan.boot_extra,
                runtime: plan.runtime,
            },
            rootfs_d_dir: &cfg.project_root.join("rootfs.d"),
            bpf_enabled: cfg.bpf_enabled(),
        },
        &mut progress,
    )
}

pub fn run_clean() -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let mut removed = 0usize;
    for f in [
        "initrd.img",
        "initrd.img.stamp",
        "rootfs.img",
        "rootfs.img.stamp",
        "tools.img",
        "tools.img.stamp",
    ] {
        let p = cfg.artifacts_dir.join(f);
        if p.is_file() {
            std::fs::remove_file(&p)?;
            println!("removed: {}", p.display());
            removed += 1;
        }
    }
    // Staging directories and assembled artifacts (busybox/bpftrace caches are kept:
    // re-downloading and re-unpacking them is expensive)
    for d in ["initramfs", "rootfs", "tools"] {
        let p = cfg.build_dir.join(d);
        if p.is_dir() {
            std::fs::remove_dir_all(&p)?;
            println!("removed: {}/", p.display());
            removed += 1;
        }
    }
    // Test case workspace build cache (cargo target; includes stale leftovers from renames)
    let testcases_target = cfg.infra_dir.join("testcases/target");
    if testcases_target.is_dir() {
        std::fs::remove_dir_all(&testcases_target)?;
        println!("removed: {}/", testcases_target.display());
        removed += 1;
    }
    println!("clean done: {removed} item(s) removed (busybox/bpftrace cache kept)",);
    Ok(0)
}

pub fn run_skill(action: SkillAction) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    // The install target is not auto-detected (a default tree such as the parent of
    // the project root is not necessarily the tree to install into); env KERNEL_PATH,
    // toml kernel_path, and the current active volume are all explicit specifications.
    let (kernel_path, source) = cfg.kernel_path()?;
    anyhow::ensure!(
        source != KernelPathSource::AutoDetect,
        "KERNEL_PATH is not set.\n  Set kernel_path in virtuoso.toml, or switch the active volume with `virtuoso kernel use <volume>`."
    );
    let kernel_path = kernel_path.display().to_string();

    match action {
        SkillAction::Install => {
            println!("Installing kernel-dev + kernel-virtuoso skills for Claude Code...");
            for skill in ["kernel-dev", "kernel-virtuoso"] {
                let dst_dir = Path::new(&kernel_path).join(".claude/skills").join(skill);
                std::fs::create_dir_all(&dst_dir)?;
                let src = cfg
                    .project_root
                    .join("devkit")
                    .join("skills")
                    .join(skill)
                    .join("SKILL.md");
                // fs::copy uses fclonefileat on macOS, and OrbStack volume views do not
                // support clone semantics (EPERM) — SKILL.md is text, so a read/write
                // two-step works around it.
                let body = std::fs::read_to_string(&src)?;
                std::fs::write(dst_dir.join("SKILL.md"), body)?;
                println!("  installed: {skill}");
            }
            println!(
                "Done. Claude Code will now recognize the skills when running from {kernel_path}/"
            );
            Ok(0)
        }
        SkillAction::Uninstall => {
            for skill in ["kernel-dev", "kernel-virtuoso"] {
                let dst = Path::new(&kernel_path).join(".claude/skills").join(skill);
                if dst.is_dir() {
                    std::fs::remove_dir_all(&dst)?;
                    println!("Removed: {}", dst.display());
                } else {
                    println!("Skill not installed at {}", dst.display());
                }
            }
            Ok(0)
        }
    }
}
