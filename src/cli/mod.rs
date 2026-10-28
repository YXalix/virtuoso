//! CLI orchestration: subcommand dispatch and wiring for each command group.
//!
//! - doctor  → `cli::doctor` (single health check entry: one-screen view +
//!   full --verbose; engine projection engine_report lives in the same file)
//! - build   → `cli::build` (build / clean / skill)
//! - kernel  → `cli::kernel` (containerized kernel provisioning: clone/defconfig/
//!   build/cc*/volume management; logic in forge)
//! - vm      → `cli::vm` (shell / test: launch, watchdog, verdict wiring)
//!
//! Behavior baseline (exit code semantics) unchanged: 0=pass, 124=timeout, anything
//! else=failure (single point in crate::judge).

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

// ---------------------------------------------------------------- config resolution (shared)

pub(crate) fn resolve_arch(cfg: &Config, cli_arch: Option<&str>) -> anyhow::Result<Arch> {
    cli_arch
        .and_then(Arch::parse)
        .or_else(|| cfg.arch())
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the target arch (check the ARCH value)"))
}

/// Resolve the NUMA topology (invalid config errors at resolution time, not runtime).
pub(crate) fn resolve_topology(cfg: &Config) -> anyhow::Result<NumaTopology> {
    let (smp, nodes, mem) = cfg.topo_params();
    NumaTopology::parse(&smp, &nodes, &mem).map_err(|e| {
        anyhow::anyhow!(
            "{e}\n  Fix smp / [components.numa] in virtuoso.toml (smp must be divisible by the NUMA node count)"
        )
    })
}

/// Boot kernel image path: kernel_image override first, then fall back to the arch
/// image inside the kernel tree.
pub(crate) fn kernel_image_path(cfg: &Config, arch: Arch) -> anyhow::Result<std::path::PathBuf> {
    let (kernel_path, _) = cfg.kernel_path()?;
    Ok(cfg
        .kernel_image()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| kernel_path.join(arch.kernel_img())))
}

/// tools.img data disk (tools attached as virtio-blk → /dev/vdb in the guest, mounted at /tools).
/// Attached only when [components.tools_disk] enabled (section default = true) and the
/// artifact exists — the default DSL argv stays at the baseline (frozen invariant 3).
pub(crate) fn tools_disk_opt(cfg: &Config) -> Option<crate::launcher::DataDisk> {
    if !cfg.tools_disk_enabled() {
        return None;
    }
    let p = cfg.artifacts_dir.join("tools.img");
    p.is_file().then(|| crate::launcher::DataDisk::new(p))
}

/// Agent channel socket (Some only when [components.agent] enabled), with leftovers
/// from the previous session cleared (QEMU does not clean up an existing socket path).
/// The socket lands in the caller-given directory (shell: target/, test: run directory).
pub(crate) fn open_agent_socket(cfg: &Config, dir: &Path, name: &str) -> Option<std::path::PathBuf> {
    let sock = cfg
        .agent_enabled()
        .then(|| dir.join(format!("{name}.sock")))?;
    let _ = std::fs::remove_file(&sock);
    Some(sock)
}

/// VM launch plan (the assembly input shared by the shell / test / probe entry points).
pub(crate) struct LaunchPlan<'a> {
    pub cfg: &'a Config,
    pub arch: Arch,
    pub topo: NumaTopology,
    pub accel: crate::launcher::Accel,
    /// test path = cfg.auto_test(); shell/probe have no auto-test semantics (false).
    pub auto_test: bool,
    /// Test case selection (`virtuoso test --only`; empty = run all). Always empty for shell/probe.
    pub only_tests: Vec<String>,
    /// `-s -S` GDB stub on :1234 (`virtuoso shell --gdb` only; always false for test/probe).
    pub gdb_stub: bool,
    /// Agent channel socket (None = no channel). probe always enables it (bypasses
    /// the component switch).
    pub agent_socket: Option<std::path::PathBuf>,
}

/// Single QemuInvocation assembly point: the three copied chains for shell / test /
/// probe become one, with a uniform member assignment order (eliminating caller
/// drift). Default members keep the frozen argv baseline (verbatim equal to the
/// platform baseline with no disk, no agent, no pmem).
pub(crate) fn build_invocation(plan: &LaunchPlan) -> anyhow::Result<crate::launcher::QemuInvocation> {
    let cfg = plan.cfg;
    let kernel = kernel_image_path(cfg, plan.arch)?;
    // Global passthrough fallback (QEMU_OPTS/qemu_opts) first, component increments
    // (vfio devices) after; the two sources are kept explicitly separate at the
    // assembly point — the config layer does not mix them.
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
    .gdb_stub(plan.gdb_stub)
    .cmdline_extra(cfg.cmdline_extra().as_deref().unwrap_or(""))
    .extra_opts(&extra);
    if let Some(sock) = &plan.agent_socket {
        inv = inv.agent_serial(sock);
    }
    Ok(inv)
}
