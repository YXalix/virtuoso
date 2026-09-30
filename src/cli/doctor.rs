//! `virtuoso doctor`：环境体检唯一入口。检查引擎复用
//! crate::builder::verify::run_checks（语义单一来源，引擎输入投影 engine_report
//! 在本文件），两种呈现：缺省 = flutter-doctor 风格一屏（引擎检查按消息
//! 前缀归并为组件行，✓/✗/! 一眼可读）；`--verbose` = 类型化配置诊断 +
//! 完整检查清单。退出码：critical 未过 → 1。

use std::path::Path;

use crate::util::which;
use crate::Arch;

use crate::builder::verify::{detail, CheckKind, Level, Report, RootfsDState};

use super::resolve_arch;
use crate::config::Config;

// ---------------------------------------------------------------- 一屏分组

/// doctor 一屏呈现（flutter-doctor 风格）：引擎检查按 CheckKind 归并为
/// 组件组行，✓/✗/! 一眼可读。分组唯一依据是 CheckKind（引擎自带），无
/// 消息文本反解；Pass/Info 行取引擎预计算的 summary 紧凑短语，Fail/Warn
/// 行取 label 剥离后的全文。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Config,
    Toolchain,
    Kernel,
    Qemu,
    Modules,
    Artifacts,
}

impl Group {
    fn name(self) -> &'static str {
        match self {
            Group::Config => "Config",
            Group::Toolchain => "Toolchain",
            Group::Kernel => "Kernel",
            Group::Qemu => "QEMU",
            Group::Modules => "Modules",
            Group::Artifacts => "Artifacts",
        }
    }
}

/// 固定呈现顺序。
const GROUP_ORDER: &[Group] = &[
    Group::Config,
    Group::Toolchain,
    Group::Kernel,
    Group::Qemu,
    Group::Modules,
    Group::Artifacts,
];

/// 引擎检查主题 → 一屏分组（穷尽匹配：引擎新增 kind 时此处编译期报错）。
fn group_of(kind: CheckKind) -> Group {
    match kind {
        CheckKind::Config => Group::Config,
        CheckKind::HostTools | CheckKind::CrossCompile => Group::Toolchain,
        CheckKind::KernelPath
        | CheckKind::KernelSource
        | CheckKind::KernelImage
        | CheckKind::KernelDocker => Group::Kernel,
        CheckKind::QemuBinary | CheckKind::QemuImg => Group::Qemu,
        CheckKind::Modules | CheckKind::Components => Group::Modules,
        CheckKind::BusyBox | CheckKind::ToolsImage | CheckKind::Initrd | CheckKind::RootfsD => {
            Group::Artifacts
        }
    }
}

