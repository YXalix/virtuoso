//! Typed configuration layer. `virtuoso.toml` is the single configuration
//! surface — structured global keys plus components (each `[components.*]`
//! section is a VM component whose `require` declares the kernel modules it
//! depends on; the builder generates the module manifest from the union of
//! enabled components, and the `[tests]` section collects module dependencies
//! of the test-case suite). Invalid configuration always errors at parse time
//! (unknown keys, invalid types).
//!
//! Scalar key priority: **process environment variables > virtuoso.toml** —
//! an env var overrides the same-named toml field (CI/CLI temporary overrides
//! never touch the file); when neither is set, built-in defaults apply.
//!
//! Sections: scalar resolution chain and Config → global scalar accessors →
//! component accessors → BusyBox provisioning → ComponentPlan (require
//! union) → toml schema (structural truth, the sole line of defense rejecting
//! unknown keys/invalid types at parse time).

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::Arch;

use crate::builder::busybox;

// ---------------------------------------------------------------- Scalar resolution chain and Config

/// Project root discovery: walk up from the current directory looking for a
/// directory containing `infra/init` (the PID 1 script).
pub fn find_project_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("infra/init").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Scalar resolution chain: process env var > virtuoso.toml field (env
/// overrides toml for same-named keys; an empty string counts as unset and
/// resolution falls through). Temporary overrides without file edits go
/// through env; persistent configuration is written to toml.
fn scalar(toml_val: Option<&StrVal>, env_key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_key) {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    if let Some(v) = toml_val {
        if !v.0.trim().is_empty() {
            return Some(v.0.clone());
        }
    }
    None
}

/// Env bool parsing (lenient semantics: "1"/"true"/"yes"/"on" is true, everything else false).
fn env_bool(key: &str) -> Option<bool> {
    std::env::var(key).ok().map(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub struct Config {
    pub project_root: PathBuf,
    /// In-VM source assets (init, modules-boot.conf, testcases, tools) — git tracked.
    pub infra_dir: PathBuf,
    /// Workspace target/ (run scratch: agent shell socket etc.).
    pub target_dir: PathBuf,
    /// Build artifacts (initrd.img / rootfs.img / tools.img) — data plane, git ignored.
    pub artifacts_dir: PathBuf,
    /// Build staging and cache (busybox supply chain, initramfs/rootfs
    /// assembly dirs) — git ignored.
    pub build_dir: PathBuf,
    /// Path to virtuoso.toml (Some only when it exists).
    pub toml_path: Option<PathBuf>,
    /// Typed parse result of virtuoso.toml (Some only when the file exists;
    /// unknown keys/invalid types already error at parse time).
    pub toml: Option<VirtuosoToml>,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let project_root = find_project_root().ok_or_else(|| {
            anyhow::anyhow!(
                "virtuoso project root not found (needs infra/init); run inside a virtuoso checkout"
            )
        })?;
        let infra_dir = project_root.join("infra");
        let target_dir = project_root.join("target");
        let toml_path = project_root.join("virtuoso.toml");
        let toml = if toml_path.is_file() {
            Some(parse_toml(&toml_path)?)
        } else {
            None
        };
        Ok(Self {
            artifacts_dir: target_dir.join("artifacts"),
            build_dir: target_dir.join("build"),
            project_root,
            infra_dir,
            target_dir,
            toml_path: toml_path.is_file().then_some(toml_path),
            toml,
        })
    }

    /// Path to virtuoso.toml (Some only when it exists).
    pub fn toml_path(&self) -> Option<&Path> {
        self.toml_path.as_deref()
    }

    /// Read a toml global scalar field (None when the field is absent or the
    /// toml file does not exist).
    fn tv(&self, f: impl FnOnce(&VirtuosoToml) -> &Option<StrVal>) -> Option<&StrVal> {
        self.toml.as_ref().and_then(|t| f(t).as_ref())
    }

    /// Component section access (None when toml is absent or the components
    /// section is missing).
    fn comp<'a, T>(
        &'a self,
        f: impl FnOnce(&'a ComponentsSection) -> Option<&'a T>,
    ) -> Option<&'a T> {
        f(self.toml.as_ref()?.components.as_ref()?)
    }
}

// ---------------------------------------------------------------- Global scalar accessors

/// Where the kernel source tree resolved from (honest diagnostics + the
/// explicitness check for skill install).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelPathSource {
    /// Process env KERNEL_PATH (CI / temporary overrides; always highest — frozen invariant 5)
    Env,
    /// Docker-mode active volume (KERNEL_VOLUME env > current state file → volume host view)
    CurrentVolume(String),
    /// virtuoso.toml kernel_path (explicit in raw mode)
    Config,
    /// Default auto-detection = parent of the project root
    AutoDetect,
}

