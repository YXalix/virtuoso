//! QEMU launch-argument DSL — the strongly typed equivalent of run-qemu.sh.
//!
//! Module split: `accel` (accelerator selection), `argv` (argv assembly, guarded by
//! frozen-baseline unit tests), `spawn` (process launch + guardian registration); this
//! file carries the invocation surface (struct + builder).

mod accel;
mod argv;
mod spawn;

pub(crate) use accel::Accel;

use std::path::PathBuf;

use crate::{Arch, HostOs};

use crate::launcher::numa::NumaTopology;
pub(crate) use crate::launcher::{DataDisk, PmemSpec};

/// QEMU launch arguments.
#[derive(Debug, Clone)]
pub(crate) struct QemuInvocation {
    pub arch: Arch,
    pub qemu_override: Option<String>,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub rootfs: PathBuf,
    /// Extra data disks (virtio-blk, appended after the rootfs → /dev/vdb onward),
    /// e.g. tools.img (mounted at /tools for PATH references).
    pub data_disks: Vec<DataDisk>,
    pub accel: Accel,
    /// Host platform: decides the memory backend shape (Linux=memfd, macOS=ram) and
    /// accel validity. Defaults to the compilation target; the argv_* unit tests pin it
    /// explicitly to freeze the dual baseline.
    pub host: HostOs,
    pub topo: NumaTopology,
    /// auto_test switch of the kernel cmdline
    pub auto_test: bool,
    /// Test selection (`virtuoso test --only`): when non-empty the cmdline appends
    /// `virtuoso.only=<a,b>` and init's auto-test runs only the listed /tests binaries.
    /// The empty default keeps argv verbatim-identical to the frozen baseline.
    pub test_only: Vec<String>,
    /// `-s -S`: suspend waiting for a GDB connection on :1234
    pub gdb_stub: bool,
    /// AI agent channel: a virtio-serial port with a host-side unix socket (chardev).
    /// Some appends chardev + virtio-serial-pci + virtserialport; None (default) keeps
    /// argv verbatim-identical to the run-qemu.sh baseline (frozen invariant 3).
    pub agent_serial: Option<PathBuf>,
    /// Persistent memory component (DT route): Some swaps the main memory backend to a
    /// host file (share=on; pmem-region writes persist to disk) and appends -dtb (the
    /// patched device tree containing the pmem-region node); None (default) keeps argv
    /// verbatim-identical to the baseline (frozen invariant 3).
    /// Single-node NUMA only; x86_64 has no DT and is rejected by the caller.
    pub pmem: Option<PmemSpec>,
    /// Extra kernel cmdline text (injection point of env `CMDLINE_EXTRA` / toml
    /// `cmdline_extra`): joined verbatim after the machine-assembled part; the empty
    /// default keeps argv verbatim-identical to the frozen baseline (frozen invariant 3).
    pub cmdline_extra: String,
    /// Passed through verbatim (shell-expansion semantics: split on whitespace)
    pub extra_opts: Vec<String>,
}

impl QemuInvocation {
    pub(crate) fn new(
        arch: Arch,
        kernel: impl Into<PathBuf>,
        initrd: impl Into<PathBuf>,
        rootfs: impl Into<PathBuf>,
    ) -> Self {
        Self {
            arch,
            qemu_override: None,
            kernel: kernel.into(),
            initrd: initrd.into(),
            rootfs: rootfs.into(),
            data_disks: Vec::new(),
            accel: Accel::Tcg,
            host: HostOs::current(),
            topo: NumaTopology {
                smp: 1,
                nodes: 1,
                memory_per_node: "1G".into(),
            },
            auto_test: false,
            test_only: Vec::new(),
            gdb_stub: false,
            agent_serial: None,
            pmem: None,
            cmdline_extra: String::new(),
            extra_opts: Vec::new(),
        }
    }

    pub(crate) fn accel(mut self, accel: Accel) -> Self {
        self.accel = accel;
        self
    }

    /// Pins the host platform (for argv freeze unit tests; the runtime path defaults
    /// to the compilation target).
    #[cfg(test)]
    pub(crate) fn host_os(mut self, host: HostOs) -> Self {
        self.host = host;
        self
    }

    pub(crate) fn topo(mut self, topo: NumaTopology) -> Self {
        self.topo = topo;
        self
    }

    pub(crate) fn qemu_override(mut self, qemu: Option<&str>) -> Self {
        self.qemu_override = qemu.filter(|s| !s.is_empty()).map(str::to_string);
        self
    }

    /// Attaches one virtio-blk data disk (appears as /dev/vdb, /dev/vdc… in call order).
    #[cfg(test)]
    pub(crate) fn virtio_disk(mut self, disk: DataDisk) -> Self {
        self.data_disks.push(disk);
        self
    }

    /// Attaches data disks in bulk (accepts Option / iterators; the empty default is unchanged).
    pub(crate) fn virtio_disks(mut self, disks: impl IntoIterator<Item = DataDisk>) -> Self {
        self.data_disks.extend(disks);
        self
    }

    pub(crate) fn auto_test(mut self, on: bool) -> Self {
        self.auto_test = on;
        self
    }

