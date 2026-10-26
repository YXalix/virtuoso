//! QEMU 启动参数 DSL —— run-qemu.sh 的强类型等价物。
//!
//! 模块划分：`accel`（加速器选择）、`argv`（argv 装配，冻结基线单测把守）、
//! `spawn`（进程拉起 + guardian 登记）；本文件承载调用面（结构体 + builder）。

mod accel;
mod argv;
mod spawn;

pub(crate) use accel::Accel;

use std::path::PathBuf;

use crate::{Arch, HostOs};

use crate::launcher::numa::NumaTopology;
pub(crate) use crate::launcher::{DataDisk, PmemSpec};

/// QEMU 启动参数。
#[derive(Debug, Clone)]
pub(crate) struct QemuInvocation {
    pub arch: Arch,
    pub qemu_override: Option<String>,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub rootfs: PathBuf,
    /// 额外数据盘（virtio-blk，追加于 rootfs 之后 → /dev/vdb 起），
    /// 如 tools.img（挂到 /tools 供 PATH 引用）。
    pub data_disks: Vec<DataDisk>,
    pub accel: Accel,
    /// 宿主平台：决定内存后端形态（Linux=memfd、macOS=ram）与 accel 合法性。
    /// 缺省取编译目标；argv_* 单测显式钉死以冻结双基线。
    pub host: HostOs,
    pub topo: NumaTopology,
    /// kernel cmdline 的 auto_test 开关
    pub auto_test: bool,
    /// 测例选择（`virtuoso test --only`）：非空时 cmdline 追加
    /// `virtuoso.only=<a,b>`，init 的 auto-test 只跑名单内的 /tests 二进制。
    /// 空缺省 argv 与冻结基线逐字一致。
    pub test_only: Vec<String>,
    /// `-s -S`：挂起等待 GDB 连接 :1234
    pub gdb_stub: bool,
    /// AI agent 通道：virtio-serial 端口，宿主侧 unix socket（chardev）。
    /// Some 时追加 chardev + virtio-serial-pci + virtserialport；None（缺省）
    /// argv 与 run-qemu.sh 基线逐字不变（冻结不变量 3）。
    pub agent_serial: Option<PathBuf>,
    /// 持久内存组件（DT 途径）：Some 时主内存后端换成宿主文件（share=on，
    /// pmem 区写入持久落盘）并追加 -dtb（补丁后的设备树，含 pmem-region
    /// 节点）；None（缺省）argv 与基线逐字不变（冻结不变量 3）。
    /// 仅单节点 NUMA 支持；x86_64 无 DT 由调用方拒绝。
    pub pmem: Option<PmemSpec>,
    /// 内核 cmdline 追加段（env `CMDLINE_EXTRA` / toml `cmdline_extra` 的
    /// 文本注入口）：原样拼在机器拼装段之后；空缺省 argv 与冻结基线
    /// 逐字一致（冻结不变量 3）。
    pub cmdline_extra: String,
    /// 原样透传（shell 展开语义：按空白切分）
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

    /// 钉死宿主平台（argv 冻结单测用；运行路径缺省取编译目标）。
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

    /// 附加一个 virtio-blk 数据盘（按调用顺序出现在 /dev/vdb、/dev/vdc…）。
    #[cfg(test)]
    pub(crate) fn virtio_disk(mut self, disk: DataDisk) -> Self {
        self.data_disks.push(disk);
        self
    }

    /// 批量附加数据盘（接受 Option / 迭代器，空缺省不变）。
    pub(crate) fn virtio_disks(mut self, disks: impl IntoIterator<Item = DataDisk>) -> Self {
        self.data_disks.extend(disks);
        self
    }

    pub(crate) fn auto_test(mut self, on: bool) -> Self {
        self.auto_test = on;
        self
    }

    /// 测例选择（空 = 全跑；调用方负责名字合法性：非空、无逗号/空白）。
    pub(crate) fn test_only(mut self, names: &[String]) -> Self {
        self.test_only = names.to_vec();
        self
    }

    #[cfg(test)]
    pub(crate) fn gdb_stub(mut self, on: bool) -> Self {
        self.gdb_stub = on;
        self
    }

    /// 启用 AI agent 通道（virtio-serial；宿主通过该 unix socket 与 guest
    /// 内 virtuoso-agent 通信）。socket 文件须不存在（QEMU 不会清理旧路径）。
    pub(crate) fn agent_serial(mut self, sock: impl Into<PathBuf>) -> Self {
        self.agent_serial = Some(sock.into());
        self
    }

    /// 附加持久内存设备（None = 缺省，argv 保持基线）。
    pub(crate) fn pmem(mut self, spec: Option<PmemSpec>) -> Self {
        self.pmem = spec;
        self
    }

    /// 追加内核 cmdline 文本段（trim 后整串拼入；空 = 冻结基线不变）。
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

    /// 内核 cmdline（run-qemu.sh 冻结文本；pmem 组件追加 mem= 把 pmem 区
    /// 从内核线性内存模型中排除，等价 x86 memmap= 语义；--only 追加
    /// virtuoso.only= 供 init 过滤 /tests；cmdline_extra 文本注入口拼在
    /// 最末——内核参数 last-wins，追加即覆盖）。
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

    /// 单行可复制启动命令（shell 引用；spawn 前展示 / 手动复现用）。
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
        // 空缺省 = 冻结基线逐字一致（不变量 3）
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
        // 空缺省 = 冻结基线逐字一致（不变量 3）
        assert_eq!(
            base.cmdline(),
            "console=ttyS0 root=/dev/vda rw init=/init loglevel=8"
        );
        // 追加段 trim 后整串拼在最末（last-wins，追加即覆盖基线同名参数）
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
        // 路径与逗号安全字符原样；含空格的 -append 值整体单引号
        assert!(args.contains("-drive file=/tmp/rootfs.img,format=raw,if=virtio"));
        assert!(args.contains("-append 'console=ttyAMA0 root=/dev/vda rw init=/init loglevel=8'"));
        assert!(args.starts_with("qemu-system-aarch64 -machine virt"));
    }
}