impl std::fmt::Display for KernelPathSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KernelPathSource::Env => write!(f, "env KERNEL_PATH"),
            KernelPathSource::CurrentVolume(v) => write!(f, "current volume {v}"),
            KernelPathSource::Config => write!(f, "config"),
            KernelPathSource::AutoDetect => write!(f, "auto-detected"),
        }
    }
}

/// Kernel source tree resolution (the single entry point; doctor/builder/
/// skill/launcher all go through here): process env KERNEL_PATH > docker
/// active volume > toml kernel_path > default parent of the project root.
/// The active volume overrides toml — `kernel use/clone` is the user's most
/// recent explicit statement of "which tree I am working on"; test still
/// consuming the old tree after a volume switch is exactly the root of
/// dual-source confusion. env is always highest (env vars win for same-named
/// scalar keys, frozen invariant 5). View resolution is injected via view_of
/// (production = forge::host_view, tests = a fake).
fn resolve_kernel_path(
    project_root: &Path,
    toml_val: Option<&str>,
    view_of: &dyn Fn(&str) -> anyhow::Result<PathBuf>,
) -> anyhow::Result<(PathBuf, KernelPathSource)> {
    if let Some(p) = std::env::var("KERNEL_PATH").ok().filter(|v| !v.trim().is_empty()) {
        return Ok((PathBuf::from(p), KernelPathSource::Env));
    }
    let volume = match std::env::var("KERNEL_VOLUME") {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => crate::forge::read_current(project_root)?.map(|c| c.volume),
    };
    if let Some(v) = volume {
        let view =
            view_of(&v).with_context(|| format!("active kernel volume {v} is unreachable"))?;
        anyhow::ensure!(
            view.is_dir(),
            "active kernel volume {v} host view {} is unreachable — start OrbStack (macOS) and retry",
            view.display()
        );
        return Ok((view, KernelPathSource::CurrentVolume(v)));
    }
    if let Some(p) = toml_val.map(str::trim).filter(|v| !v.is_empty()) {
        return Ok((PathBuf::from(p), KernelPathSource::Config));
    }
    let parent = project_root
        .parent()
        .context("project root has no parent, cannot auto-detect KERNEL_PATH")?;
    Ok((parent.to_path_buf(), KernelPathSource::AutoDetect))
}

impl Config {
    /// Resolve the target arch; returns None when unresolvable (the
    /// diagnostics layer warns).
    pub fn arch(&self) -> Option<Arch> {
        self.arch_str()
            .and_then(|a| Arch::parse(&a))
            .or_else(Arch::host_default)
    }

    /// Raw arch string (for the diagnostics layer to show the source).
    pub fn arch_str(&self) -> Option<String> {
        scalar(self.tv(|t| &t.arch), "ARCH")
    }

    /// Kernel source tree resolution: process env KERNEL_PATH > docker
    /// active volume > toml kernel_path > default parent of the project
    /// root. The source is returned alongside the path (diagnostics display
    /// + explicitness check); see [`resolve_kernel_path`] for the rules.
    pub fn kernel_path(&self) -> anyhow::Result<(PathBuf, KernelPathSource)> {
        resolve_kernel_path(
            &self.project_root,
            self.tv(|t| &t.kernel_path).map(|v| v.0.as_str()),
            &crate::forge::host_view,
        )
    }

    /// kernel_image override (default = the arch's image inside the kernel tree).
    pub fn kernel_image(&self) -> Option<String> {
        scalar(self.tv(|t| &t.kernel_image), "KERNEL_IMAGE")
    }

    /// Raw QEMU_TIMEOUT string ("0" is rejected by the test command).
    pub fn timeout_raw(&self) -> String {
        scalar(self.tv(|t| &t.timeout_secs), "QEMU_TIMEOUT").unwrap_or_else(|| "0".into())
    }

