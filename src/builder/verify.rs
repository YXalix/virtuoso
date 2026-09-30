//! 前置检查引擎（verify.sh 的 Rust 接管）。输出文本与退出码语义对齐 shell
//! 基线：critical FAIL → exit 1；WARN/INFO 不影响退出码。
//!
//! doctor 的两种呈现（一屏分组 / --verbose 全量）都消费本模块的 Report：
//! 每条 Check 自带 `kind`（分组）与 `summary`（一屏紧凑短语，None = 不进
//! 一屏）——新增前置条件只动引擎，呈现自动跟随，doctor 不做消息文本反解。
//!
//! 分节：类型与呈现（Check/CheckKind/Level/Report）→ 引擎输入与编排
//! （CheckInput/run_checks）→ 逐项检查函数。

use std::path::Path;

use crate::{Arch, HostOs};

use crate::builder::modconf;

// ---------------------------------------------------------------- 类型与呈现

/// 检查主题（doctor 一屏分组的唯一依据；grouping 与 label 同源，无文本反解）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckKind {
    Config,
    HostTools,
    CrossCompile,
    KernelPath,
    KernelSource,
    KernelImage,
    KernelDocker,
    QemuBinary,
    QemuImg,
    Modules,
    Components,
    BusyBox,
    ToolsImage,
    Initrd,
    RootfsD,
}

impl CheckKind {
    /// 消息标签（msg 以 "<label>: " 开头；doctor 剥前缀取正文）。
    pub(crate) fn label(self) -> &'static str {
        match self {
            CheckKind::Config => "Configuration",
            CheckKind::HostTools => "Host tools",
            CheckKind::CrossCompile => "Cross-compile",
            CheckKind::KernelPath => "KERNEL_PATH",
            CheckKind::KernelSource => "Kernel source",
            CheckKind::KernelImage => "Kernel image",
            CheckKind::KernelDocker => "Kernel docker",
            CheckKind::QemuBinary => "QEMU binary",
            CheckKind::QemuImg => "qemu-img",
            CheckKind::Modules => "Kernel modules",
            CheckKind::Components => "Components",
            CheckKind::BusyBox => "BusyBox",
            CheckKind::ToolsImage => "Tools image",
            CheckKind::Initrd => "Initrd",
            CheckKind::RootfsD => "rootfs.d",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Pass,
    Fail,
    Warn,
    Info,
}

#[derive(Debug)]
pub(crate) struct Check {
    pub level: Level,
    pub kind: CheckKind,
    pub msg: String,
    /// 一屏紧凑短语（Pass/Info 用；None = 不进一屏，如纯可选信息行）。
    pub summary: Option<String>,
}

/// msg 的 "<label>: " 前缀之后正文（Fail/Warn 逐行修复提示）。
pub(crate) fn detail(kind: CheckKind, msg: &str) -> &str {
    msg.strip_prefix(kind.label())
        .map(|r| r.trim_start_matches([':', ' ']))
        .unwrap_or(msg)
}

fn check(
    level: Level,
    kind: CheckKind,
    msg: impl Into<String>,
    summary: Option<String>,
) -> Check {
    Check {
        level,
        kind,
        msg: msg.into(),
        summary,
    }
}
fn pass(kind: CheckKind, msg: impl Into<String>, summary: Option<String>) -> Check {
    check(Level::Pass, kind, msg, summary)
}
fn fail(kind: CheckKind, msg: impl Into<String>) -> Check {
    check(Level::Fail, kind, msg, None)
}
fn warn(kind: CheckKind, msg: impl Into<String>) -> Check {
    check(Level::Warn, kind, msg, None)
}
fn info(kind: CheckKind, msg: impl Into<String>, summary: Option<String>) -> Check {
    check(Level::Info, kind, msg, summary)
}

pub(crate) struct Report {
    pub checks: Vec<Check>,
    pub critical_pass: u32,
    pub critical_fail: u32,
    pub warnings: u32,
}

impl Report {
    /// 追加检查并重算计数（doctor 附加 docker 供给组用）。
    pub(crate) fn extend(&mut self, extra: Vec<Check>) {
        self.checks.extend(extra);
        self.recount();
    }

