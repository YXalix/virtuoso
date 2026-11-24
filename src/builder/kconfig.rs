//! Kernel `.config` requirements (the knowledge behind `virtuoso kernel
//! check-config`).
//!
//! Virtuoso boots the kernel under test in QEMU: the boot chain (initramfs →
//! virtio root disk → ext4 rootfs → serial console) and each VM component
//! ([components.*]) depend on specific Kconfig symbols. This module is the
//! single structured source of that dependency knowledge: a static
//! requirement table plus pure evaluation functions over a parsed `.config`.
//! Version drift (4.19 → mainline) is captured per entry via `since` + `note`
//! — e.g. the virtio-pci module split (5.15/5.16) and VFIO_PCI_CORE (5.15).
//! The narrative reference with the full version matrix lives in
//! `docs/usage/kernel-config.md`.

use std::collections::BTreeMap;

use crate::Arch;

/// Which part of the harness needs the symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Group {
    /// Required to reach root= and a working console (the two-stage boot chain).
    Boot,
    /// Required only when the named VM component (or hook recipe) is used.
    Component(&'static str),
}

/// How the symbol must be enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Need {
    /// Must be built in (=y); a module cannot satisfy the boot path here.
    Builtin,
    /// =y or =m both work; =m lands in a module manifest (boot set or runtime).
    BuiltinOrModule,
}

/// One Kconfig dependency of the harness.
pub(crate) struct Req {
    /// Symbol name without the CONFIG_ prefix.
    pub symbol: &'static str,
    /// Module (.ko) stems the symbol builds when =m (empty for pure bools;
    /// CONFIG_VIRTIO alone builds virtio.ko + virtio_ring.ko).
    pub modules: &'static [&'static str],
    pub group: Group,
    /// Arches the entry applies to (empty = all).
    pub arches: &'static [Arch],
    pub need: Need,
    /// First mainline version (major, minor) defining the symbol; older
    /// kernels simply do not have it (the functionality lives elsewhere).
    pub since: Option<(u32, u32)>,
    /// Purpose + version guidance (English, user-visible).
    pub note: &'static str,
}

const ARM64: &[Arch] = &[Arch::Arm64];
const X86_RISCV: &[Arch] = &[Arch::X86_64, Arch::Riscv64];
const ARM64_RISCV: &[Arch] = &[Arch::Arm64, Arch::Riscv64];
const ALL: &[Arch] = &[];

// ---------------------------------------------------------------- table constructors
// Small const-fn constructors keep the table data-like: each row states only
// what varies (symbol / modules / note); the common shape is named once.

/// Boot-chain entry, all arches, no version gate; =y or =m both work.
const fn boot(symbol: &'static str, modules: &'static [&'static str], note: &'static str) -> Req {
    Req { symbol, modules, group: Group::Boot, arches: ALL, need: Need::BuiltinOrModule, since: None, note }
}

/// Boot-chain bool that must be builtin (=y), all arches.
const fn boot_builtin(symbol: &'static str, note: &'static str) -> Req {
    Req { symbol, modules: &[], group: Group::Boot, arches: ALL, need: Need::Builtin, since: None, note }
}

/// Boot-chain bool limited to specific arches (console rows, PCI host).
const fn boot_on(arches: &'static [Arch], symbol: &'static str, note: &'static str) -> Req {
    Req { symbol, modules: &[], group: Group::Boot, arches, need: Need::Builtin, since: None, note }
}

/// Boot-chain entry whose symbol only exists since a given mainline version.
const fn boot_since(symbol: &'static str, modules: &'static [&'static str], since: (u32, u32), note: &'static str) -> Req {
    Req { symbol, modules, group: Group::Boot, arches: ALL, need: Need::BuiltinOrModule, since: Some(since), note }
}

/// Component entry, all arches, no version gate; =y or =m both work.
const fn comp(name: &'static str, symbol: &'static str, modules: &'static [&'static str], note: &'static str) -> Req {
    Req { symbol, modules, group: Group::Component(name), arches: ALL, need: Need::BuiltinOrModule, since: None, note }
}

/// Component bool (=y), all arches, no version gate.
const fn comp_builtin(name: &'static str, symbol: &'static str, note: &'static str) -> Req {
    Req { symbol, modules: &[], group: Group::Component(name), arches: ALL, need: Need::Builtin, since: None, note }
}

/// Component entry whose symbol only exists since a given mainline version.
const fn comp_since(name: &'static str, symbol: &'static str, modules: &'static [&'static str], need: Need, since: (u32, u32), note: &'static str) -> Req {
    Req { symbol, modules, group: Group::Component(name), arches: ALL, need, since: Some(since), note }
}

