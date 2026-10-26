//! CLI 编排：子命令分发与各命令组的接线。
//!
//! - doctor  → `cli::doctor`（体检唯一入口：一屏呈现 + --verbose 全量；引擎投影 engine_report 同文件）
//! - build   → `cli::build`（build / clean / skill）
//! - kernel  → `cli::kernel`（容器化内核供给：clone/defconfig/build/cc*/卷管理；逻辑在 forge）
//! - vm      → `cli::vm`（shell / test：启动、看门狗、判定接线）
//!
//! 行为基线（退出码语义）不变：0=通过、124=超时、其余=失败（单点在 crate::judge）。

mod build;
mod diagnostics;
mod doctor;
mod kernel;
mod pmem;
mod probe;
mod vm;

use std::path::Path;

use crate::launcher::NumaTopology;
use crate::Arch;

use crate::config::Config;
use crate::Command as CliCommand;

pub(crate) use pmem::pmem_opt;

pub(crate) fn code_of(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(130)
}

pub fn dispatch(cmd: CliCommand) -> anyhow::Result<i32> {
    match cmd {
        CliCommand::Doctor {
            arch,
            json,
            verbose,
        } => doctor::run_doctor(arch.as_deref(), json, verbose),
        CliCommand::Build { busybox_only } => build::run_build(busybox_only),
        CliCommand::Kernel { action } => kernel::run_kernel(action),
        CliCommand::Shell { kvm, tcg, gdb } => vm::run_shell(kvm, tcg, gdb),
        CliCommand::Test {
            timeout,
            arch,
            replay_until_fail,
            tcg,
            only,
        } => vm::run_test(timeout, arch.as_deref(), replay_until_fail, tcg, &only),
        CliCommand::Clean => build::run_clean(),
        CliCommand::Skill { action } => build::run_skill(action),
        CliCommand::Probe {
            arch,
            timeout,
            cmds,
            cmd_file,
            json,
        } => probe::run_probe(arch.as_deref(), timeout, &cmds, cmd_file.as_deref(), json),
    }
}

// ---------------------------------------------------------------- 配置解析（各命令组共用）

pub(crate) fn resolve_arch(cfg: &Config, cli_arch: Option<&str>) -> anyhow::Result<Arch> {
    cli_arch
        .and_then(Arch::parse)
        .or_else(|| cfg.arch())
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the target arch (check the ARCH value)"))
}

/// 解析 NUMA 拓扑（非法配置解析期报错，而非运行时）。
pub(crate) fn resolve_topology(cfg: &Config) -> anyhow::Result<NumaTopology> {
    let (smp, nodes, mem) = cfg.topo_params();
    NumaTopology::parse(&smp, &nodes, &mem).map_err(|e| {
        anyhow::anyhow!(
            "{e}\n  Fix smp / [components.numa] in virtuoso.toml (smp must be divisible by the NUMA node count)"
        )
    })
}

/// 启动内核镜像路径：优先 kernel_image 覆盖，再回落内核树内 arch 对应镜像。
pub(crate) fn kernel_image_path(cfg: &Config, arch: Arch) -> anyhow::Result<std::path::PathBuf> {
    let (kernel_path, _) = cfg.kernel_path()?;
    Ok(cfg
        .kernel_image()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| kernel_path.join(arch.kernel_img())))
}

/// tools.img 数据盘（tools 外挂 virtio-blk → guest 内 /dev/vdb 挂 /tools）。
/// [components.tools_disk] enabled（段缺省 = true）且产物存在才附加——
/// DSL 缺省 argv 保持基线（冻结不变量 3）。
pub(crate) fn tools_disk_opt(cfg: &Config) -> Option<crate::launcher::DataDisk> {
    if !cfg.tools_disk_enabled() {
        return None;
    }
    let p = cfg.artifacts_dir.join("tools.img");
    p.is_file().then(|| crate::launcher::DataDisk::new(p))
}

/// agent 通道 socket（[components.agent] enabled 才 Some）。
/// socket 落调用方给定的目录（shell 用 target/，test 用 run 目录）。
pub(crate) fn agent_socket_opt(cfg: &Config, dir: &Path, name: &str) -> Option<std::path::PathBuf> {
    cfg.agent_enabled()
        .then(|| dir.join(format!("{name}.sock")))
}

/// agent 通道 socket（组件开关判定）+ 清掉上一次会话的残留
/// （QEMU 不清理已存在的 socket 路径）。
pub(crate) fn open_agent_socket(
    cfg: &Config,
    dir: &Path,
    name: &str,
) -> Option<std::path::PathBuf> {
    let sock = agent_socket_opt(cfg, dir, name)?;
    let _ = std::fs::remove_file(&sock);
    Some(sock)
}

/// VM 启动计划（shell / test / probe 三入口共用的装配输入）。
pub(crate) struct LaunchPlan<'a> {
    pub cfg: &'a Config,
    pub arch: Arch,
    pub topo: NumaTopology,
    pub accel: crate::launcher::Accel,
    /// test 路径 = cfg.auto_test()；shell/probe 无自动测试语义（false）。
    pub auto_test: bool,
    /// 测例选择（`virtuoso test --only`；空 = 全跑）。shell/probe 恒空。
    pub only_tests: Vec<String>,
    /// agent 通道 socket（None = 无通道）。probe 恒开（绕过组件开关）。
    pub agent_socket: Option<std::path::PathBuf>,
}

/// 单一 QemuInvocation 装配点：shell / test / probe 三份复制链合一，
/// 成员赋值顺序统一（消除调用方漂移）。缺省成员保持冻结 argv 基线
/// （无盘无 agent 无 pmem 时逐字等于平台基线）。
pub(crate) fn build_invocation(plan: &LaunchPlan) -> anyhow::Result<crate::launcher::QemuInvocation> {
    let cfg = plan.cfg;
    let kernel = kernel_image_path(cfg, plan.arch)?;
    // 全局透传兜底（QEMU_OPTS/qemu_opts）在前，组件增量（vfio 设备）在后；
    // 两类来源在装配点显式分清，config 层不混装。
    let mut extra = cfg.qemu_extra();
    extra.extend(cfg.vfio_opts());
    let mut inv = crate::launcher::QemuInvocation::new(
        plan.arch,
        &kernel,
        cfg.artifacts_dir.join("initrd.img"),
        cfg.artifacts_dir.join("rootfs.img"),
    )
    .accel(plan.accel)
    .topo(plan.topo.clone())
    .pmem(pmem_opt(cfg, plan.arch, &plan.topo)?)
    .virtio_disks(tools_disk_opt(cfg))
    .qemu_override(cfg.qemu_override().as_deref())
    .auto_test(plan.auto_test)
    .test_only(&plan.only_tests)
    .cmdline_extra(cfg.cmdline_extra().as_deref().unwrap_or(""))
    .extra_opts(&extra);
    if let Some(sock) = &plan.agent_socket {
        inv = inv.agent_serial(sock);
    }
    Ok(inv)
}