    fn recount(&mut self) {
        self.critical_pass = self
            .checks
            .iter()
            .filter(|c| c.level == Level::Pass)
            .count() as u32;
        self.critical_fail = self
            .checks
            .iter()
            .filter(|c| c.level == Level::Fail)
            .count() as u32;
        self.warnings = self
            .checks
            .iter()
            .filter(|c| c.level == Level::Warn)
            .count() as u32;
    }

    /// 渲染（tty 下着色；--verbose 全量清单）。
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&crate::util::bold("[DOCTOR] QEMU E2E Prerequisites Check"));
        out.push_str("\n========================================\n");
        for chk in &self.checks {
            let (tag, code) = match chk.level {
                Level::Pass => ("[PASS]", "0;32"),
                Level::Fail => ("[FAIL]", "0;31"),
                Level::Warn => ("[WARN]", "0;33"),
                Level::Info => ("[INFO]", "0;36"),
            };
            out.push_str(&format!("  {} {}\n", crate::util::paint(code, tag), chk.msg));
        }
        out
    }
}

/// 仅影响颜色，不影响判定（doctor 呈现层共用同一 NO_COLOR 语义）。
pub(crate) fn is_stdout_tty() -> bool {
    crate::util::tty()
}

// ---------------------------------------------------------------- 引擎输入与编排

/// rootfs.d drop-in 状态（目录不随 clone 分发；doctor 负责缺失即创建，
/// 见 cli::doctor::ensure_rootfs_d，本引擎只读状态）。
pub(crate) enum RootfsDState {
    /// 目录就绪：files = 将并入的文件数；created = 本次运行刚创建。
    Ready { files: usize, created: bool },
    /// 路径存在但不是目录（drop-in 不生效）。
    NotADir,
    /// 不存在（err = 创建失败原因；None = 调用方未尝试创建）。
    Absent { err: Option<String> },
}

/// 检查引擎输入（doctor / engine_report 投影；一次装配，免长参数表）。
pub(crate) struct CheckInput<'a> {
    pub config_file_exists: bool,
    pub kernel_path: Option<&'a Path>,
    /// kernel_path 的解析来源（"env KERNEL_PATH" / "current volume X" /
    /// "config" / "auto-detected"），呈现用；None = 路径同样缺失。
    pub kernel_path_source: Option<&'a str>,
    pub arch: Arch,
    pub host: HostOs,
    pub host_is_cross: bool,
    pub kernel_image: Option<&'a Path>,
    pub qemu_bin: Option<&'a str>,
    pub qemu_override: Option<&'a str>,
    /// (模块名, .ko 是否找到)
    pub modules: &'a [(String, bool)],
    pub busybox_cached: bool,
    pub tools_img_exists: bool,
    pub initrd: Option<&'a Path>,
    pub vfio_enabled: bool,
    pub pmem_enabled: bool,
    pub rootfs_d: RootfsDState,
}

/// 运行全部检查（verify.sh 的 11 项 + 组件平台门 + rootfs.d drop-in），
/// 逐项独立成函数。
pub(crate) fn run_checks(input: &CheckInput) -> Report {
    let mut checks = Vec::new();
    check_config(input, &mut checks);
    check_host_tools(input, &mut checks);
    check_kernel_source(input, &mut checks);
    check_kernel_image(input, &mut checks);
    check_qemu(input, &mut checks);
    check_modules(input, &mut checks);
    check_busybox(input, &mut checks);
    check_cross(input, &mut checks);
    check_tools_image(input, &mut checks);
    check_initrd(input, &mut checks);
    check_components(input, &mut checks);
    check_rootfs_d(input, &mut checks);

    let critical_pass = checks.iter().filter(|c| c.level == Level::Pass).count() as u32;
    let critical_fail = checks.iter().filter(|c| c.level == Level::Fail).count() as u32;
    let warnings = checks.iter().filter(|c| c.level == Level::Warn).count() as u32;
    Report {
        checks,
        critical_pass,
        critical_fail,
        warnings,
    }
}