/// Component entry limited to specific arches.
const fn comp_on(name: &'static str, arches: &'static [Arch], symbol: &'static str, modules: &'static [&'static str], note: &'static str) -> Req {
    Req { symbol, modules, group: Group::Component(name), arches, need: Need::BuiltinOrModule, since: None, note }
}

/// The requirement table. Boot entries first (console rows included), then
/// per-component entries. Consistency with the frozen `infra/modules-boot.conf`
/// base set is pinned by the `boot_manifest_matches_table` unit test.
static REQUIREMENTS: &[Req] = &[
    // ---------------------------------------------------------- boot chain
    boot_builtin("MODULES", "module loading (insmod) — every =m entry below depends on it"),
    boot_builtin("BLK_DEV_INITRD", "initramfs support for -initrd (phase 1 boot)"),
    boot_builtin("RD_GZIP", "initrd.img is a gzip-compressed newc archive"),
    boot("VIRTIO", &["virtio", "virtio_ring"], "virtio bus core (one symbol builds both virtio.ko and virtio_ring.ko)"),
    boot("VIRTIO_PCI", &["virtio_pci"], "QEMU attaches if=virtio disks as virtio-blk-pci on all three arches"),
    boot_since("VIRTIO_PCI_LIB", &["virtio_pci_modern_dev"], (5, 15),
        "modern virtio-pci core, auto-selected by VIRTIO_PCI; split out of virtio_pci.ko in 5.15 — on <=5.10 drop virtio_pci_modern_dev from infra/modules-boot.conf"),
    boot_since("VIRTIO_PCI_LIB_LEGACY", &["virtio_pci_legacy_dev"], (5, 16),
        "legacy virtio-pci core, auto-selected by VIRTIO_PCI_LEGACY (default y); split out in 5.16 — on <=5.15 drop virtio_pci_legacy_dev from infra/modules-boot.conf"),
    boot("VIRTIO_BLK", &["virtio_blk"], "block device for rootfs.img (/dev/vda) and tools.img (/dev/vdb)"),
    boot_on(ARM64_RISCV, "PCI", "PCIe host for virtio-pci (x86_64 selects PCI unconditionally)"),
    boot_on(ARM64_RISCV, "PCI_HOST_GENERIC", "generic ECAM host bridge of the QEMU virt machine"),
    boot("EXT4_FS", &["ext4"], "rootfs.img filesystem"),
    boot("JBD2", &["jbd2"], "ext4 journaling (selected by EXT4_FS)"),
    boot("FS_MBCACHE", &["mbcache"], "ext4 xattr block cache; hidden tristate, defaults follow EXT4_FS (=m when ext4=m)"),
    boot("CRC16", &["crc16"], "ext4 metadata checksums, selected by EXT4_FS; openEuler configs force =y (builtin — the builder skips it via modules.builtin)"),
    // console: arm64 = PL011 ttyAMA0, x86_64/riscv64 = 8250 ttyS0
    boot_on(ARM64, "SERIAL_AMBA_PL011", "PL011 UART of the QEMU virt machine (console=ttyAMA0)"),
    boot_on(ARM64, "SERIAL_AMBA_PL011_CONSOLE", "console= registration on the PL011 UART"),
    boot_on(X86_RISCV, "SERIAL_8250", "8250 UART (console=ttyS0)"),
    boot_on(X86_RISCV, "SERIAL_8250_CONSOLE", "console= registration on the 8250 UART"),
    // pseudo filesystems mounted by both init stages
    boot_builtin("DEVTMPFS", "/dev population; pair with DEVTMPFS_MOUNT=y (the initramfs falls back to mknod + mdev -s otherwise)"),
    boot_builtin("PROC_FS", "/proc (root= parsing, /proc/partitions fallback)"),
    boot_builtin("SYSFS", "/sys (module parameters, block device discovery)"),
    boot_builtin("TMPFS", "/tmp and /dev/shm"),
    // devpts needs no entry: unconditional since 2.6.4 (CONFIG_DEVPTS_FS
    // was removed then); `mount -t devpts` always works on 4.19+.
    // ---------------------------------------------------------- components
    comp("agent", "VIRTIO_CONSOLE", &["virtio_console"],
        "virtio-serial port driver for the probe channel; `virtuoso probe` force-merges virtio_console regardless of the component switch"),
    comp("vfio", "VFIO", &["vfio"], "vfio core (guest side of PCI passthrough)"),
    comp("vfio", "VFIO_PCI", &["vfio_pci"], "PCI passthrough driver; on >=5.15 list vfio_pci_core before it in require"),
    comp_since("vfio", "VFIO_PCI_CORE", &["vfio_pci_core"], Need::BuiltinOrModule, (5, 15),
        "split out of vfio_pci.ko in 5.15 (hidden, auto-selected by VFIO_PCI)"),
    comp("vfio", "VFIO_IOMMU_TYPE1", &["vfio_iommu_type1"], "IOMMU backend for the passthrough group (x86 VT-d/AMD-Vi, ARM SMMU)"),
    comp("pmem", "LIBNVDIMM", &["libnvdimm"], "nvdimm core (present across 4.19..mainline)"),
    comp("pmem", "BLK_DEV_PMEM", &["nd_pmem"], "builds nd_pmem.ko — /dev/pmem0 (symbol name differs from the module name)"),
    comp("pmem", "ND_BTT", &["nd_btt"], "hidden tristate; selected by BLK_DEV_PMEM when BTT=y (default)"),
    comp_on("pmem", ARM64_RISCV, "OF_PMEM", &["of_pmem"],
        "device-tree pmem-region binding — the pmem component's DT route (x86_64 NFIT route unsupported)"),
    comp_builtin("pmem", "FS_DAX", "mount -o dax (fsdax) on ext4 over /dev/pmem0"),
    comp_builtin("bpf", "BPF_SYSCALL", "eBPF syscall base for bpftrace / bpf-run / bpftool"),
    comp_since("bpf", "DEBUG_INFO_BTF", &[], Need::Builtin, (5, 2),
        "kernel BTF (/sys/kernel/btf/vmlinux); introduced in 5.2 — bpf observability effectively needs >=5.2"),
    comp_builtin("bpf", "KPROBES", "kprobe: probe family"),
    comp_builtin("bpf", "KPROBE_EVENTS", "kprobe events on tracefs/perf"),
    comp_builtin("bpf", "BPF_EVENTS", "BPF attach onto perf events"),
    comp_builtin("bpf", "PERF_EVENTS", "perf PMU backend for kprobe/tracepoint attach"),
    comp_builtin("bpf", "TRACING", "tracefs (/sys/kernel/tracing) for tracepoint: probes (no separate TRACEFS_FS symbol exists)"),
    comp_builtin("bpf", "DEBUG_FS", "debugfs (mounted best-effort by the test init)"),
    comp_builtin("numa", "NUMA", "guest NUMA topology; x86_64 always, arm64 since 4.7, riscv64 since 5.12"),
    comp_builtin("hooks", "HUGETLBFS", "hugetlbfs mount + nr_hugepages (the init-hooks.sh recipe in docs/concepts/boot.md); HUGETLB_PAGE is auto-selected"),
    ];

pub(crate) fn requirements() -> &'static [Req] {
    REQUIREMENTS
}

