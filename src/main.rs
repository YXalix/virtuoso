//! Virtuoso — kernel E2E 虚拟化测试装置的 CLI 入口。
//!
//! 单包 crate（无 workspace 成员）：clap 定义与子命令分发在 `main`，命令
//! 实现在 `cli`，运行工件在 `runs`，类型化配置在 `config`；领域模块
//! `builder` / `launcher` / `judge` / `forge` 与顶层工具集 `util`、平台
//! 类型 `arch` 同住 `src/`。

mod arch;
mod builder;
mod cli;
mod config;
mod forge;
mod judge;
mod launcher;
mod runs;
mod util;

pub(crate) use arch::{Arch, HostOs};

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "virtuoso",
    version,
    about = "Virtuoso — kernel E2E virtualization test harness",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// One-screen environment check (✓/✗/! per component; --verbose for the full checklist)
    Doctor {
        /// Override the target arch (passed through as the ARCH env var)
        #[arg(long)]
        arch: Option<String>,
        /// Full output: typed config diagnostics + complete checklist
        #[arg(long)]
        verbose: bool,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Build initrd/rootfs: C testcases + modules.conf + BusyBox
    Build {
        /// Only provision the static BusyBox for the current arch (GitHub release download, cached locally)
        #[arg(long = "busybox-only")]
        busybox_only: bool,
    },
    /// Containerized kernel supply (forge): named-volume sources + pinned
    /// toolchain image. Volume management and current switching
    /// (state file .virtuoso/kernel-current.json); output = a host-visible
    /// kernel tree (consumed via kernel_path)
    Kernel {
        #[command(subcommand)]
        action: KernelAction,
    },
    /// Boot QEMU interactively into the BusyBox shell (--gdb waits for GDB on :1234)
    Shell {
        /// KVM acceleration (Linux; only when host matches the target arch)
        #[arg(long)]
        kvm: bool,
        /// Force TCG emulation (use on macOS, which defaults to HVF)
        #[arg(long)]
        tcg: bool,
        /// Halt waiting for GDB on :1234 (always TCG)
        #[arg(long)]
        gdb: bool,
    },
    /// CI mode: rebuild initrd → run with timeout → marker-protocol verdict.
    /// --replay-until-fail N: re-run up to N times for suspected flaky cases,
    /// stopping at the first non-passed verdict.
    Test {
        /// Wall-clock timeout seconds; 0 is rejected. Default: timeout_secs from virtuoso.toml
        #[arg(long)]
        timeout: Option<u64>,
        /// Override the target arch (passed through as the ARCH env var)
        #[arg(long)]
        arch: Option<String>,
        /// Replay round limit (default 1 = single run)
        #[arg(long = "replay-until-fail")]
        replay_until_fail: Option<u32>,
        /// Force TCG emulation (macOS defaults to HVF; Linux already defaults to TCG)
        #[arg(long)]
        tcg: bool,
        /// Run only the named tests (comma-separated, repeatable): flat
        /// basename, group/case, or group/ (whole group);
        /// empty = run all of /tests. Passed to init as virtuoso.only=
        #[arg(long = "only", value_delimiter = ',')]
        only: Vec<String>,
    },
    /// Remove build artifacts
    Clean,
    /// Manage AI skills (install into / remove from the kernel tree)
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// AI probe: send a command batch over the virtio-serial agent and stream
    /// structured events back (tools/virtuoso-agent channel; exit codes 0/1/124)
    Probe {
        /// Override the target arch (passed through as the ARCH env var)
        #[arg(long)]
        arch: Option<String>,
        /// Total wall-clock timeout seconds, including TCG boot and handshake (0 is rejected; default 300)
        #[arg(long)]
        timeout: Option<u64>,
        /// Shell command to run (repeatable)
        #[arg(long = "cmd")]
        cmds: Vec<String>,
        /// File with one command per line (# comments allowed)
        #[arg(long = "cmd-file")]
        cmd_file: Option<PathBuf>,
        /// Machine-readable JSON output (raw event lines, for AI pipelines)
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SkillAction {
    /// Install the kernel-dev skills into the kernel tree
    Install,
    /// Remove the skills from the kernel tree
    Uninstall,
}

#[derive(Subcommand)]
enum KernelAction {
    /// Clone kernel sources into a named volume (create + render .clangd + set current)
    Clone {
        /// Kernel git repository URL
        url: String,
        /// Branch/tag to clone (default master; KERNEL_REF overrides)
        #[arg(long = "ref")]
        ref_name: Option<String>,
        /// Target volume name (default virtuoso-kernel; KERNEL_VOLUME overrides)
        #[arg(long = "as")]
        as_volume: Option<String>,
        /// Override the target arch (default: top-level arch; KERNEL_ARCH overrides)
        #[arg(long)]
        arch: Option<String>,
        /// Full clone with complete git history (default: --depth 1, tree only)
        #[arg(long)]
        full: bool,
    },
    /// Run make <name> (.config lands in the volume)
    Defconfig {
        /// make target (default defconfig; e.g. arm64_defconfig / oe_core_defconfig)
        name: Option<String>,
        /// Override the target arch (default: current-recorded / top-level arch)
        #[arg(long)]
        arch: Option<String>,
    },
    /// make Image/bzImage + modules; generate CDB (raw form under /ksrc, consumed by clangd in the devcontainer)
    Build {
        /// Parallel jobs (default: host cores)
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
        /// Override the target arch
        #[arg(long)]
        arch: Option<String>,
    },
    /// Print the host-visible path of a volume (for kernel_path; AI/editor cwd)
    Path {
        /// Query a specific volume (default: current)
        #[arg(long)]
        volume: Option<String>,
    },
    /// Interactive bash inside the container
    Shell {
        /// Override the target arch
        #[arg(long)]
        arch: Option<String>,
    },
    /// List volumes: content state (empty/cloned/configured) + current marker
    List,
    /// Switch current (writes .virtuoso/kernel-current.json; switching back keeps increments)
    Use {
        /// Volume name
        volume: String,
        /// Override the recorded arch (default: currently resolved target arch)
        #[arg(long)]
        arch: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    crate::launcher::install_ctrlc_guard();
    match cli::dispatch(cli.command) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("ERROR: {e:#}");
            std::process::exit(1);
        }
    }
}