    /// Test selection (empty = run all; the caller owns name validity: non-empty, no
    /// commas/whitespace).
    pub(crate) fn test_only(mut self, names: &[String]) -> Self {
        self.test_only = names.to_vec();
        self
    }

    pub(crate) fn gdb_stub(mut self, on: bool) -> Self {
        self.gdb_stub = on;
        self
    }

    /// Enables the AI agent channel (virtio-serial; the host talks to the in-guest
    /// virtuoso-agent over this unix socket). The socket file must not exist (QEMU does
    /// not clean up stale paths).
    pub(crate) fn agent_serial(mut self, sock: impl Into<PathBuf>) -> Self {
        self.agent_serial = Some(sock.into());
        self
    }

    /// Attaches a persistent memory device (None = default, argv stays on the baseline).
    pub(crate) fn pmem(mut self, spec: Option<PmemSpec>) -> Self {
        self.pmem = spec;
        self
    }

    /// Appends a kernel cmdline text section (trimmed whole string joined in; empty =
    /// frozen baseline unchanged).
    pub(crate) fn cmdline_extra(mut self, extra: &str) -> Self {
        self.cmdline_extra = extra.trim().to_string();
        self
    }

    pub(crate) fn extra_opts(mut self, opts: &[String]) -> Self {
        self.extra_opts = opts.to_vec();
        self
    }

    pub(crate) fn qemu_bin(&self) -> String {
        self.qemu_override
            .clone()
            .unwrap_or_else(|| self.arch.qemu_bin().to_string())
    }

    /// Kernel cmdline (frozen run-qemu.sh text; the pmem component appends mem= to
    /// exclude the pmem region from the kernel's linear memory model, equivalent to
    /// x86 memmap= semantics; --only appends virtuoso.only= for init to filter /tests;
    /// the cmdline_extra text injection point joins last — kernel params are last-wins,
    /// appending overrides).
    pub(crate) fn cmdline(&self) -> String {
        let mut cmd = format!(
            "console={} root=/dev/vda rw init=/init loglevel=8",
            self.arch.console()
        );
        if self.auto_test {
            cmd.push_str(" auto_test");
        }
        if !self.test_only.is_empty() {
            cmd.push_str(&format!(" virtuoso.only={}", self.test_only.join(",")));
        }
        if let Some(pmem) = &self.pmem {
            cmd.push_str(&format!(" mem={}", pmem.mem_limit));
        }
        if !self.cmdline_extra.is_empty() {
            cmd.push(' ');
            cmd.push_str(&self.cmdline_extra);
        }
        cmd
    }

    /// Single-line copyable launch command (shell-quoted; shown before spawn / for
    /// manual reproduction).
    pub(crate) fn command_line(&self) -> anyhow::Result<String> {
        let mut parts = vec![self.qemu_bin()];
        parts.extend(self.argv()?.iter().map(|a| crate::util::quote(a)));
        Ok(parts.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmdline_without_auto_test_has_no_trailing_space() {
        let inv = QemuInvocation::new(Arch::Riscv64, "k", "i", "r");
        assert_eq!(
            inv.cmdline(),
            "console=ttyS0 root=/dev/vda rw init=/init loglevel=8"
        );
    }

    #[test]
    fn cmdline_test_only_appends_selection_and_empty_keeps_baseline() {
        let base = QemuInvocation::new(Arch::Arm64, "k", "i", "r");
        // empty default = verbatim-identical to the frozen baseline (invariant 3)
        assert_eq!(
            base.cmdline(),
            "console=ttyAMA0 root=/dev/vda rw init=/init loglevel=8"
        );
        let sel = base
            .clone()
            .auto_test(true)
            .test_only(&["test-a".to_string(), "test-b".to_string()]);
        assert_eq!(
            sel.cmdline(),
            "console=ttyAMA0 root=/dev/vda rw init=/init loglevel=8 auto_test virtuoso.only=test-a,test-b"
        );
    }

    #[test]
    fn cmdline_extra_appends_text_and_empty_keeps_baseline() {
        let base = QemuInvocation::new(Arch::Riscv64, "k", "i", "r");
        // empty default = verbatim-identical to the frozen baseline (invariant 3)
        assert_eq!(
            base.cmdline(),
            "console=ttyS0 root=/dev/vda rw init=/init loglevel=8"
        );
        // appended section joined whole at the end after trim (last-wins; overrides baseline params)
        let tweaked = base.clone().cmdline_extra("  nokaslr loglevel=4  ");
        assert_eq!(
            tweaked.cmdline(),
            "console=ttyS0 root=/dev/vda rw init=/init loglevel=8 nokaslr loglevel=4"
        );
    }

    #[test]
    fn command_line_quotes_append_value() {
        let inv = QemuInvocation::new(
            Arch::Arm64,
            "/tmp/vmlinuz",
            "/tmp/initrd.img",
            "/tmp/rootfs.img",
        )
        .host_os(HostOs::Linux);
        let args = inv.command_line().unwrap();
        // paths and comma-safe chars verbatim; the space-containing -append value is single-quoted whole
        assert!(args.contains("-drive file=/tmp/rootfs.img,format=raw,if=virtio"));
        assert!(args.contains("-append 'console=ttyAMA0 root=/dev/vda rw init=/init loglevel=8'"));
        assert!(args.starts_with("qemu-system-aarch64 -machine virt"));
    }
}
