//! argv assembly: the full argv matches run-qemu.sh's expansion order verbatim
//! (machine, memory-backend, numa, kvm, cpu, smp, m, kernel, initrd,
//! append, rootfs drive, data disks, extra, console, options, debug).

use crate::HostOs;

use super::{Accel, QemuInvocation};

impl QemuInvocation {
    /// Full argv, matching run-qemu.sh's expansion order verbatim:
    /// machine, memory-backend, numa, kvm, cpu, smp, m, kernel, initrd,
    /// append, rootfs drive, data disks, extra, console, options, debug.
    pub(crate) fn argv(&self) -> anyhow::Result<Vec<String>> {
        // accel × host-platform validity (errors surface at argv construction, not left to QEMU)
        match (self.accel, self.host) {
            (Accel::Kvm, HostOs::Darwin) => {
                anyhow::bail!(
                    "KVM requires a Linux host (on macOS the hardware accelerator is HVF)"
                );
            }
            (Accel::Hvf, HostOs::Linux) => {
                anyhow::bail!(
                    "HVF requires a macOS host (on Linux the hardware accelerator is KVM)"
                );
            }
            _ => {}
        }
        let total_mem = self.topo.total_memory()?;
        let mut args: Vec<String> = Vec::new();

        self.push_machine_and_memory(&mut args, &total_mem)?;
        self.push_accel_cpu_smp(&mut args);
        self.push_boot_and_drives(&mut args, &total_mem);
        self.push_agent_channel(&mut args);
        self.push_tail(&mut args);
        Ok(args)
    }

    /// -machine + memory backend (single node: memory-backend=mem linked on the machine /
    /// multi-node: per-node -object+-numa / pmem: swap to a file backend with share=on).
    /// The Linux baseline freezes on the memfd backend; macOS QEMU lacks memfd_create,
    /// so ram is used (equivalent shape: both are plain anonymous memory; with ram the
    /// share semantics are always off and need no explicit declaration).
    fn mem_backend(&self, id: &str, size: &str) -> String {
        match self.host {
            HostOs::Linux => format!("memory-backend-memfd,id={id},size={size},share=off"),
            HostOs::Darwin => format!("memory-backend-ram,id={id},size={size}"),
        }
    }

    fn push_machine_and_memory(
        &self,
        args: &mut Vec<String>,
        total_mem: &str,
    ) -> anyhow::Result<()> {
        args.push("-machine".into());
        if self.topo.nodes > 1 {
            if self.pmem.is_some() {
                anyhow::bail!("the pmem component does not support multi-node NUMA yet (single-node topology kept)");
            }
            args.push(self.arch.machine().into());
        } else {
            args.push(format!("{},memory-backend=mem", self.arch.machine()));
        }

        if self.topo.nodes > 1 {
            let per_node = self.topo.smp / self.topo.nodes;
            for i in 0..self.topo.nodes {
                args.push("-object".into());
                args.push(self.mem_backend(&format!("mem{i}"), &self.topo.memory_per_node));
                let start = i * per_node;
                let end = start + per_node - 1;
                args.push("-numa".into());
                args.push(format!("node,nodeid={i},memdev=mem{i},cpus={start}-{end}"));
            }
        } else if let Some(pmem) = &self.pmem {
            // main memory swaps to a file backend (share=on): the pmem region carved out
            // of DT is host-file backed, so guest writes persist to disk (same shape on
            // both platforms)
            args.push("-object".into());
            args.push(format!(
                "memory-backend-file,id=mem,mem-path={},size={total_mem},share=on",
                pmem.ram_backend.display()
            ));
        } else {
            args.push("-object".into());
            args.push(self.mem_backend("mem", total_mem));
        }
        Ok(())
    }

