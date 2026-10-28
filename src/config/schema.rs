//! toml schema: typed structure and parsing of `virtuoso.toml` (`Config`
//! in the parent module only holds accessors; structural truth lives here).
//! Unknown keys (`deny_unknown_fields`) and invalid types always error at
//! parse time; `StrVal` uniformly accepts string/int scalars, `Stage`
//! distinguishes the boot/runtime KO load stages.

use std::path::Path;

use anyhow::Context;

use super::ComponentPlan;

// ---------------------------------------------------------------- scalar types

/// Accepts a string or integer scalar and unifies it into a String
/// (timeout_secs = 60 and = "60" are equivalent). Other types
/// (bool/array...) error at parse time.
#[derive(Debug, Clone)]
pub(crate) struct StrVal(pub(crate) String);

impl<'de> serde::Deserialize<'de> for StrVal {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = StrVal;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("string or integer")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<StrVal, E> {
                Ok(StrVal(v.to_string()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<StrVal, E> {
                Ok(StrVal(v.to_string()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<StrVal, E> {
                Ok(StrVal(v.to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

/// KO load stage for a component: `boot` = merged into the initramfs (needed
/// before mounting root), `runtime` (default) = loaded by the rootfs init
/// after switch_root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Boot,
    Runtime,
}

impl<'de> serde::Deserialize<'de> for Stage {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match String::deserialize(d)?.as_str() {
            "boot" => Ok(Stage::Boot),
            "runtime" => Ok(Stage::Runtime),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["boot", "runtime"],
            )),
        }
    }
}

// ---------------------------------------------------------------- sections

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VirtuosoToml {
    pub(crate) kernel_path: Option<StrVal>,
    pub(crate) kernel_image: Option<StrVal>,
    pub(crate) arch: Option<StrVal>,
    pub(crate) timeout_secs: Option<StrVal>,
    pub(crate) smp: Option<StrVal>,
    pub(crate) auto_test: Option<bool>,
    pub(crate) qemu: Option<StrVal>,
    pub(crate) cmdline_extra: Option<StrVal>,
    pub(crate) qemu_opts: Option<Vec<String>>,
    pub(crate) components: Option<ComponentsSection>,
    pub(crate) busybox: Option<BusyboxSection>,
    pub(crate) tests: Option<TestsSection>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComponentsSection {
    pub(crate) tools_disk: Option<ToolsDiskComponent>,
    pub(crate) agent: Option<AgentComponent>,
    pub(crate) vfio: Option<VfioComponent>,
    pub(crate) numa: Option<NumaComponent>,
    pub(crate) pmem: Option<PmemComponent>,
    pub(crate) bpf: Option<BpfComponent>,
}

// Component common fields (enabled / require / stage) are declared explicitly
// per struct — no #[serde(flatten)], which is incompatible with
// deny_unknown_fields (unknown keys would slip through).

/// Unified view of the component common fields: enabled determination and
/// the require union look only at these three facets; each component struct
/// implements them through the `impl_component!` macro (the three fields are
/// declared identically on every component).
pub(crate) trait Component {
    fn enabled(&self) -> Option<bool>;
    fn stage(&self) -> Option<Stage>;
    fn require(&self) -> &Option<Vec<String>>;
}

macro_rules! impl_component {
    ($t:ty) => {
        impl Component for $t {
            fn enabled(&self) -> Option<bool> {
                self.enabled
            }
            fn stage(&self) -> Option<Stage> {
                self.stage
            }
            fn require(&self) -> &Option<Vec<String>> {
                &self.require
            }
        }
    };
}

/// Merge an enabled component's require into the plan (disabled components
/// are skipped entirely).
pub(crate) fn push_enabled(plan: &mut ComponentPlan, c: &impl Component) {
    if c.enabled().unwrap_or(false) {
        plan.push(c.stage(), c.require());
    }
}

/// Persistent tools disk as tools.img (attached virtio-blk → /dev/vdb
/// mounted at /tools). Section default is enabled; require defaults to empty
/// (virtio_blk/ext4 are already in the boot base set).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolsDiskComponent {
    pub(crate) enabled: Option<bool>,
    pub(crate) require: Option<Vec<String>>,
    pub(crate) stage: Option<Stage>,
}
impl_component!(ToolsDiskComponent);

/// AI probe channel (virtio-serial + the in-guest virtuoso-agent).
/// Section default is off — argv keeps the frozen baseline (invariant 3).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentComponent {
    pub(crate) enabled: Option<bool>,
    pub(crate) require: Option<Vec<String>>,
    pub(crate) stage: Option<Stage>,
}
impl_component!(AgentComponent);

/// vfio-pci passthrough: devices = a list of host device BDFs, generating
/// one `-device vfio-pci,host=<bdf>` each.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VfioComponent {
    pub(crate) enabled: Option<bool>,
    pub(crate) require: Option<Vec<String>>,
    pub(crate) stage: Option<Stage>,
    pub(crate) devices: Option<Vec<String>>,
}
impl_component!(VfioComponent);

/// Multi-node NUMA topology; section default (or enabled=false) = single node.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NumaComponent {
    pub(crate) enabled: Option<bool>,
    pub(crate) require: Option<Vec<String>>,
    pub(crate) stage: Option<Stage>,
    pub(crate) nodes: Option<StrVal>,
    pub(crate) memory_per_node: Option<StrVal>,
}
impl_component!(NumaComponent);

/// Persistent memory (QEMU nvdimm → guest /dev/pmem0); section default =
/// off (argv keeps the frozen baseline). size = backend file size (default
/// "256M"); require depends on the kernel under test's configuration
/// (declare when e.g. libnvdimm/nfit/nd_pmem are built as =m).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PmemComponent {
    pub(crate) enabled: Option<bool>,
    pub(crate) require: Option<Vec<String>>,
    pub(crate) stage: Option<Stage>,
    pub(crate) size: Option<StrVal>,
}
impl_component!(PmemComponent);

/// bpf observability component: bpftrace (the official release AppImage) is
/// provisioned on the tools disk. Section default = off; a pure build-side
/// switch — QEMU argv stays unrelated to the frozen baseline; at runtime it
/// runs directly as `/tools/bin/bpftrace` (kernel side requires BPF/BTF,
/// on by default in the openEuler defconfig).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BpfComponent {
    pub(crate) enabled: Option<bool>,
    pub(crate) require: Option<Vec<String>>,
    pub(crate) stage: Option<Stage>,
}
impl_component!(BpfComponent);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BusyboxSection {
    pub(crate) version: Option<StrVal>,
    pub(crate) release_repo: Option<StrVal>,
    pub(crate) dl_url: Option<StrVal>,
}

/// The `[tests]` section: KO dependencies of the test-case suite — the
/// natural home for declaring modules under test (built as `=m`) when
/// writing test cases for a kernel feature, without hanging them on
/// unrelated components. Entries follow the component `require` format
/// (conf lines), always at the runtime stage (test cases all run after
/// pivot); merged after the five components in the plan = same-named module
/// component entries win, this only fills the difference.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TestsSection {
    pub(crate) require: Option<Vec<String>>,
}