// ---------------------------------------------------------------- 逐项检查函数

/// 宿主工具表（平台无关）。make/sed 随 busybox 源码兜底退役；initrd 打包
/// 已原生化（builder::cpio），cpio/gzip/wget/nproc/timeout 不再是硬需求；
/// 下载层 wget 缺失时有 curl 回退。
/// testcases 全走 cargo（build.rs + cc）后 C 编译器统一 zig cc 包装
/// （builder::cross），cmake 随 CMake 路径退役。
pub(crate) fn host_tools() -> &'static [&'static str] {
    &["tar", "zig", "find"]
}

/// 下载工具（busybox 供给层）：wget 或 curl 任一（macOS 自带 curl）。
fn fetch_tool_ok() -> bool {
    crate::util::which("wget") || crate::util::which("curl")
}

/// 组件计划条目（conf 行）的模块是否都能在内核树找到（模块检查的输入收集）。
/// `kernel_path` 为 None（未配置内核树）时全部记为未找到。
pub(crate) fn module_presence(
    module_lines: &[String],
    kernel_path: Option<&Path>,
    fallback_dir: &Path,
) -> Vec<(String, bool)> {
    let names: Vec<&str> = module_lines
        .iter()
        .map(|line| modconf::module_name(line))
        .filter(|m| !m.is_empty())
        .collect();
    let kos = kernel_path
        .filter(|_| !names.is_empty())
        .map(modconf::collect_kos);
    names
        .into_iter()
        .map(|m| {
            let found = kos
                .as_ref()
                .map(|map| map.contains_key(m) || modconf::infra_ko(fallback_dir, m).is_some())
                .unwrap_or(false);
            (m.to_string(), found)
        })
        .collect()
}

// 1. Configuration
fn check_config(input: &CheckInput, checks: &mut Vec<Check>) {
    if input.config_file_exists {
        checks.push(pass(
            CheckKind::Config,
            "Configuration: virtuoso.toml found",
            Some("virtuoso.toml found".into()),
        ));
    } else {
        checks.push(fail(
            CheckKind::Config,
            "Configuration: virtuoso.toml not found (run `virtuoso` inside the project root, or restore the shipped template)",
        ));
    }
}

// 2. Host tools（平台无关表 + 下载/镜像/交叉工具链）
fn check_host_tools(input: &CheckInput, checks: &mut Vec<Check>) {
    let host = input.host;
    let tools = host_tools();
    let mut missing: Vec<String> = tools
        .iter()
        .filter(|t| !crate::util::which(t))
        .map(|t| (*t).to_string())
        .collect();
    if !fetch_tool_ok() {
        missing.push("wget|curl".into());
    }
    if crate::builder::image::find_mke2fs().is_none() {
        missing.push("mke2fs".into());
    }
    // C 用例编译器：Linux = 宿主 gcc/cc；macOS = CC env 或 zig（交叉）
    let cc_ok = if host == HostOs::Darwin {
        std::env::var_os("CC").is_some() || crate::util::which("zig")
    } else {
        ["gcc", "cc"].iter().any(|c| crate::util::which(c))
    };
    if !cc_ok {
        missing.push(if host == HostOs::Darwin {
            "zig (brew install zig) or CC".into()
        } else {
            "gcc/cc".into()
        });
    }
    if missing.is_empty() {
        checks.push(pass(
            CheckKind::HostTools,
            format!(
                "Host tools: all found ({}) [{}]",
                tools.join(" "),
                host.name()
            ),
            Some("host tools".into()),
        ));
    } else {
        let hint = if missing.iter().any(|m| m.starts_with("mke2fs")) && host == HostOs::Darwin {
            " (brew install e2fsprogs; keg-only 路径已自动探测)"
        } else {
            ""
        };
        checks.push(fail(
            CheckKind::HostTools,
            format!("Host tools: missing -{}{hint}", missing.join(" ")),
        ));
    }
}

