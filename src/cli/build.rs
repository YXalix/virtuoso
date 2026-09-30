//! 构建与资产管理命令：build / clean / skill。
//! 实际构建逻辑在 builder，本模块只做配置投影与进程接线。

use std::path::Path;

use super::resolve_arch;
use crate::config::{Config, KernelPathSource};
use crate::SkillAction;

pub fn run_build(busybox_only: bool) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    if busybox_only {
        // 仅备当前架构静态 BusyBox（四层供给链，builder 接管）
        let arch = resolve_arch(&cfg, None)?;
        let supply = cfg.busybox_supply();
        let mut progress = crate::util::Progress::stdout();
        crate::builder::busybox::ensure(&cfg.build_dir, arch, &supply, &mut progress)?;
        return Ok(0);
    }
    build_pair_for(&cfg, None, &[]).map(|_| 0)
}

/// 构建两段式引导对（test 路径以 run 目录 build.log 承载进度输出）。
/// `extra_runtime` 为调用方强制追加的 runtime 模块条目（probe 恒开
/// agent 通道，强制并入 virtio_console，不依赖组件开关）。
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
        &cfg.infra_dir,
        &cfg.build_dir,
        &cfg.artifacts_dir,
        &kernel_path,
        arch,
        &supply,
        &[],
        &crate::builder::modconf::Modules {
            boot_extra: plan.boot_extra,
            runtime: plan.runtime,
        },
        &cfg.project_root.join("rootfs.d"),
        cfg.bpf_enabled(),
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
    // 暂存目录与组装产物（busybox/bpftrace 缓存保留，重下/重解包代价高）
    for d in ["initramfs", "rootfs", "tools"] {
        let p = cfg.build_dir.join(d);
        if p.is_dir() {
            std::fs::remove_dir_all(&p)?;
            println!("removed: {}/", p.display());
            removed += 1;
        }
    }
    // 用例 workspace 构建缓存（cargo target；含改名后遗留的旧产物）
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
    // 安装目标不自动探测（项目根上一级这种缺省树未必是想装入的树）；env
    // KERNEL_PATH、toml kernel_path 与 current 活动卷都是明确指定。
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
                // fs::copy 在 macOS 走 fclonefileat，OrbStack 卷视图不支持克隆
                // 语义（EPERM）——SKILL.md 是文本，读写两步绕开。
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