pub(crate) fn parse_toml(path: &Path) -> anyhow::Result<VirtuosoToml> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read {} failed", path.display()))?;
    toml::from_str(&raw).map_err(|e| {
        let mut msg = format!("{} parse failed (unknown key or invalid type): {e}", path.display());
        if raw.contains("[numa]") {
            msg.push_str("\n  The legacy [numa] section has moved to [components.numa] (enabled = true + nodes/memory_per_node)");
        }
        anyhow::anyhow!(msg)
    })
}

// ---------------------------------------------------------------- Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> anyhow::Result<VirtuosoToml> {
        toml::from_str(text).map_err(|e| anyhow::anyhow!("{e}"))
    }

    #[test]
    fn toml_globals_parse() {
        let cfg = parse(
            r#"
arch = "arm64"
timeout_secs = 60
smp = 4
auto_test = false
cmdline_extra = "nokaslr initcall_debug"
qemu_opts = ["-device vfio-pci,host=01:00.0"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.arch.as_ref().unwrap().0, "arm64");
        assert_eq!(cfg.timeout_secs.as_ref().unwrap().0, "60");
        assert_eq!(cfg.smp.as_ref().unwrap().0, "4");
        assert_eq!(cfg.auto_test, Some(false));
        assert_eq!(cfg.cmdline_extra.as_ref().unwrap().0, "nokaslr initcall_debug");
        assert_eq!(cfg.qemu_opts.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn components_parse_with_require_and_stage() {
        let cfg = parse(
            r#"
[components.tools_disk]
enabled = true

[components.agent]
enabled = true
require = ["virtio_console"]

[components.vfio]
enabled = true
devices = ["0000:01:00.0"]
require = ["vfio", "vfio_pci"]
stage = "boot"

[components.numa]
enabled = true
nodes = 2
memory_per_node = "1G"
"#,
        )
        .unwrap();
        let comps = cfg.components.as_ref().unwrap();
        assert_eq!(comps.tools_disk.as_ref().unwrap().enabled, Some(true));
        let agent = comps.agent.as_ref().unwrap();
        assert_eq!(agent.require.clone().unwrap(), vec!["virtio_console"]);
        let vfio = comps.vfio.as_ref().unwrap();
        assert_eq!(vfio.devices.clone().unwrap(), vec!["0000:01:00.0"]);
        assert_eq!(vfio.stage, Some(Stage::Boot));
        let numa = comps.numa.as_ref().unwrap();
        assert_eq!(numa.nodes.as_ref().unwrap().0, "2");
    }

    #[test]
    fn component_plan_unions_enabled_requires_in_schema_order() {
        // Order frozen: tools_disk → agent → vfio → numa → pmem; dedup keeps
        // the first; a disabled component's require is not merged;
        // stage=boot partitions into boot_extra.
        let cfg = parse(
            r#"
[components.tools_disk]
enabled = true
require = ["virtio_blk"]

[components.agent]
enabled = true
require = ["virtio_console"]

[components.vfio]
enabled = false
require = ["virtio_blk"]
stage = "boot"

[components.numa]
enabled = true
require = ["crc64"]
stage = "boot"

[components.pmem]
enabled = true
require = ["nd_pmem"]
"#,
        )
        .unwrap();
        let comps = cfg.components.as_ref().unwrap();
        let mut plan = ComponentPlan::default();
        // Reusing Config::component_plan's assembly logic directly is too
        // heavy (it needs a full Config); mirror its call order to verify
        // push semantics.
        let td = comps.tools_disk.as_ref().unwrap();
        let ag = comps.agent.as_ref().unwrap();
        let nm = comps.numa.as_ref().unwrap();
        let pm = comps.pmem.as_ref().unwrap();
        plan.push(td.stage, &td.require);
        plan.push(ag.stage, &ag.require);
        // vfio disabled — require not merged
        plan.push(nm.stage, &nm.require);
        plan.push(pm.stage, &pm.require);
        assert_eq!(plan.runtime, ["virtio_blk", "virtio_console", "nd_pmem"]);
        assert_eq!(plan.boot_extra, ["crc64"]);
        assert_eq!(
            plan.all().cloned().collect::<Vec<_>>(),
            ["crc64", "virtio_blk", "virtio_console", "nd_pmem"]
        );
    }

    #[test]
    fn push_enabled_skips_disabled_components() {
        // the Component facet (impl_component!) drives the enabled gate
        let cfg = parse("[components.bpf]\nenabled = true\nrequire = [\"bpf\"]\n").unwrap();
        let bpf = cfg.components.as_ref().unwrap().bpf.as_ref().unwrap();
        let mut plan = ComponentPlan::default();
        push_enabled(&mut plan, bpf);
        assert_eq!(plan.all().cloned().collect::<Vec<_>>(), ["bpf"]);

        let cfg = parse("[components.bpf]\nenabled = false\nrequire = [\"bpf\"]\n").unwrap();
        let bpf = cfg.components.as_ref().unwrap().bpf.as_ref().unwrap();
        let mut plan = ComponentPlan::default();
        push_enabled(&mut plan, bpf);
        assert_eq!(plan.all().count(), 0);
    }

    #[test]
    fn pmem_parses_with_size_and_defaults_disabled() {
        // Section default = off: the size accessor returns None
        let cfg = parse("[components.pmem]\n").unwrap();
        let comps = cfg.components.as_ref().unwrap();
        assert!(comps.pmem.as_ref().unwrap().enabled.is_none());

        let cfg = parse(
            r#"
[components.pmem]
enabled = true
size = "512M"
require = ["libnvdimm", "nfit", "nd_pmem"]
"#,
        )
        .unwrap();
        let comps = cfg.components.as_ref().unwrap();
        let pm = comps.pmem.as_ref().unwrap();
        assert_eq!(pm.size.as_ref().unwrap().0, "512M");
        assert_eq!(
            pm.require.clone().unwrap(),
            vec!["libnvdimm", "nfit", "nd_pmem"]
        );
    }

    #[test]
    fn pmem_unknown_key_rejected() {
        assert!(parse("[components.pmem]\nenabled = true\nsizes = \"1G\"\n").is_err());
    }

    #[test]
    fn bpf_component_defaults_off_and_parses_enabled() {
        // Section default = no section; empty section/missing enabled = off
        let cfg = parse("[components.bpf]\n").unwrap();
        let bpf = cfg.components.as_ref().unwrap().bpf.as_ref().unwrap();
        assert!(bpf.enabled.is_none());
        let mut plan = ComponentPlan::default();
        push_enabled(&mut plan, bpf);
        assert_eq!(plan.all().count(), 0);

        let cfg = parse(
            r#"
[components.bpf]
enabled = true
require = ["bpf"]
"#,
        )
        .unwrap();
        let bpf = cfg.components.as_ref().unwrap().bpf.as_ref().unwrap();
        assert_eq!(bpf.enabled, Some(true));
        let mut plan = ComponentPlan::default();
        push_enabled(&mut plan, bpf);
        assert_eq!(plan.all().cloned().collect::<Vec<_>>(), ["bpf"]);
    }

    #[test]
    fn bpf_unknown_key_rejected() {
        assert!(parse("[components.bpf]\nenabled = true\nversion = \"0.27.0\"\n").is_err());
    }

    #[test]
    fn tests_section_parses_and_rejects_unknown_keys() {
        let cfg = parse("[tests]\nrequire = [\"overlay\", \"kvm\"]\n").unwrap();
        assert_eq!(
            cfg.tests.as_ref().unwrap().require.clone().unwrap(),
            vec!["overlay", "kvm"]
        );
        // The test-case suite has no on/off semantics (auto_test is a global
        // key): enabled must error
        assert!(parse("[tests]\nenabled = true\n").is_err());
        assert!(parse("[tests]\nstage = \"boot\"\n").is_err());
    }

    #[test]
    fn unknown_key_is_rejected_at_parse_time() {
        assert!(
            parse("arch = \"arm64\"\nshmp = 4\n").is_err(),
            "misspelled keys must error at parse time"
        );
    }

    #[test]
    fn unknown_component_key_is_rejected() {
        assert!(parse("[components.agent]\nenableds = true\n").is_err());
    }

    #[test]
    fn wrong_type_is_rejected_at_parse_time() {
        assert!(parse("qemu_opts = [1, 2]").is_err());
        assert!(parse("arch = true").is_err());
        assert!(parse("[components.agent]\nstage = \"middle\"\n").is_err());
    }
}