/// Look up the table entry owning a module stem (for manifest diagnostics).
pub(crate) fn req_for_module(module: &str) -> Option<&'static Req> {
    requirements().iter().find(|r| r.modules.contains(&module))
}

// ---------------------------------------------------------------- .config parsing

/// Value of a CONFIG_ symbol in a .config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tristate {
    Y,
    M,
    /// `# CONFIG_X is not set` (or a bool/tristate absent from .config).
    Unset,
}

/// Parses .config text into symbol → tristate. Non-bool values (strings,
/// ints) map to Y ("present"); comment/blank lines are skipped.
pub(crate) fn parse_dot_config(text: &str) -> BTreeMap<String, Tristate> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("# CONFIG_") {
            if let Some(name) = rest.strip_suffix(" is not set") {
                map.insert(name.to_string(), Tristate::Unset);
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("CONFIG_") else {
            continue;
        };
        let Some((name, value)) = rest.split_once('=') else {
            continue;
        };
        let t = match value {
            "y" => Tristate::Y,
            "m" => Tristate::M,
            _ => Tristate::Y, // string/int/hex values count as present
        };
        map.insert(name.to_string(), t);
    }
    map
}

/// Kernel version from the top-level Makefile (VERSION/PATCHLEVEL/SUBLEVEL).
pub(crate) fn version_from_makefile(text: &str) -> Option<(u32, u32, u32)> {
    let mut major = None;
    let mut minor = None;
    let mut patch = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Ok(n) = value.trim().parse::<u32>() else {
            continue;
        };
        match key.trim() {
            "VERSION" => major = Some(n),
            "PATCHLEVEL" => minor = Some(n),
            "SUBLEVEL" => patch = Some(n),
            _ => {}
        }
        if major.is_some() && minor.is_some() && patch.is_some() {
            break;
        }
    }
    Some((major?, minor?, patch?))
}

