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
//! union). The toml schema (structural truth, the sole line of defense
//! rejecting unknown keys/invalid types at parse time) lives in the `schema`
//! submodule.

mod schema;

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::Arch;

use crate::builder::busybox;

use schema::{parse_toml, push_enabled, ComponentsSection, Stage, StrVal, VirtuosoToml};

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
    if let Some(p) = crate::util::env_nonempty("KERNEL_PATH") {
        return Ok((PathBuf::from(p), KernelPathSource::Env));
    }
    if let Some(v) = crate::forge::active_volume(project_root)? {
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
            None => crate::util::env_nonempty("NUMA_NODES").unwrap_or_else(|| "1".into()),
        };
        let mem = match &numa {
            Some(n) => {
                scalar(n.memory_per_node.as_ref(), "NUMA_MEMORY").unwrap_or_else(|| "1G".into())
            }
            None => crate::util::env_nonempty("NUMA_MEMORY").unwrap_or_else(|| "1G".into()),
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

// ---------------------------------------------------------------- Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempRoot;

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