// 3/4. 内核来源：解析后的内核源码树（env > current 卷 > toml > 缺省）
fn check_kernel_source(input: &CheckInput, checks: &mut Vec<Check>) {
    match input.kernel_path.filter(|p| !p.as_os_str().is_empty()) {
        Some(p) => checks.push(pass(
            CheckKind::KernelPath,
            match input.kernel_path_source {
                Some(src) => format!("Kernel tree: {} ({src})", p.display()),
                None => format!("Kernel tree: {}", p.display()),
            },
            None,
        )),
        None => checks.push(fail(
            CheckKind::KernelPath,
            "KERNEL_PATH not set (set kernel_path in virtuoso.toml or KERNEL_PATH env var)",
        )),
    }

    let kernel_ok = input
        .kernel_path
        .map(|p| p.join("arch").is_dir())
        .unwrap_or(false);
    match (
        input.kernel_path.filter(|p| !p.as_os_str().is_empty()),
        kernel_ok,
    ) {
        (Some(p), true) => {
            let kver = kernel_version(p);
            let summary = kver
                .as_ref()
                .map(|v| format!("v{v}"))
                .unwrap_or_else(|| p.display().to_string());
            checks.push(pass(
                CheckKind::KernelSource,
                format!(
                    "Kernel source: {}{}",
                    p.display(),
                    kver.map(|v| format!(" (v{v})")).unwrap_or_default()
                ),
                Some(summary),
            ));
        }
        (Some(p), false) => checks.push(fail(
            CheckKind::KernelSource,
            format!(
                "Kernel source: {}/arch not found (set KERNEL_PATH)",
                p.display()
            ),
        )),
        (None, _) => {}
    }
}

// 5. Kernel image
fn check_kernel_image(input: &CheckInput, checks: &mut Vec<Check>) {
    let summary = |p: &Path| {
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        format!(
            "{} ({})",
            input.arch.kernel_img(),
            crate::util::human_size_ls(size)
        )
    };
    match input.kernel_image.filter(|p| p.is_file()) {
        Some(img) => checks.push(pass(
            CheckKind::KernelImage,
            format!("Kernel image: {}", summary(img)),
            Some(summary(img)),
        )),
        None => {
            let hint = "build the kernel first (virtuoso kernel build in docker mode)";
            checks.push(fail(
                CheckKind::KernelImage,
                format!(
                    "Kernel image: {} not found ({hint})",
                    input.arch.kernel_img()
                ),
            ));
        }
    }
}

// 6. QEMU binary
fn check_qemu(input: &CheckInput, checks: &mut Vec<Check>) {
    let qemu_found = input.qemu_bin.is_some_and(crate::util::which);
    let qemu_override = input.qemu_override.filter(|s| !s.is_empty());
    if let Some(q) = qemu_override {
        if qemu_found {
            checks.push(pass(
                CheckKind::QemuBinary,
                format!("QEMU binary: {q} (from env)"),
                Some(format!("{q} (from env)")),
            ));
        } else {
            checks.push(fail(
                CheckKind::QemuBinary,
                format!("QEMU binary: {q} not found (from QEMU env var)"),
            ));
        }
    } else if qemu_found {
        checks.push(pass(
            CheckKind::QemuBinary,
            format!("QEMU binary: {}", input.arch.qemu_bin()),
            Some(input.arch.qemu_bin().to_string()),
        ));
    } else {
        checks.push(fail(
            CheckKind::QemuBinary,
            format!(
                "QEMU binary: {} not found (install qemu-system-{})",
                input.arch.qemu_bin(),
                input.arch.name()
            ),
        ));
    }
    if crate::util::which("qemu-img") {
        checks.push(info(CheckKind::QemuImg, "qemu-img: available", None));
    } else {
        checks.push(info(
            CheckKind::QemuImg,
            "qemu-img: not found (optional, for manual disk image work)",
            None,
        ));
    }
}