    /// Accelerator switches + -cpu + -smp.
    fn push_accel_cpu_smp(&self, args: &mut Vec<String>) {
        match self.accel {
            Accel::Kvm => args.push("-enable-kvm".into()),
            Accel::Hvf => {
                args.push("-accel".into());
                args.push("hvf".into());
            }
            Accel::Tcg => {}
        }
        args.push("-cpu".into());
        args.push(match self.accel {
            Accel::Kvm | Accel::Hvf => "host".into(),
            Accel::Tcg => self.arch.cpu_tcg().into(),
        });

        args.push("-smp".into());
        if self.topo.nodes > 1 {
            let per_node = self.topo.smp / self.topo.nodes;
            args.push(format!(
                "{},sockets={},cores={},threads=1",
                self.topo.smp, self.topo.nodes, per_node
            ));
        } else {
            args.push(self.topo.smp.to_string());
        }
    }

    /// -m / -kernel / -initrd / -append / (pmem -dtb) / rootfs drive / data disks.
    fn push_boot_and_drives(&self, args: &mut Vec<String>, total_mem: &str) {
        args.push("-m".into());
        args.push(total_mem.to_string());
        args.push("-kernel".into());
        args.push(self.kernel.display().to_string());
        args.push("-initrd".into());
        args.push(self.initrd.display().to_string());
        args.push("-append".into());
        args.push(self.cmdline());

        if let Some(pmem) = &self.pmem {
            // patched device tree (pmem region carved out of /memory + root pmem-region
            // node), replacing the QEMU-generated one — of_pmem registers /dev/pmem0 from it
            args.push("-dtb".into());
            args.push(pmem.dtb.display().to_string());
        }

        args.push("-drive".into());
        args.push(format!(
            "file={},format=raw,if=virtio",
            self.rootfs.display()
        ));

        for disk in &self.data_disks {
            args.push("-drive".into());
            args.push(format!("file={},format=raw,if=virtio", disk.path.display()));
        }
    }

    /// AI agent channel (virtio-serial chardev + port device).
    fn push_agent_channel(&self, args: &mut Vec<String>) {
        if let Some(sock) = &self.agent_serial {
            args.push("-chardev".into());
            args.push(format!(
                "socket,id=agentch,path={},server=on,wait=off",
                sock.display()
            ));
            args.push("-device".into());
            args.push("virtio-serial-pci,id=virtio-serial0".into());
            args.push("-device".into());
            args.push("virtserialport,chardev=agentch,id=agentport,name=virtuoso-agent".into());
        }
    }