    /// AUTO_TEST switch (default true — a normal boot runs the tests; env
    /// overrides toml, env_bool lenient semantics: 1/true/yes/on is true).
    pub fn auto_test(&self) -> bool {
        env_bool("AUTO_TEST")
            .unwrap_or_else(|| self.toml.as_ref().and_then(|t| t.auto_test).unwrap_or(true))
    }

    /// QEMU binary override.
    pub fn qemu_override(&self) -> Option<String> {
        scalar(self.tv(|t| &t.qemu), "QEMU")
    }

    /// Kernel cmdline extra: `CMDLINE_EXTRA` env var first, else toml
    /// `cmdline_extra`. Appended verbatim as one string after the
    /// machine-assembled baseline (empty default = argv identical word for
    /// word to the frozen baseline); kernel params are last-wins, so
    /// appending overrides (the `console=` exception: registered as a second
    /// console, the later one becomes preferred).
    pub fn cmdline_extra(&self) -> Option<String> {
        scalar(self.tv(|t| &t.cmdline_extra), "CMDLINE_EXTRA")
    }

    /// QEMU passthrough args (global fallback): `QEMU_OPTS` env var (split
    /// on whitespace) first, else the toml `qemu_opts` array. Component
    /// increments (vfio devices etc.) are not mixed in here — they are
    /// appended explicitly at the assembly point (cli::build_invocation),
    /// keeping the global fallback and component increments separate.
    pub fn qemu_extra(&self) -> Vec<String> {
        match std::env::var("QEMU_OPTS") {
            Ok(s) if !s.trim().is_empty() => s.split_whitespace().map(str::to_string).collect(),
            _ => self
                .toml
                .as_ref()
                .and_then(|t| t.qemu_opts.as_ref())
                .map(|list| list.to_vec())
                .unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------- Component accessors

impl Config {
    /// vfio passthrough device BDF list (empty when the component is disabled).
    pub fn vfio(&self) -> Option<&[String]> {
        let v = self.comp(|c| c.vfio.as_ref())?;
        v.enabled
            .unwrap_or(false)
            .then(|| v.devices.as_deref().unwrap_or(&[]))
    }

    /// vfio component: one `-device vfio-pci,host=<bdf>` per device.
    /// Component increments are appended explicitly at the assembly point
    /// (cli::build_invocation) and bypass the qemu_opts env/toml priority chain.
    pub fn vfio_opts(&self) -> Vec<String> {
        match self.vfio() {
            Some(devices) if !devices.is_empty() => devices
                .iter()
                .map(|d| format!("-device vfio-pci,host={d}"))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// NUMA topology params (smp, nodes, memory_per_node) for
    /// NumaTopology::parse. Componentized resolution: smp is global;
    /// nodes/memory come from the enabled [components.numa], falling back to
    /// the legacy vars when disabled (default single node), then to 1 / "1G".
    pub fn topo_params(&self) -> (String, String, String) {
        let smp = scalar(self.tv(|t| &t.smp), "SMP").unwrap_or_else(|| "8".into());
        let numa = self
            .comp(|c| c.numa.as_ref())
            .filter(|n| n.enabled.unwrap_or(false));
        // Priority frozen: NUMA_NODES / NUMA_MEMORY env override toml (whether or not the component is enabled)
        let nodes = match &numa {
            Some(n) => scalar(n.nodes.as_ref(), "NUMA_NODES").unwrap_or_else(|| "2".into()),
            None => std::env::var("NUMA_NODES")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "1".into()),
        };
        let mem = match &numa {
            Some(n) => {
                scalar(n.memory_per_node.as_ref(), "NUMA_MEMORY").unwrap_or_else(|| "1G".into())
            }
            None => std::env::var("NUMA_MEMORY")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "1G".into()),
        };
        (smp, nodes, mem)
    }

    /// Component plan: the require union of enabled components (schema-fixed
    /// order tools_disk → agent → vfio → numa → pmem → bpf, dedup keeping
    /// the first), partitioned by stage. The `[tests]` section's require is
    /// merged last (always runtime; same-named module component entries win,
    /// test cases only fill the difference).
    pub fn component_plan(&self) -> ComponentPlan {
        let mut plan = ComponentPlan::default();
        if let Some(c) = self.comp(|c| c.tools_disk.as_ref()) {
            push_enabled(&mut plan, c);
        }
        if let Some(c) = self.comp(|c| c.agent.as_ref()) {
            push_enabled(&mut plan, c);
        }
        if let Some(c) = self.comp(|c| c.vfio.as_ref()) {
            push_enabled(&mut plan, c);
        }
        if let Some(c) = self.comp(|c| c.numa.as_ref()) {
            push_enabled(&mut plan, c);
        }
        if let Some(c) = self.comp(|c| c.pmem.as_ref()) {
            push_enabled(&mut plan, c);
        }
        if let Some(c) = self.comp(|c| c.bpf.as_ref()) {
            push_enabled(&mut plan, c);
        }
        if let Some(t) = self.toml.as_ref().and_then(|t| t.tests.as_ref()) {
            plan.push(None, &t.require);
        }
        plan
    }

    /// Whether the tools_disk component is enabled (section default = true:
    /// attach tools.img whenever it exists, matching the "caller increments"
    /// semantics of frozen invariant 3).
    pub fn tools_disk_enabled(&self) -> bool {
        self.comp(|c| c.tools_disk.as_ref())
            .and_then(|c| c.enabled)
            .unwrap_or(true)
    }

    /// Whether the agent component is enabled (section default = false: argv keeps the frozen baseline).
    pub fn agent_enabled(&self) -> bool {
        self.comp(|c| c.agent.as_ref())
            .and_then(|c| c.enabled)
            .unwrap_or(false)
    }

    /// Whether the bpf observability component is enabled (bpftrace is
    /// provisioned on the tools disk). Section default = false: a pure
    /// build-side switch, QEMU argv stays unrelated to the frozen baseline.
    pub fn bpf_enabled(&self) -> bool {
        self.comp(|c| c.bpf.as_ref())
            .and_then(|c| c.enabled)
            .unwrap_or(false)
    }

    /// pmem component size (Some only when enabled; missing size uses the
    /// default "256M"). Section default (or enabled=false) = off: argv keeps
    /// the frozen baseline.
    pub fn pmem_size(&self) -> Option<String> {
        self.comp(|c| c.pmem.as_ref())
            .filter(|p| p.enabled.unwrap_or(false))
            .map(|p| {
                p.size
                    .as_ref()
                    .map(|s| s.0.clone())
                    .unwrap_or_else(|| "256M".into())
            })
    }
}

// ---------------------------------------------------------------- BusyBox provisioning config

impl Config {
    /// BusyBox provisioning config (BUSYBOX_* vars first, falling back to
    /// the toml [busybox] section).
    pub fn busybox_supply(&self) -> busybox::Supply {
        let toml = self.toml.as_ref().and_then(|t| t.busybox.as_ref());
        busybox::Supply {
            version: scalar(toml.and_then(|b| b.version.as_ref()), "BUSYBOX_VERSION"),
            release_repo: scalar(
                toml.and_then(|b| b.release_repo.as_ref()),
                "BUSYBOX_RELEASE_REPO",
            ),
            dl_url: scalar(toml.and_then(|b| b.dl_url.as_ref()), "BUSYBOX_DL_URL"),
        }
    }
}

// ---------------------------------------------------------------- ComponentPlan

/// The KO dependency union of enabled components: `boot_extra` is appended
/// after the modules-boot.conf frozen base set (loaded at the initramfs
/// stage), `runtime` generates the rootfs /lib/modules/modules.conf (loaded
/// after switch_root). An entry = a conf line `"<module> [key=val ...]"`;
/// the first token is the module name (the dedup key). The builder consumes
/// this plan to produce the initramfs base-set additions and the rootfs
/// modules.conf.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ComponentPlan {
    pub boot_extra: Vec<String>,
    pub runtime: Vec<String>,
}

impl ComponentPlan {
    pub(crate) fn push(&mut self, stage: Option<Stage>, require: &Option<Vec<String>>) {
        let Some(lines) = require else { return };
        let boot = stage == Some(Stage::Boot);
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let name = line.split_whitespace().next().unwrap_or_default();
            let slot = if boot {
                &mut self.boot_extra
            } else {
                &mut self.runtime
            };
            if slot
                .iter()
                .any(|e| e.split_whitespace().next().unwrap_or_default() == name)
            {
                continue;
            }
            slot.push(line.to_string());
        }
    }

    /// All entries (boot_extra first) — input for verify's module existence check.
    pub fn all(&self) -> impl Iterator<Item = &String> {
        self.boot_extra.iter().chain(self.runtime.iter())
    }
}

// ---------------------------------------------------------------- toml schema

// toml schema: typed structure and parsing of `virtuoso.toml` (`Config`
// only holds accessors; structural truth lives here). Unknown keys
// (`deny_unknown_fields`) and invalid types always error at parse time;
// `StrVal` uniformly accepts string/int scalars, `Stage` distinguishes the
// boot/runtime KO load stages.

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
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
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
/// implements them item by item (five lines of boilerplate replace the
/// caller's five-line copy blocks).
pub(crate) trait Component {
    fn enabled(&self) -> Option<bool>;
    fn stage(&self) -> Option<Stage>;
    fn require(&self) -> &Option<Vec<String>>;
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

impl Component for ToolsDiskComponent {
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

/// AI probe channel (virtio-serial + the in-guest virtuoso-agent).
/// Section default is off — argv keeps the frozen baseline (invariant 3).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentComponent {
    pub(crate) enabled: Option<bool>,
    pub(crate) require: Option<Vec<String>>,
    pub(crate) stage: Option<Stage>,
}

impl Component for AgentComponent {
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

impl Component for VfioComponent {
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

impl Component for NumaComponent {
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

impl Component for PmemComponent {
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

impl Component for BpfComponent {
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
    use crate::util::TempRoot;