/// 组内最严重级别（Info 视同 Pass：可选工具缺失不降级）。
fn worst(a: Level, b: Level) -> Level {
    let rank = |l: Level| match l {
        Level::Fail => 3,
        Level::Warn => 2,
        Level::Pass => 1,
        Level::Info => 0,
    };
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

#[derive(Debug, PartialEq)]
struct GroupOut {
    group: Group,
    level: Level,
    /// (级别, 文本)：Pass/Info 为紧凑短语（合并渲染），Fail/Warn 为全文（逐行）。
    lines: Vec<(Level, String)>,
}

fn slot(out: &mut [GroupOut], g: Group) -> &mut GroupOut {
    out.iter_mut()
        .find(|s| s.group == g)
        .expect("GROUP_ORDER 覆盖全部分组")
}

fn group_checks(report: &Report) -> Vec<GroupOut> {
    let mut out: Vec<GroupOut> = GROUP_ORDER
        .iter()
        .map(|g| GroupOut {
            group: *g,
            level: Level::Info,
            lines: Vec::new(),
        })
        .collect();
    for chk in &report.checks {
        let s = slot(&mut out, group_of(chk.kind));
        s.level = worst(s.level, chk.level);
        match chk.level {
            Level::Pass | Level::Info => {
                if let Some(short) = &chk.summary {
                    if !s.lines.iter().any(|(_, l)| l == short) {
                        s.lines.push((chk.level, short.clone()));
                    }
                }
            }
            Level::Fail | Level::Warn => {
                s.lines.push((
                    chk.level,
                    detail(chk.kind, &chk.msg).to_string(),
                ));
            }
        }
    }
    out.retain(|s| !s.lines.is_empty());
    out
}

fn render(groups: &[GroupOut], tty: bool) -> String {
    let c = |code: &str, s: &str| {
        if tty {
            crate::util::paint(code, s)
        } else {
            s.to_string()
        }
    };
    let mut out = String::new();
    for g in groups {
        let (icon, code) = match g.level {
            Level::Fail => ("✗", "0;31"),
            Level::Warn => ("!", "0;33"),
            _ => ("✓", "0;32"),
        };
        let mut rows: Vec<String> = Vec::new();
        let bad: Vec<&String> = g
            .lines
            .iter()
            .filter(|(l, _)| matches!(l, Level::Fail | Level::Warn))
            .map(|(_, t)| t)
            .collect();
        let good = g
            .lines
            .iter()
            .filter(|(l, _)| matches!(l, Level::Pass | Level::Info))
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        if !bad.is_empty() {
            rows.extend(bad.iter().map(|s| (*s).clone()));
            if !good.is_empty() {
                rows.push(good);
            }
        } else {
            rows.push(good);
        }
        let name = format!("{:<11}", g.group.name());
        out.push_str(&format!(
            "  {} {}  {}\n",
            c(code, icon),
            c("1", name.as_str()),
            rows[0]
        ));
        for row in &rows[1..] {
            // 与首行正文列对齐：2 缩进 + 图标 1 + 空格 + 组名 11 + 2 空格
            out.push_str(&format!("{:<17}{}\n", "", row));
        }
    }
    out
}

// ---------------------------------------------------------------- 引擎输入投影

/// 检查引擎输入投影（保证检查语义单一来源——新增前置条件只动
/// crate::builder::verify::run_checks，doctor 的两种呈现自动跟随）。
fn engine_report(cfg: &Config, arch: Arch, rootfs_d: RootfsDState) -> anyhow::Result<Report> {
    let host_arch = Arch::parse(std::env::consts::ARCH);
    // 解析失败（如活动卷不可达）不遮蔽其余检查——docker_report 会给出细节
    let (kernel_path, kernel_path_source) = match cfg.kernel_path() {
        Ok((kp, src)) => (Some(kp), Some(src.to_string())),
        Err(_) => (None, None),
    };
    let supply = cfg.busybox_supply();

    let kernel_img = kernel_path.as_ref().map(|p| p.join(arch.kernel_img()));
    let plan = cfg.component_plan();
    let module_lines: Vec<String> = plan.all().cloned().collect();
    let modules = crate::builder::verify::module_presence(&module_lines, kernel_path.as_deref(), &cfg.infra_dir);

    Ok(crate::builder::verify::run_checks(&crate::builder::verify::CheckInput {
        config_file_exists: cfg.toml.is_some(),
        kernel_path: kernel_path.as_deref(),
        kernel_path_source: kernel_path_source.as_deref(),
        arch,
        host: crate::HostOs::current(),
        host_is_cross: host_arch.is_some_and(|h| h != arch),
        kernel_image: kernel_img.as_deref(),
        qemu_bin: which(arch.qemu_bin()).then_some(arch.qemu_bin()),
        qemu_override: cfg.qemu_override().as_deref(),
        modules: &modules,
        busybox_cached: crate::builder::busybox::cache_bin(
            &cfg.build_dir,
            crate::builder::busybox::effective_version(&supply),
            arch,
        )
        .is_file(),
        tools_img_exists: cfg.artifacts_dir.join("tools.img").is_file(),
        initrd: Some(&cfg.artifacts_dir.join("initrd.img")),
        vfio_enabled: cfg.vfio().is_some(),
        pmem_enabled: cfg.pmem_size().is_some(),
        rootfs_d,
    }))
}

/// docker 供给模式附加检查（forge 活动卷）：状态文件存在 = 活动卷开启，
/// 才投影 forge 检查——raw 用户无状态文件，零打扰。
fn docker_report(cfg: &Config, report: &mut Report) {
    let Ok(Some(current)) = crate::forge::read_current(&cfg.project_root) else {
        return;
    };
    let volume = std::env::var("KERNEL_VOLUME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(current.volume);
    let engine_err = crate::forge::engine_guard().err().map(|e| e.to_string());
    let host_view = crate::forge::host_view(&volume).ok();
    let image = std::env::var(crate::forge::IMAGE_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| crate::forge::DEFAULT_IMAGE.to_string());
    let image_present = crate::forge::image_present(&image);
    report.extend(crate::builder::verify::kernel_docker_checks(
        &volume,
        &current.arch,
        engine_err.as_deref(),
        host_view.as_deref(),
        image_present,
        &image,
    ));
}

/// rootfs.d 缺失即建（空目录）：drop-in 目录不随 clone 分发，doctor 是新人
/// 必经入口，建出后由引擎检查就地解释（verify::check_rootfs_d）。创建失败
/// 不拦体检——降级为 WARN 呈现，退出码不受影响。
fn ensure_rootfs_d(dir: &Path) -> RootfsDState {
    if let Ok(md) = std::fs::metadata(dir) {
        return if md.is_dir() {
            RootfsDState::Ready {
                files: count_files(dir),
                created: false,
            }
        } else {
            RootfsDState::NotADir
        };
    }
    match std::fs::create_dir_all(dir) {
        Ok(()) => RootfsDState::Ready {
            files: 0,
            created: true,
        },
        Err(e) => RootfsDState::Absent {
            err: Some(e.to_string()),
        },
    }
}

/// rootfs.d 将并入的文件数（递归；与 builder 的整树并入同口径）。
fn count_files(dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            n += count_files(&p);
        } else {
            n += 1;
        }
    }
    n
}