    /// extra_opts passthrough and closing args (console / -no-reboot / gdb stub).
    fn push_tail(&self, args: &mut Vec<String>) {
        for opt in &self.extra_opts {
            args.extend(opt.split_whitespace().map(str::to_string));
        }

        args.push("-nographic".into());
        args.push("-serial".into());
        args.push("mon:stdio".into());
        args.push("-no-reboot".into());

        if self.gdb_stub {
            args.push("-s".into());
            args.push("-S".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::{DataDisk, PmemSpec};
    use crate::launcher::numa::NumaTopology;
    use crate::{Arch, HostOs};

    fn base_inv() -> QemuInvocation {
        QemuInvocation::new(
            Arch::Arm64,
            "/tmp/vmlinuz",
            "/tmp/initrd.img",
            "/tmp/rootfs.img",
        )
        .topo(NumaTopology::parse("8", "2", "1G").unwrap())
        // the frozen baseline is per host platform; unit tests pin it, no drift with the target
        .host_os(HostOs::Linux)
    }

    #[test]
    fn argv_frozen_baseline_multi_node() {
        let args = base_inv().auto_test(true).argv().unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-machine virt "));
        assert!(joined.contains("-object memory-backend-memfd,id=mem0,size=1G,share=off"));
        assert!(joined.contains("-numa node,nodeid=0,memdev=mem0,cpus=0-3"));
        assert!(joined.contains("-numa node,nodeid=1,memdev=mem1,cpus=4-7"));
        assert!(joined.contains("-smp 8,sockets=2,cores=4,threads=1"));
        assert!(joined.contains("-cpu cortex-a72"));
        assert!(joined.contains("-m 2G"));
        assert!(joined
            .contains("-append console=ttyAMA0 root=/dev/vda rw init=/init loglevel=8 auto_test"));
        assert!(joined.contains("-drive file=/tmp/rootfs.img,format=raw,if=virtio"));
        assert!(joined.contains("-nographic -serial mon:stdio -no-reboot"));
        // ordering constraint: machine comes first, -no-reboot after console
        assert_eq!(args[0], "-machine");
        let pos = |p: &str| args.iter().position(|a| a == p).unwrap();
        assert!(pos("-serial") < pos("-no-reboot"));
    }

    #[test]
    fn argv_single_node_uses_memory_backend_on_machine() {
        let inv = QemuInvocation::new(Arch::X86_64, "k", "i", "r")
            .topo(NumaTopology::parse("4", "1", "2G").unwrap())
            .host_os(HostOs::Linux);
        let args = inv.argv().unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-machine virt,memory-backend=mem"));
        assert!(joined.contains("memory-backend-memfd,id=mem,size=2G,share=off"));
        assert!(joined.contains("-smp 4"));
        assert!(joined.contains("-cpu qemu64"));
        assert!(joined.contains("console=ttyS0"));
        assert!(!joined.contains("-numa"));
    }

    #[test]
    fn kvm_uses_host_cpu_and_enable_flag() {
        let args = base_inv().accel(Accel::Kvm).argv().unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-enable-kvm -cpu host"));
        assert!(!joined.contains("cortex-a72"));
    }

    #[test]
    fn argv_darwin_frozen_baseline_uses_ram_backend() {
        // darwin baseline: no memfd (macOS QEMU is not built with that backend); both the
        // single node and each NUMA node switch to memory-backend-ram (ram is never
        // shared, no share= field)
        let args = base_inv()
            .host_os(HostOs::Darwin)
            .topo(NumaTopology::parse("8", "1", "2G").unwrap())
            .auto_test(true)
            .argv()
            .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-machine virt,memory-backend=mem"));
        assert!(joined.contains("-object memory-backend-ram,id=mem,size=2G"));
        assert!(!joined.contains("memfd"));
        assert!(!joined.contains("share=off"));
        assert!(joined.contains("-cpu cortex-a72"));
    }

    #[test]
    fn argv_darwin_numa_uses_ram_backend_per_node() {
        let args = base_inv().host_os(HostOs::Darwin).argv().unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-object memory-backend-ram,id=mem0,size=1G"));
        assert!(joined.contains("-object memory-backend-ram,id=mem1,size=1G"));
        assert!(!joined.contains("memfd"));
    }

    #[test]
    fn hvf_uses_accel_flag_and_host_cpu() {
        let args = base_inv()
            .host_os(HostOs::Darwin)
            .accel(Accel::Hvf)
            .argv()
            .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-accel hvf -cpu host"));
        assert!(!joined.contains("cortex-a72"));
        assert!(!joined.contains("-enable-kvm"));
    }

    #[test]
    fn kvm_rejected_on_darwin_and_hvf_rejected_on_linux() {
        let kvm_on_mac = base_inv()
            .host_os(HostOs::Darwin)
            .accel(Accel::Kvm)
            .argv()
            .unwrap_err();
        assert!(kvm_on_mac.to_string().contains("HVF"));
        let hvf_on_linux = base_inv().accel(Accel::Hvf).argv().unwrap_err();
        assert!(hvf_on_linux.to_string().contains("KVM"));
    }

    #[test]
    fn virtio_disk_and_gdb_and_extra_opts() {
        let args = base_inv()
            .virtio_disk(DataDisk::new("/tmp/tools.img"))
            .gdb_stub(true)
            .extra_opts(&["-device vfio-pci,host=00:01.0".to_string()])
            .argv()
            .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-drive file=/tmp/tools.img,format=raw,if=virtio"));
        // NVMe test disk removed: no blockdev/nvme devices anymore
        assert!(!joined.contains("-blockdev"));
        assert!(!joined.contains("nvme"));
        assert!(joined.contains("-s -S"));
        assert!(joined.contains("-device vfio-pci,host=00:01.0"));
        // ordering: data disks right after the rootfs drive, before extra_opts
        let pos = |p: &str| args.iter().position(|a| a.contains(p)).unwrap();
        assert!(pos("file=/tmp/rootfs.img") < pos("file=/tmp/tools.img"));
    }

    #[test]
    fn multiple_data_disks_append_in_order() {
        let args = base_inv()
            .virtio_disk(DataDisk::new("/tmp/tools.img"))
            .virtio_disk(DataDisk::new("/tmp/data0.img"))
            .argv()
            .unwrap();
        let joined = args.join(" ");
        assert!(joined
            .contains("-drive file=/tmp/rootfs.img,format=raw,if=virtio -drive file=/tmp/tools.img,format=raw,if=virtio -drive file=/tmp/data0.img,format=raw,if=virtio"));
    }

    #[test]
    fn data_disks_coexist_with_agent_serial() {
        let args = base_inv()
            .virtio_disk(DataDisk::new("/tmp/tools.img"))
            .agent_serial("/tmp/virtuoso-agent.sock")
            .argv()
            .unwrap();
        let pos = |p: &str| args.iter().position(|a| a.contains(p)).unwrap();
        assert!(pos("-drive") < pos("-chardev"));
    }

    #[test]
    fn no_data_disks_keeps_baseline_argv() {
        // frozen invariant 3: default (no data disks) argv matches the run-qemu.sh baseline verbatim
        let args = base_inv().argv().unwrap();
        let joined = args.join(" ");
        assert_eq!(
            joined.matches("-drive").count(),
            1,
            "only the rootfs -drive is present"
        );
        assert!(!joined.contains("-blockdev"));
        assert!(!joined.contains("nvdimm"), "nvdimm must not appear by default");
    }

    #[test]
    fn pmem_swaps_ram_backend_adds_dtb_and_mem_limit() {
        // single node: main memory swaps to a file backend (share=on) + -dtb + cmdline mem=
        let inv = QemuInvocation::new(Arch::X86_64, "k", "i", "r")
            .topo(NumaTopology::parse("4", "1", "2G").unwrap())
            .host_os(HostOs::Linux)
            .pmem(Some(PmemSpec::new(
                "256M",
                "1792M",
                "/tmp/ram.img",
                "/tmp/virt.dtb",
            )));
        let args = inv.argv().unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-machine virt,memory-backend=mem"));
        assert!(!joined.contains("nvdimm"), "the DT route uses no QEMU nvdimm device");
        assert!(joined
            .contains("-object memory-backend-file,id=mem,mem-path=/tmp/ram.img,size=2G,share=on"));
        assert!(joined.contains("-dtb /tmp/virt.dtb"));
        assert!(joined.contains("mem=1792M"));
        // ordering: -dtb after -append, before -drive
        let pos = |p: &str| args.iter().position(|a| a.contains(p)).unwrap();
        assert!(pos("-append") < pos("-dtb"));
        assert!(pos("-dtb") < pos("-drive"));
    }

    #[test]
    fn pmem_rejected_with_multi_node_numa() {
        let err = base_inv()
            .pmem(Some(PmemSpec::new(
                "256M",
                "1792M",
                "/tmp/ram.img",
                "/tmp/virt.dtb",
            )))
            .argv()
            .unwrap_err();
        assert!(err.to_string().contains("NUMA"));
    }

    #[test]
    fn agent_serial_off_by_default_keeps_baseline_argv() {
        // frozen invariant 3: with the agent channel off, argv is verbatim-identical to the baseline
        let joined = base_inv().argv().unwrap().join(" ");
        assert!(!joined.contains("virtio-serial"));
        assert!(!joined.contains("chardev"));
    }

    #[test]
    fn agent_serial_adds_chardev_and_virtserialport() {
        let args = base_inv()
            .agent_serial("/tmp/virtuoso-agent.sock")
            .argv()
            .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains(
            "-chardev socket,id=agentch,path=/tmp/virtuoso-agent.sock,server=on,wait=off"
        ));
        assert!(joined.contains("-device virtio-serial-pci,id=virtio-serial0"));
        assert!(joined
            .contains("-device virtserialport,chardev=agentch,id=agentport,name=virtuoso-agent"));
        // ordering constraint: agent devices after drives, before -nographic
        let pos = |p: &str| args.iter().position(|a| a.contains(p)).unwrap();
        assert!(pos("-drive") < pos("-chardev"));
        assert!(pos("virtserialport") < pos("-nographic"));
    }
}