    fn parse(text: &str) -> anyhow::Result<VirtuosoToml> {
        toml::from_str(text).map_err(|e| anyhow::anyhow!("{e}"))
    }

    #[test]
    fn scalar_env_overrides_toml() {
        // Priority frozen: process env var > virtuoso.toml. env changes are
        // process-global; serialize with a mutex and use a dedicated key
        // prefix to avoid polluting real config keys.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap();
        let toml = StrVal("arm64".into());
        std::env::set_var("VIRTUOSO_TEST_SCALAR", "riscv64");
        assert_eq!(
            scalar(Some(&toml), "VIRTUOSO_TEST_SCALAR").as_deref(),
            Some("riscv64"),
            "same-named process env var must override toml"
        );
        std::env::remove_var("VIRTUOSO_TEST_SCALAR");
        assert_eq!(
            scalar(Some(&toml), "VIRTUOSO_TEST_SCALAR").as_deref(),
            Some("arm64"),
            "falls back to toml when env is unset"
        );
        // Empty string counts as unset: both env and toml empty values fall through
        std::env::set_var("VIRTUOSO_TEST_SCALAR", "");
        assert_eq!(
            scalar(Some(&toml), "VIRTUOSO_TEST_SCALAR").as_deref(),
            Some("arm64")
        );
        std::env::remove_var("VIRTUOSO_TEST_SCALAR");
        assert_eq!(
            scalar(Some(&StrVal("  ".into())), "VIRTUOSO_TEST_SCALAR"),
            None
        );
        assert_eq!(scalar(None, "VIRTUOSO_TEST_SCALAR"), None);
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

    #[test]
    fn component_plan_dedups_by_module_name_keeping_first_args() {
        let mut plan = ComponentPlan::default();
        plan.push(
            None,
            &Some(vec!["nvme-core poll_queues=2".into(), "nvme-core".into()]),
        );
        assert_eq!(plan.runtime, ["nvme-core poll_queues=2".to_string()]);
        plan.push(Some(Stage::Boot), &Some(vec!["nvme-core".into()]));
        assert_eq!(plan.boot_extra, ["nvme-core"], "dedup only within the same stage partition");
    }

    // ---- Kernel source tree resolution: env > current active volume > toml > default ----

    /// Mutex shared by the resolution-order tests (env changes are process-global).
    static KSRC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Scratch project_root + optional current state file (volume, arch).
    fn ksrc_scratch(name: &str, current: Option<(&str, &str)>) -> TempRoot {
        let dir = TempRoot::new(&format!("cfg-ksrc-{name}"));
        if let Some((vol, arch)) = current {
            let p = dir.join(".virtuoso").join("kernel-current.json");
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!(r#"{{ "volume": "{vol}", "arch": "{arch}" }}"#)).unwrap();
        }
        dir
    }

    /// Fake view resolution: volume → view_root/<volume>, the directory
    /// really exists (passes the is_dir guard).
    fn fake_view_factory() -> (TempRoot, impl Fn(&str) -> anyhow::Result<PathBuf>) {
        let root = TempRoot::new("cfg-ksrc-view");
        let base = root.to_path_buf();
        let view_of = move |v: &str| -> anyhow::Result<PathBuf> {
            let p = base.join(v);
            std::fs::create_dir_all(&p)?;
            Ok(p)
        };
        (root, view_of)
    }

    fn clear_kernel_env() {
        std::env::remove_var("KERNEL_PATH");
        std::env::remove_var("KERNEL_VOLUME");
    }

    #[test]
    fn current_volume_overrides_toml_kernel_path() {
        let _g = KSRC_LOCK.lock().unwrap();
        clear_kernel_env();
        let dir = ksrc_scratch("cur-toml", Some(("ksrc-mainline", "arm64")));
        let (view_root, view_of) = fake_view_factory();
        let (p, src) = resolve_kernel_path(&dir, Some("/old/tree"), &view_of).unwrap();
        assert_eq!(p, view_root.join("ksrc-mainline"));
        assert_eq!(
            src,
            KernelPathSource::CurrentVolume("ksrc-mainline".into()),
            "after a volume switch the kernel tree must follow the active volume, not the stale toml value"
        );
    }

    #[test]
    fn env_kernel_path_beats_everything() {
        let _g = KSRC_LOCK.lock().unwrap();
        std::env::set_var("KERNEL_PATH", "/env/tree");
        let dir = ksrc_scratch("env-first", Some(("ksrc-mainline", "arm64")));
        let (_, view_of) = fake_view_factory();
        let (p, src) = resolve_kernel_path(&dir, Some("/toml/tree"), &view_of).unwrap();
        assert_eq!(p, PathBuf::from("/env/tree"));
        assert_eq!(src, KernelPathSource::Env);
        clear_kernel_env();
    }

    #[test]
    fn toml_kernel_path_beats_autodetect() {
        let _g = KSRC_LOCK.lock().unwrap();
        clear_kernel_env();
        let dir = ksrc_scratch("toml-only", None);
        let (_, view_of) = fake_view_factory();
        let (p, src) = resolve_kernel_path(&dir, Some("/toml/tree"), &view_of).unwrap();
        assert_eq!(p, PathBuf::from("/toml/tree"));
        assert_eq!(src, KernelPathSource::Config);
        let (p, src) = resolve_kernel_path(&dir, None, &view_of).unwrap();
        assert_eq!(p, dir.parent().unwrap());
        assert_eq!(src, KernelPathSource::AutoDetect);
    }

    #[test]
    fn unreachable_current_volume_is_an_error_not_silent_fallback() {
        let _g = KSRC_LOCK.lock().unwrap();
        clear_kernel_env();
        let dir = ksrc_scratch("gone", Some(("vanished-vol", "arm64")));
        // View resolution succeeds but the directory is absent (OrbStack not
        // running) → error rather than silently consuming the stale toml tree
        let absent_root = TempRoot::absent("cfg-ksrc-absent");
        let absent = move |v: &str| -> anyhow::Result<PathBuf> {
            Ok(absent_root.join(v)) // directory not created
        };
        let err = format!(
            "{}",
            resolve_kernel_path(&dir, Some("/old/tree"), &absent).unwrap_err()
        );
        assert!(
            err.contains("vanished-vol") && err.contains("unreachable"),
            "{err}"
        );
        // View resolution itself fails (docker missing on Linux) → also an error
        let boom = |v: &str| -> anyhow::Result<PathBuf> {
            anyhow::bail!("docker volume inspect {v} failed")
        };
        assert!(resolve_kernel_path(&dir, Some("/old/tree"), &boom).is_err());
        clear_kernel_env();
    }
}