// ---------------------------------------------------------------- 入口

pub fn run_doctor(arch_override: Option<&str>, json: bool, verbose: bool) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let arch = resolve_arch(&cfg, arch_override)?;
    let rootfs_d = ensure_rootfs_d(&cfg.project_root.join("rootfs.d"));
    let mut report = engine_report(&cfg, arch, rootfs_d)?;
    docker_report(&cfg, &mut report);

    // --verbose：全量呈现（类型化配置诊断 + 完整检查清单），文本态专属
    if verbose && !json {
        super::diagnostics::print_diagnostics(&cfg, arch_override);
        print!("{}", report.render());
        println!();
        if report.critical_fail > 0 {
            println!("  Fix the issues above, then re-run: virtuoso doctor --verbose");
        } else {
            println!("  Ready. Run: virtuoso test --timeout 30");
        }
        return Ok(i32::from(report.critical_fail > 0));
    }

    let groups = group_checks(&report);

    let fail_total = report.critical_fail;
    if json {
        let level = |l: Level| match l {
            Level::Fail => "fail",
            Level::Warn => "warn",
            _ => "pass",
        };
        let out = serde_json::json!({
            "arch": arch.name(),
            "ok": fail_total == 0,
            "critical_fail": fail_total,
            "warnings": report.warnings,
            "groups": groups
                .iter()
                .map(|g| serde_json::json!({
                    "name": g.group.name(),
                    "level": level(g.level),
                    "details": g.lines.iter().map(|(_, t)| t).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        let tty = crate::builder::verify::is_stdout_tty();
        let (pass_icon, fail_icon) = (
            crate::util::icon(crate::util::Icon::Pass),
            crate::util::icon(crate::util::Icon::Fail),
        );
        println!("Virtuoso doctor · arch {}", arch.name());
        print!("{}", render(&groups, tty));
        if fail_total > 0 {
            println!(
                "\n  {} {fail_total} critical, {} warnings — full checklist: virtuoso doctor --verbose",
                fail_icon,
                report.warnings
            );
        } else {
            let suggest = match cfg.timeout_raw() {
                t if t.trim() == "0" => "virtuoso test --timeout 60".to_string(),
                t => format!("virtuoso test --timeout {}", t.trim()),
            };
            println!("\n  {pass_icon} Ready — {suggest}");
        }
    }
    Ok(i32::from(fail_total > 0))
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::verify::Check;

    fn check(level: Level, kind: CheckKind, msg: &str, summary: Option<&str>) -> Check {
        Check {
            level,
            kind,
            msg: msg.into(),
            summary: summary.map(str::to_string),
        }
    }

    fn report(checks: Vec<Check>) -> Report {
        let critical_fail = checks.iter().filter(|c| c.level == Level::Fail).count() as u32;
        let warnings = checks.iter().filter(|c| c.level == Level::Warn).count() as u32;
        Report {
            checks,
            critical_pass: 0,
            critical_fail,
            warnings,
        }
    }

    #[test]
    fn group_of_covers_every_kind() {
        // 穷尽性由 group_of 的 match 保证（新增 kind 编译期报错）；
        // 这里钉死每个 kind 的预期分组，防止无意挪组破坏一屏布局。
        for (kind, want) in [
            (CheckKind::Config, Group::Config),
            (CheckKind::HostTools, Group::Toolchain),
            (CheckKind::CrossCompile, Group::Toolchain),
            (CheckKind::KernelPath, Group::Kernel),
            (CheckKind::KernelSource, Group::Kernel),
            (CheckKind::KernelImage, Group::Kernel),
            (CheckKind::KernelDocker, Group::Kernel),
            (CheckKind::QemuBinary, Group::Qemu),
            (CheckKind::QemuImg, Group::Qemu),
            (CheckKind::Modules, Group::Modules),
            (CheckKind::Components, Group::Modules),
            (CheckKind::BusyBox, Group::Artifacts),
            (CheckKind::ToolsImage, Group::Artifacts),
            (CheckKind::Initrd, Group::Artifacts),
            (CheckKind::RootfsD, Group::Artifacts),
        ] {
            assert_eq!(group_of(kind), want, "kind={kind:?}");
        }
    }

    #[test]
    fn worst_orders_fail_over_warn_over_pass() {
        assert_eq!(worst(Level::Info, Level::Pass), Level::Pass);
        assert_eq!(worst(Level::Pass, Level::Warn), Level::Warn);
        assert_eq!(worst(Level::Warn, Level::Fail), Level::Fail);
    }

    #[test]
    fn group_checks_fails_lead_and_warn_keeps_full_text() {
        let groups = group_checks(&report(vec![
            check(
                Level::Pass,
                CheckKind::Config,
                "Configuration: virtuoso.toml found",
                Some("virtuoso.toml found"),
            ),
            check(
                Level::Fail,
                CheckKind::QemuBinary,
                "QEMU binary: qemu-system-arm not found (install qemu-system-arm)",
                None,
            ),
            check(
                Level::Warn,
                CheckKind::Modules,
                "Kernel modules: 2/3 found, missing: nd_btt",
                None,
            ),
            check(
                Level::Pass,
                CheckKind::KernelImage,
                "Kernel image: Image (42M)",
                Some("Image (42M)"),
            ),
        ]));
        let qemu = groups.iter().find(|g| g.group == Group::Qemu).unwrap();
        assert_eq!(qemu.level, Level::Fail);
        assert_eq!(
            qemu.lines,
            vec![(
                Level::Fail,
                "qemu-system-arm not found (install qemu-system-arm)".into()
            )]
        );
        let modules = groups.iter().find(|g| g.group == Group::Modules).unwrap();
        assert_eq!(modules.level, Level::Warn);
        assert_eq!(modules.lines[0].1, "2/3 found, missing: nd_btt");
    }

    #[test]
    fn group_checks_healthy_report_is_one_line_per_group() {
        let groups = group_checks(&report(vec![
            check(
                Level::Pass,
                CheckKind::Config,
                "Configuration: virtuoso.toml found",
                Some("virtuoso.toml found"),
            ),
            check(
                Level::Pass,
                CheckKind::HostTools,
                "Host tools: all found (wget tar gcc)",
                Some("host tools"),
            ),
            check(Level::Pass, CheckKind::KernelPath, "KERNEL_PATH: /k", None),
            check(
                Level::Pass,
                CheckKind::KernelSource,
                "Kernel source: /k (v6.6.0)",
                Some("v6.6.0"),
            ),
            check(
                Level::Pass,
                CheckKind::KernelImage,
                "Kernel image: Image (42M)",
                Some("Image (42M)"),
            ),
            check(
                Level::Pass,
                CheckKind::QemuBinary,
                "QEMU binary: qemu-system-aarch64",
                Some("qemu-system-aarch64"),
            ),
            check(Level::Info, CheckKind::QemuImg, "qemu-img: available", None),
            check(
                Level::Info,
                CheckKind::Modules,
                "Kernel modules: none required",
                Some("none required"),
            ),
            check(
                Level::Pass,
                CheckKind::BusyBox,
                "BusyBox: cached (arm64)",
                Some("busybox"),
            ),
            check(
                Level::Info,
                CheckKind::ToolsImage,
                "Tools image: exists (attached as /dev/vdb)",
                Some("tools.img"),
            ),
            check(
                Level::Info,
                CheckKind::Initrd,
                "Initrd: /a/initrd.img (12M)",
                Some("initrd.img (12M)"),
            ),
            check(
                Level::Info,
                CheckKind::RootfsD,
                "rootfs.d: 2 file(s) (drop-in dir; merged into rootfs at build (add-only))",
                Some("rootfs.d (2 files)"),
            ),
        ]));
        let rendered = render(&groups, false);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 6, "六个组件组各一行:\n{rendered}");
        assert!(lines[0].contains("✓") && lines[0].contains("Config"));
        assert!(lines[2].contains("Kernel") && lines[2].contains("v6.6.0 · Image (42M)"));
        assert!(
            lines[5].contains("Artifacts")
                && lines[5].contains("busybox · tools.img · initrd.img (12M) · rootfs.d (2 files)")
        );
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("doctor-rd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn ensure_rootfs_d_creates_missing_then_counts() {
        let d = tmp("create");
        assert!(matches!(
            ensure_rootfs_d(&d),
            RootfsDState::Ready {
                files: 0,
                created: true
            }
        ));
        std::fs::write(d.join("a.txt"), "x").unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub").join("b.sh"), "y").unwrap();
        assert!(matches!(
            ensure_rootfs_d(&d),
            RootfsDState::Ready {
                files: 2,
                created: false
            }
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ensure_rootfs_d_bails_on_non_dir() {
        let d = tmp("notdir");
        std::fs::write(&d, "not a dir").unwrap();
        assert!(matches!(ensure_rootfs_d(&d), RootfsDState::NotADir));
        let _ = std::fs::remove_dir_all(&d);
    }
}