// 7. Kernel modules（WARN，不判死；清单 = 启用组件 require 并集 + boot 基础集附加）
fn check_modules(input: &CheckInput, checks: &mut Vec<Check>) {
    if input.modules.is_empty() {
        checks.push(info(
            CheckKind::Modules,
            "Kernel modules: none required (no enabled component declares require)",
            Some("none required".into()),
        ));
    } else {
        let total = input.modules.len();
        let found = input.modules.iter().filter(|(_, ok)| *ok).count();
        if found == total {
            checks.push(pass(
                CheckKind::Modules,
                format!("Kernel modules: {found}/{total} found"),
                Some(format!("{found}/{total} found")),
            ));
        } else {
            let missing: Vec<&str> = input
                .modules
                .iter()
                .filter(|(_, ok)| !ok)
                .map(|(m, _)| m.as_str())
                .collect();
            checks.push(warn(
                CheckKind::Modules,
                format!(
                    "Kernel modules: {found}/{total} found, missing: {}",
                    missing.join(" ")
                ),
            ));
        }
    }
}

// 8. BusyBox
fn check_busybox(input: &CheckInput, checks: &mut Vec<Check>) {
    if input.busybox_cached {
        checks.push(pass(
            CheckKind::BusyBox,
            format!("BusyBox: cached ({})", input.arch.name()),
            Some("busybox".into()),
        ));
    } else {
        checks.push(info(
            CheckKind::BusyBox,
            format!(
                "BusyBox: not cached for {} (GitHub release download on first build)",
                input.arch.name()
            ),
            Some("busybox (not cached)".into()),
        ));
    }
}

// 9. Cross-compile
fn check_cross(input: &CheckInput, checks: &mut Vec<Check>) {
    if input.host_is_cross {
        checks.push(warn(
            CheckKind::CrossCompile,
            format!(
                "Cross-compile: ARCH={} differs from host ({}); C 用例走 zig/CC，Rust 走 rustup musl target",
                input.arch.name(),
                std::env::consts::ARCH
            ),
        ));
    }
}

// 10. Tools image
fn check_tools_image(input: &CheckInput, checks: &mut Vec<Check>) {
    checks.push(if input.tools_img_exists {
        info(
            CheckKind::ToolsImage,
            "Tools image: exists (attached as /dev/vdb, mounted at /tools)",
            Some("tools.img".into()),
        )
    } else {
        info(
            CheckKind::ToolsImage,
            "Tools image: not built yet (run `virtuoso build`)",
            Some("tools.img (not built)".into()),
        )
    });
}

// 11. initrd
fn check_initrd(input: &CheckInput, checks: &mut Vec<Check>) {
    match input.initrd.filter(|p| p.is_file()) {
        Some(p) => {
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            let summary = format!("initrd.img ({})", crate::util::human_size_ls(size));
            checks.push(info(
                CheckKind::Initrd,
                format!(
                    "Initrd: {} ({})",
                    p.display(),
                    crate::util::human_size_ls(size)
                ),
                Some(summary),
            ));
        }
        None => checks.push(info(
            CheckKind::Initrd,
            "Initrd: not built yet (run `virtuoso build`)",
            Some("initrd (not built)".into()),
        )),
    }
}

// 12. 组件平台门：vfio 架构性依赖 Linux（IOMMU + vfio-pci）；pmem 的
// memory-backend-file/dumpdtb 链路在 macOS 上未经实测（brew dtc 可得）
fn check_components(input: &CheckInput, checks: &mut Vec<Check>) {
    if input.vfio_enabled {
        if input.host == HostOs::Darwin {
            checks.push(fail(
                CheckKind::Components,
                "Components: vfio requires Linux host (IOMMU + vfio-pci) — 关闭 [components.vfio]",
            ));
        } else {
            checks.push(pass(
                CheckKind::Components,
                "Components: vfio enabled (Linux + IOMMU)",
                Some("vfio enabled (Linux + IOMMU)".into()),
            ));
        }
    }
    if input.pmem_enabled && input.host == HostOs::Darwin {
        checks.push(warn(
            CheckKind::Components,
            "Components: pmem on macOS is experimental (memory-backend-file + dumpdtb/fdtput 未在 HVF 实测)",
        ));
    }
}