// ---------------------------------------------------------------- evaluation

/// Outcome of checking one requirement against a .config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// =y.
    Builtin,
    /// =m (module; lands in a manifest).
    Module,
    /// Symbol exists in this kernel version but is not enabled.
    Missing,
    /// The kernel predates the symbol (since); the functionality lives in
    /// another module/symbol there — informational, never a failure.
    NotYet,
}

pub(crate) struct Finding {
    pub req: &'static Req,
    pub status: Status,
}

impl Finding {
    /// Boot-group gaps always fail; component gaps fail only when the
    /// component is enabled in virtuoso.toml.
    pub(crate) fn is_failure(&self, component_enabled: bool) -> bool {
        match self.status {
            Status::Missing => match self.req.group {
                Group::Boot => true,
                Group::Component(_) => component_enabled,
            },
            _ => false,
        }
    }
}

/// True when `version` (major, minor, patch) predates `since` (major, minor).
pub(crate) fn predates(version: (u32, u32, u32), since: (u32, u32)) -> bool {
    (version.0, version.1) < since
}

/// Evaluates the table against a parsed .config for one arch/version.
/// Entries for other arches are skipped.
pub(crate) fn evaluate(
    config: &BTreeMap<String, Tristate>,
    version: (u32, u32, u32),
    arch: Arch,
) -> Vec<Finding> {
    requirements()
        .iter()
        .filter(|req| req.arches.is_empty() || req.arches.contains(&arch))
        .map(|req| {
            let status = if req.since.is_some_and(|s| predates(version, s)) {
                Status::NotYet
            } else {
                match config.get(req.symbol) {
                    Some(Tristate::Y) => Status::Builtin,
                    Some(Tristate::M) => Status::Module,
                    _ => Status::Missing,
                }
            };
            Finding { req, status }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dot_config_handles_y_m_unset_and_values() {
        let text = "\
CONFIG_VIRTIO=y
CONFIG_VIRTIO_BLK=m
# CONFIG_EXT4_FS is not set
CONFIG_LOCALVERSION=\"-oe\"
CONFIG_NR_CPUS=256
# a plain comment
";
        let map = parse_dot_config(text);
        assert_eq!(map.get("VIRTIO"), Some(&Tristate::Y));
        assert_eq!(map.get("VIRTIO_BLK"), Some(&Tristate::M));
        assert_eq!(map.get("EXT4_FS"), Some(&Tristate::Unset));
        assert_eq!(map.get("LOCALVERSION"), Some(&Tristate::Y));
        assert_eq!(map.get("NR_CPUS"), Some(&Tristate::Y));
        assert!(!map.contains_key("a plain comment"));
    }

    #[test]
    fn version_from_makefile_parses_triple() {
        let text = "# SPDX\nVERSION = 6\nPATCHLEVEL = 6\nSUBLEVEL = 0\nEXTRAVERSION =\nNAME = x\n";
        assert_eq!(version_from_makefile(text), Some((6, 6, 0)));
        let text419 = "VERSION = 4\nPATCHLEVEL = 19\nSUBLEVEL = 90\n";
        assert_eq!(version_from_makefile(text419), Some((4, 19, 90)));
        assert_eq!(version_from_makefile("VERSION = x\n"), None);
    }

    #[test]
    fn evaluate_gates_symbols_by_since_and_arch() {
        // A 4.19 tree with virtio_pci=m and no split symbols: the split
        // entries must report NotYet (informational), not Missing.
        let mut config = BTreeMap::new();
        config.insert("VIRTIO".into(), Tristate::M);
        config.insert("VIRTIO_PCI".into(), Tristate::M);
        config.insert("VIRTIO_BLK".into(), Tristate::M);
        config.insert("EXT4_FS".into(), Tristate::M);
        let findings = evaluate(&config, (4, 19, 0), Arch::Arm64);
        let status = |sym: &str| {
            findings
                .iter()
                .find(|f| f.req.symbol == sym)
                .unwrap_or_else(|| panic!("{sym} missing from findings"))
                .status
        };
        assert_eq!(status("VIRTIO_PCI_LIB"), Status::NotYet);
        assert_eq!(status("VIRTIO_PCI_LIB_LEGACY"), Status::NotYet);
        assert_eq!(status("VIRTIO_PCI"), Status::Module);
        assert_eq!(status("MODULES"), Status::Missing);
        // arm64 console rows apply, x86 rows are filtered out
        assert!(findings.iter().any(|f| f.req.symbol == "SERIAL_AMBA_PL011"));
        assert!(!findings.iter().any(|f| f.req.symbol == "SERIAL_8250"));
        // a 6.6 tree has the split symbols
        let findings66 = evaluate(&config, (6, 6, 0), Arch::Arm64);
        let lib = findings66
            .iter()
            .find(|f| f.req.symbol == "VIRTIO_PCI_LIB")
            .unwrap();
        assert_eq!(lib.status, Status::Missing, "6.6 without VIRTIO_PCI_LIB is a real gap");
    }

    #[test]
    fn failure_semantics_boot_always_component_only_when_enabled() {
        let mut config = BTreeMap::new();
        config.insert("MODULES".into(), Tristate::Y);
        let findings = evaluate(&config, (6, 6, 0), Arch::X86_64);
        let ext4 = findings.iter().find(|f| f.req.symbol == "EXT4_FS").unwrap();
        assert!(ext4.is_failure(false), "boot gaps fail regardless");
        let vfio = findings.iter().find(|f| f.req.symbol == "VFIO").unwrap();
        assert!(!vfio.is_failure(false), "component gap with the component off is informational");
        assert!(vfio.is_failure(true), "component gap fails when the component is enabled");
    }

    #[test]
    fn boot_manifest_matches_table() {
        // Every module of the frozen infra/modules-boot.conf base set must
        // have a Boot-group table entry (and vice versa) — the table is the
        // symbol-level explanation of that manifest.
        let conf = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("infra/modules-boot.conf");
        let names = super::super::modconf::parse(&conf);
        assert!(!names.is_empty(), "infra/modules-boot.conf must parse");
        for name in &names {
            let req = req_for_module(name)
                .unwrap_or_else(|| panic!("boot manifest module {name} has no kconfig table entry"));
            assert_eq!(req.group, Group::Boot, "{name} must be a Boot-group entry");
        }
        for req in requirements() {
            if req.group == Group::Boot {
                for module in req.modules {
                    assert!(
                        names.iter().any(|n| n == module),
                        "Boot-group module {module} missing from infra/modules-boot.conf"
                    );
                }
            }
        }
    }

    #[test]
    fn table_symbols_are_unique_and_wellformed() {
        let mut seen = std::collections::BTreeSet::new();
        for req in requirements() {
            assert!(
                req.symbol.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                "bad symbol {}",
                req.symbol
            );
            assert!(seen.insert(req.symbol), "duplicate symbol {}", req.symbol);
        }
    }
}
