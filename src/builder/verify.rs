//! Prerequisite check engine (Rust takeover of verify.sh). Output text and
//! exit code semantics align with the shell baseline: critical FAIL →
//! exit 1; WARN/INFO do not affect the exit code.
//!
//! Both doctor presentations (one-screen grouped / --verbose full) consume
//! this module's Report: each Check carries its own `kind` (grouping) and
//! `summary` (a compact one-screen phrase; None = excluded from the one
//! screen) — new prerequisites only touch the engine and the presentation
//! follows automatically; doctor never reverse-parses message text.
//!
//! Sections: types and presentation (Check/CheckKind/Level/Report) →
//! engine input and orchestration (CheckInput/run_checks) → per-check
//! functions.

use std::path::Path;

use crate::{Arch, HostOs};

use crate::builder::modconf;

// ---------------------------------------------------------------- types and presentation

/// Check subject (the sole basis for doctor one-screen grouping; grouping and label share a source, no text reverse-parsing).
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
    /// Message label (msg starts with "<label>: "; doctor strips the prefix for the body).
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
    /// Compact one-screen phrase (for Pass/Info; None = excluded from the one screen, e.g. purely optional info lines).
    pub summary: Option<String>,
}

/// Body after the msg's "<label>: " prefix (the per-line fix hint for Fail/Warn).
pub(crate) fn detail(kind: CheckKind, msg: &str) -> &str {
    msg.strip_prefix(kind.label())
        .map(|r| r.trim_start_matches([':', ' ']))
        .unwrap_or(msg)
}

fn mk_check(
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
    mk_check(Level::Pass, kind, msg, summary)
}
fn fail(kind: CheckKind, msg: impl Into<String>) -> Check {
    mk_check(Level::Fail, kind, msg, None)
}
fn warn(kind: CheckKind, msg: impl Into<String>) -> Check {
    mk_check(Level::Warn, kind, msg, None)
}
fn info(kind: CheckKind, msg: impl Into<String>, summary: Option<String>) -> Check {
    mk_check(Level::Info, kind, msg, summary)
}

pub(crate) struct Report {
    pub checks: Vec<Check>,
    pub critical_fail: u32,
    pub warnings: u32,
}

impl Report {
    /// Appends checks and recomputes counts (used by doctor to attach the docker provisioning group).
    pub(crate) fn extend(&mut self, extra: Vec<Check>) {
        self.checks.extend(extra);
        self.recount();
    }

    /// Recomputes the counts from the check list (the only counting path).
    pub(crate) fn recount(&mut self) {
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

    /// Renders (colored on tty; --verbose prints the full list).
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

// ---------------------------------------------------------------- engine input and orchestration

/// rootfs.d drop-in state (the directory is not distributed with clone;
/// doctor creates it when missing, see cli::doctor::ensure_rootfs_d; this
/// engine only reads state).
pub(crate) enum RootfsDState {
    /// Directory ready: files = number of files to be merged; created = just created this run.
    Ready { files: usize, created: bool },
    /// Path exists but is not a directory (drop-in inactive).
    NotADir,
    /// Does not exist (err = why creation failed; None = caller did not attempt creation).
    Absent { err: Option<String> },
}

/// Check engine input (projected by doctor / engine_report; assembled once, avoiding a long parameter list).
pub(crate) struct CheckInput<'a> {
    pub config_file_exists: bool,
    pub kernel_path: Option<&'a Path>,
    /// Where kernel_path was resolved from ("env KERNEL_PATH" / "current
    /// volume X" / "config" / "auto-detected"), for presentation; None = the
    /// path is missing too.
    pub kernel_path_source: Option<&'a str>,
    pub arch: Arch,
    pub host: HostOs,
    pub host_is_cross: bool,
    pub kernel_image: Option<&'a Path>,
    pub qemu_bin: Option<&'a str>,
    pub qemu_override: Option<&'a str>,
    /// (module name, whether the .ko was found)
    pub modules: &'a [(String, bool)],
    pub busybox_cached: bool,
    pub tools_img_exists: bool,
    pub initrd: Option<&'a Path>,
    pub vfio_enabled: bool,
    pub pmem_enabled: bool,
    pub rootfs_d: RootfsDState,
}

/// Runs all checks (the shell baseline's checks + component platform gates +
/// rootfs.d drop-in), each as an independent function.
pub(crate) fn run_checks(input: &CheckInput) -> Report {
    let mut report = Report {
        checks: Vec::new(),
        critical_fail: 0,
        warnings: 0,
    };
    check_config(input, &mut report.checks);
    check_host_tools(input, &mut report.checks);
    check_kernel_source(input, &mut report.checks);
    check_kernel_image(input, &mut report.checks);
    check_qemu(input, &mut report.checks);
    check_modules(input, &mut report.checks);
    check_busybox(input, &mut report.checks);
    check_cross(input, &mut report.checks);
    check_tools_image(input, &mut report.checks);
    check_initrd(input, &mut report.checks);
    check_components(input, &mut report.checks);
    check_rootfs_d(input, &mut report.checks);
    report.recount();
    report
}

// ---------------------------------------------------------------- per-check functions

/// Host tools table (platform-independent). make/sed retired along with the
/// busybox source fallback; initrd packing is now native (builder::cpio),
/// so cpio/gzip/wget/nproc/timeout are no longer hard requirements; the
/// download layer falls back to curl when wget is missing.
/// With testcases all on cargo (build.rs + cc), the C compiler is uniformly
/// the zig cc wrapper (builder::cross), and cmake retired along the CMake path.
pub(crate) fn host_tools() -> &'static [&'static str] {
    &["tar", "zig", "find"]
}