// 13. rootfs.d drop-in（可选用户目录，不判死；doctor 负责缺失即创建）
fn check_rootfs_d(input: &CheckInput, checks: &mut Vec<Check>) {
    let hint = "drop-in dir; merged into rootfs at build (add-only)";
    match &input.rootfs_d {
        RootfsDState::Ready { files: _, created: true } => checks.push(info(
            CheckKind::RootfsD,
            format!("rootfs.d: created ({hint})"),
            Some("rootfs.d created".into()),
        )),
        RootfsDState::Ready { files: 0, created: false } => checks.push(info(
            CheckKind::RootfsD,
            format!("rootfs.d: empty ({hint})"),
            Some("rootfs.d empty".into()),
        )),
        RootfsDState::Ready { files, created: false } => checks.push(info(
            CheckKind::RootfsD,
            format!("rootfs.d: {files} file(s) ({hint})"),
            Some(format!("rootfs.d ({files} files)")),
        )),
        RootfsDState::NotADir => checks.push(warn(
            CheckKind::RootfsD,
            "rootfs.d: not a directory — drop-in skipped (replace it with a directory)",
        )),
        RootfsDState::Absent { err: Some(e) } => checks.push(warn(
            CheckKind::RootfsD,
            format!("rootfs.d: not present (create failed: {e}); optional {hint}"),
        )),
        RootfsDState::Absent { err: None } => checks.push(info(
            CheckKind::RootfsD,
            format!("rootfs.d: not present (optional {hint}; `virtuoso doctor` creates it)"),
            None,
        )),
    }
}

/// docker 供给模式（forge 活动卷）检查：仅在状态文件存在（活动卷开启）时
/// 由 doctor 投影附加——raw 用户无状态文件，零打扰。
pub(crate) fn kernel_docker_checks(
    volume: &str,
    arch: &str,
    engine_err: Option<&str>,
    host_view: Option<&Path>,
    image_present: bool,
    image: &str,
) -> Vec<Check> {
    let mut checks = Vec::new();
    match engine_err {
        // engine ok 行无独立信息量：组状态由其余行决定，不进一屏
        None => checks.push(pass(
            CheckKind::KernelDocker,
            "Kernel docker: engine ok",
            None,
        )),
        Some(e) => checks.push(fail(CheckKind::KernelDocker, format!("Kernel docker: {e}"))),
    }
    match host_view.filter(|p| p.is_dir()) {
        Some(p) => checks.push(pass(
            CheckKind::KernelDocker,
            format!(
                "Kernel docker: volume {volume} reachable ({}) [arch {arch}]",
                p.display()
            ),
            Some(format!("docker volume {volume}")),
        )),
        None => checks.push(fail(
            CheckKind::KernelDocker,
            format!(
                "Kernel docker: volume {volume} host view unreachable (start OrbStack / run `virtuoso kernel clone <git-url>`)"
            ),
        )),
    }
    if image_present {
        checks.push(pass(
            CheckKind::KernelDocker,
            format!("Kernel docker: toolchain image {image}"),
            Some("toolchain image".into()),
        ));
    } else {
        checks.push(info(
            CheckKind::KernelDocker,
            "Kernel docker: toolchain image not pulled yet (auto-pull on next `virtuoso kernel` command)",
            Some("toolchain image (not pulled)".into()),
        ));
    }
    checks
}

fn kernel_version(kernel: &Path) -> Option<String> {
    let makefile = std::fs::read_to_string(kernel.join("Makefile")).ok()?;
    let mut parts = Vec::new();
    for key in ["VERSION", "PATCHLEVEL", "SUBLEVEL"] {
        for line in makefile.lines() {
            if let Some(v) = line.strip_prefix(&format!("{key} = ")) {
                parts.push(v.trim().to_string());
                break;
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join("."))
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_strips_kind_label() {
        assert_eq!(
            detail(
                CheckKind::QemuBinary,
                "QEMU binary: qemu-system-arm not found"
            ),
            "qemu-system-arm not found"
        );
        assert_eq!(detail(CheckKind::BusyBox, "BusyBox: cached (arm64)"), "cached (arm64)");
        // 未以 label 开头的消息原样保留（防御性）
        assert_eq!(detail(CheckKind::Config, "bare text"), "bare text");
    }
}