/// Download tool (busybox supply layer): either wget or curl (macOS ships curl).
fn fetch_tool_ok() -> bool {
    crate::util::which("wget") || crate::util::which("curl")
}

/// Whether every module from component plan entries (conf lines) can be
/// found in the kernel tree (input collection for the module check).
/// When `kernel_path` is None (no kernel tree configured), everything counts as not found.
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

// 2. Host tools (platform-independent table + download/image/cross toolchains)
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
    // C test case compiler: Linux = host gcc/cc; macOS = CC env or zig (cross)
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
            " (brew install e2fsprogs; keg-only paths are probed automatically)"
        } else {
            ""
        };
        checks.push(fail(
            CheckKind::HostTools,
            format!("Host tools: missing -{}{hint}", missing.join(" ")),
        ));
    }
}

// 3/4. Kernel origin: the resolved kernel source tree (env > current volume > toml > default)
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
            // Name the actually resolved path (the KERNEL_IMAGE override may point
            // somewhere other than the arch default inside the tree).
            let name = input
                .kernel_image
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| input.arch.kernel_img().to_string());
            checks.push(fail(
                CheckKind::KernelImage,
                format!("Kernel image: {name} not found ({hint})"),
            ));
        }
    }
}

// 6. QEMU binary (qemu_bin carries the effective binary — override when set,
// else the arch default — with found-ness already resolved by the projection)
fn check_qemu(input: &CheckInput, checks: &mut Vec<Check>) {
    let qemu_override = input.qemu_override.filter(|s| !s.is_empty());
    if let Some(q) = qemu_override {
        if input.qemu_bin.is_some() {
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
    } else if input.qemu_bin.is_some() {
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

// 7. Kernel modules (WARN, never fatal; list = union of enabled component requires + boot base set extras)
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
                "Cross-compile: ARCH={} differs from host ({}); C test cases go through zig/CC, Rust through the rustup musl target",
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

// 12. Component platform gates: vfio architecturally requires Linux (IOMMU
// + vfio-pci); the pmem memory-backend-file/dumpdtb chain is untested on
// macOS (brew dtc provides it)
fn check_components(input: &CheckInput, checks: &mut Vec<Check>) {
    if input.vfio_enabled {
        if input.host == HostOs::Darwin {
            checks.push(fail(
                CheckKind::Components,
                "Components: vfio requires Linux host (IOMMU + vfio-pci) — disable [components.vfio]",
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
            "Components: pmem on macOS is experimental (memory-backend-file + dumpdtb/fdtput untested under HVF)",
        ));
    }
}

// 13. rootfs.d drop-in (optional user dir, never fatal; doctor creates it when missing)
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

/// Docker provisioning mode (forge active volume) checks: attached by the
/// doctor projection only when the state file exists (active volume on) —
/// raw users have no state file and see zero noise.
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
        // The engine ok line carries no standalone information: group status is decided by the other lines, excluded from the one screen
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

// ---------------------------------------------------------------- tests

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
        // Messages not starting with the label are kept verbatim (defensive)
        assert_eq!(detail(CheckKind::Config, "bare text"), "bare text");
    }
}
