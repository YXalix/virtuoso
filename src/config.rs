//! 类型化配置层。`virtuoso.toml` 是唯一配置面 —— 结构化全局键 +
//! 组件化（每个 `[components.*]` 段是一个 VM 组件，`require` 声明其依赖的
//! 内核模块，builder 按启用组件的并集生成模块清单；`[tests]` 段收测例
//! 套件的模块依赖）。非法配置一律解析期报错（未知键、非法类型）。
//!
//! 标量键取值优先级：**进程环境变量 > virtuoso.toml** —— 同名键环境变量
//! 覆盖 toml 字段（CI/命令行临时改参不动文件），都未设置时用内置缺省。
//!
//! 分节：标量取值链与 Config → 全局标量访问器 → 组件访问器 → BusyBox
//! 供给 → ComponentPlan（require 并集）→ toml schema（结构真相，未知键/
//! 非法类型解析期报错的唯一防线）。

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::Arch;

use crate::builder::busybox;

// ---------------------------------------------------------------- 标量取值链与 Config

/// 项目根定位：从当前目录逐级向上寻找含 `infra/init`（PID 1 脚本）的目录。
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

/// 标量取值链：进程环境变量 > virtuoso.toml 字段（同名键 env 覆盖 toml；
/// 空串视为未设置，继续回落）。无文件编辑的临时改参走 env，持久配置写 toml。
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

/// env 布尔解析（宽松语义："1"/"true"/"yes"/"on" 为真，其余为假）。
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
    /// VM 内源资产（init、modules-boot.conf、testcases、tools）——git 跟踪。
    pub infra_dir: PathBuf,
    /// 工作区 target/（运行 scratch：agent shell socket 等）。
    pub target_dir: PathBuf,
    /// 构建产物（initrd.img / rootfs.img / tools.img）——数据面，git 忽略。
    pub artifacts_dir: PathBuf,
    /// 构建暂存与缓存（busybox 供给链、initramfs/rootfs 组装目录）——git 忽略。
    pub build_dir: PathBuf,
    /// virtuoso.toml 路径（存在才 Some）。
    pub toml_path: Option<PathBuf>,
    /// virtuoso.toml 类型化解析结果（文件存在才 Some；未知键/非法类型已在
    /// 解析期报错）。
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

    /// virtuoso.toml 路径（存在才 Some）。
    pub fn toml_path(&self) -> Option<&Path> {
        self.toml_path.as_deref()
    }

    /// 取 toml 全局标量字段（None 当字段未写或 toml 文件不存在）。
    fn tv(&self, f: impl FnOnce(&VirtuosoToml) -> &Option<StrVal>) -> Option<&StrVal> {
        self.toml.as_ref().and_then(|t| f(t).as_ref())
    }

    /// 组件段访问入口（toml 未配置 / components 段缺失 = None）。
    fn comp<'a, T>(
        &'a self,
        f: impl FnOnce(&'a ComponentsSection) -> Option<&'a T>,
    ) -> Option<&'a T> {
        f(self.toml.as_ref()?.components.as_ref()?)
    }
}

// ---------------------------------------------------------------- 全局标量访问器

impl Config {
    /// 解析目标架构；无法解析时返回 None（诊断层告警）。
    pub fn arch(&self) -> Option<Arch> {
        self.arch_str()
            .and_then(|a| Arch::parse(&a))
            .or_else(Arch::host_default)
    }

    /// arch 原始字符串（诊断层呈现来源用）。
    pub fn arch_str(&self) -> Option<String> {
        scalar(self.tv(|t| &t.arch), "ARCH")
    }

    /// (KERNEL_PATH, 是否显式指定)。未指定时 = 项目根上一级（qemu-e2e 自动探测语义）。
    pub fn kernel_path(&self) -> anyhow::Result<(PathBuf, bool)> {
        match scalar(self.tv(|t| &t.kernel_path), "KERNEL_PATH") {
            Some(p) => Ok((PathBuf::from(p), true)),
            None => {
                let parent = self
                    .project_root
                    .parent()
                    .context("project root has no parent, cannot auto-detect KERNEL_PATH")?;
                Ok((parent.to_path_buf(), false))
            }
        }
    }

    /// kernel_image 覆盖（缺省 = 内核树内 arch 对应镜像）。
    pub fn kernel_image(&self) -> Option<String> {
        scalar(self.tv(|t| &t.kernel_image), "KERNEL_IMAGE")
    }

    /// QEMU_TIMEOUT 原始字符串（"0" 由 test 命令拒绝）。
    pub fn timeout_raw(&self) -> String {
        scalar(self.tv(|t| &t.timeout_secs), "QEMU_TIMEOUT").unwrap_or_else(|| "0".into())
    }

    /// AUTO_TEST 开关（缺省 true —— 常规启动即跑测试；env 覆盖 toml，
    /// env_bool 宽松语义：1/true/yes/on 为真）。
    pub fn auto_test(&self) -> bool {
        env_bool("AUTO_TEST")
            .unwrap_or_else(|| self.toml.as_ref().and_then(|t| t.auto_test).unwrap_or(true))
    }

    /// QEMU 二进制覆盖。
    pub fn qemu_override(&self) -> Option<String> {
        scalar(self.tv(|t| &t.qemu), "QEMU")
    }

    /// QEMU 透传参数（全局兜底）：`QEMU_OPTS` 环境变量（空白切分）优先，
    /// 否则 toml `qemu_opts` 数组。组件增量（vfio 设备等）不在此混入——
    /// 由装配点（cli::build_invocation）显式追加，全局兜底与组件增量分清。
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

// ---------------------------------------------------------------- 组件访问器

impl Config {
    /// vfio 直通设备 BDF 列表（组件未启用 = 空）。
    pub fn vfio(&self) -> Option<&[String]> {
        let v = self.comp(|c| c.vfio.as_ref())?;
        v.enabled
            .unwrap_or(false)
            .then(|| v.devices.as_deref().unwrap_or(&[]))
    }

    /// vfio 组件：每设备一条 `-device vfio-pci,host=<bdf>`。
    /// 组件增量由装配点（cli::build_invocation）显式追加，不参与
    /// qemu_opts 的 env/toml 优先级链。
    pub fn vfio_opts(&self) -> Vec<String> {
        match self.vfio() {
            Some(devices) if !devices.is_empty() => devices
                .iter()
                .map(|d| format!("-device vfio-pci,host={d}"))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// NUMA 拓扑参数 (smp, nodes, memory_per_node)，供 NumaTopology::parse。
    /// 组件化取值：smp 全局；nodes/memory 来自启用的 [components.numa]，
    /// 未启用（缺省单节点）回落遗留变量，再回落 1 / "1G"。
    pub fn topo_params(&self) -> (String, String, String) {
        let smp = scalar(self.tv(|t| &t.smp), "SMP").unwrap_or_else(|| "8".into());
        let numa = self
            .comp(|c| c.numa.as_ref())
            .filter(|n| n.enabled.unwrap_or(false));
        // 优先级冻结：NUMA_NODES / NUMA_MEMORY env 覆盖 toml（组件启用与否皆然）
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

    /// 组件计划：启用组件的 require 并集（按 schema 固定顺序
    /// tools_disk → agent → vfio → numa → pmem → bpf，去重保首个），按 stage 分区。
    /// `[tests]` 段的 require 最后并入（恒 runtime；同名模块组件条目优先，
    /// 测例只补差集）。
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

    /// tools_disk 组件是否启用（段缺省 = true：tools.img 存在即附加，
    /// 与冻结不变量 3 的「调用方增量」语义一致）。
    pub fn tools_disk_enabled(&self) -> bool {
        self.comp(|c| c.tools_disk.as_ref())
            .and_then(|c| c.enabled)
            .unwrap_or(true)
    }

    /// agent 组件是否启用（段缺省 = false：argv 保持冻结基线）。
    pub fn agent_enabled(&self) -> bool {
        self.comp(|c| c.agent.as_ref())
            .and_then(|c| c.enabled)
            .unwrap_or(false)
    }

    /// bpf 观测组件（bpftrace 随 tools 盘供给）是否启用。
    /// 段缺省 = false：纯构建侧开关，QEMU argv 与冻结基线无关。
    pub fn bpf_enabled(&self) -> bool {
        self.comp(|c| c.bpf.as_ref())
            .and_then(|c| c.enabled)
            .unwrap_or(false)
    }

    /// pmem 组件 size（启用才 Some；未写 size 用缺省 "256M"）。
    /// 段缺省（或 enabled=false）= 关闭：argv 保持冻结基线。
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

// ---------------------------------------------------------------- BusyBox 供给配置

impl Config {
    /// BusyBox 供给配置（BUSYBOX_* 变量优先，回落 toml [busybox] 段）。
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

/// 启用组件的 KO 依赖并集：`boot_extra` 追加在 modules-boot.conf 冻结
/// 基础集之后（initramfs 阶段加载），`runtime` 生成 rootfs 的
/// /lib/modules/modules.conf（switch_root 后加载）。条目 = conf 行
/// `"<module> [key=val ...]"`，首 token 是模块名（去重键）。
/// builder 消费该计划生成 initramfs 基础集追加与 rootfs modules.conf。
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

    /// 全部条目（boot_extra 在前）—— verify 的模块存在性检查输入。
    pub fn all(&self) -> impl Iterator<Item = &String> {
        self.boot_extra.iter().chain(self.runtime.iter())
    }
}

// ---------------------------------------------------------------- toml schema

// toml schema：`virtuoso.toml` 的类型化结构与解析（`Config`
// 只做访问器，结构真相在这里）。未知键（`deny_unknown_fields`）与非法类型
// 一律解析期报错；`StrVal` 统一收编 string/int 标量，`Stage` 区分 KO 的
// boot/runtime 加载阶段。

/// 接受 string 或 integer 标量并统一成 String（timeout_secs = 60 与 = "60"
/// 等价）。其余类型（bool/array…）在解析期报错。
#[derive(Debug, Clone)]
pub(crate) struct StrVal(pub(crate) String);

impl<'de> serde::Deserialize<'de> for StrVal {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = StrVal;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("字符串或整数")
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

/// 组件 KO 的加载阶段：`boot` = 并入 initramfs（挂 root 前就要），
/// `runtime`（缺省）= switch_root 后由 rootfs init 加载。
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

// 组件公共字段（enabled / require / stage）逐结构体显式声明 —— 不用
// #[serde(flatten)]，它与 deny_unknown_fields 不兼容（未知键会漏过）。

/// 组件公共字段的统一视图：启用判定与 require 并集只看这三个面，
/// 各组件结构体逐项实现（五行样板换掉调用方的五行复制块）。
pub(crate) trait Component {
    fn enabled(&self) -> Option<bool>;
    fn stage(&self) -> Option<Stage>;
    fn require(&self) -> &Option<Vec<String>>;
}

/// 启用组件的 require 并入计划（禁用组件整体跳过）。
pub(crate) fn push_enabled(plan: &mut ComponentPlan, c: &impl Component) {
    if c.enabled().unwrap_or(false) {
        plan.push(c.stage(), c.require());
    }
}

/// tools.img 常驻工具盘（外挂 virtio-blk → /dev/vdb 挂 /tools）。
/// 段缺省即启用；require 默认为空（virtio_blk/ext4 已在 boot 基础集）。
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

/// AI probe 通道（virtio-serial + guest 内 virtuoso-agent）。
/// 段缺省即关闭 —— argv 保持冻结基线（不变量 3）。
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

/// vfio-pci 直通：devices = 宿主设备 BDF 列表，逐条生成
/// `-device vfio-pci,host=<bdf>`。
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

/// 多节点 NUMA 拓扑；段缺省（或 enabled=false）= 单节点。
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

/// 持久内存（QEMU nvdimm → guest /dev/pmem0）；段缺省 = 关闭（argv 保持
/// 冻结基线）。size = 后端文件大小（缺省 "256M"）；require 按被测内核
/// 配置取舍（如 libnvdimm/nfit/nd_pmem 为 =m 时声明）。
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

/// bpf 观测组件：bpftrace（官方 Release AppImage）随 tools 盘供给。
/// 段缺省 = 关闭；纯构建侧开关 —— QEMU argv 与冻结基线无关，运行时
/// 以 `/tools/bin/bpftrace` 直跑（内核侧要求 BPF/BTF，openEuler defconfig
/// 缺省即开）。
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

/// `[tests]` 段：测例套件的 KO 依赖——给内核特性写测例时，被测模块
/// （`=m` 形态）声明的自然归属，不必挂到无关组件上。条目格式与组件
/// `require` 一致（conf 行），恒 runtime 阶段（测例都在 pivot 后跑）；
/// 并入计划时排在五个组件之后 = 同名模块组件条目优先，这里只补差集。
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

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> anyhow::Result<VirtuosoToml> {
        toml::from_str(text).map_err(|e| anyhow::anyhow!("{e}"))
    }

    #[test]
    fn scalar_env_overrides_toml() {
        // 优先级冻结：进程环境变量 > virtuoso.toml。env 改动是进程全局的，
        // 用互斥锁串行化，键名用专用前缀避免污染真实配置键。
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap();
        let toml = StrVal("arm64".into());
        std::env::set_var("VIRTUOSO_TEST_SCALAR", "riscv64");
        assert_eq!(
            scalar(Some(&toml), "VIRTUOSO_TEST_SCALAR").as_deref(),
            Some("riscv64"),
            "同名键进程环境变量必须覆盖 toml"
        );
        std::env::remove_var("VIRTUOSO_TEST_SCALAR");
        assert_eq!(
            scalar(Some(&toml), "VIRTUOSO_TEST_SCALAR").as_deref(),
            Some("arm64"),
            "env 未设置时回落 toml"
        );
        // 空串视为未设置：env 与 toml 的空值都继续回落
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
qemu_opts = ["-device vfio-pci,host=01:00.0"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.arch.as_ref().unwrap().0, "arm64");
        assert_eq!(cfg.timeout_secs.as_ref().unwrap().0, "60");
        assert_eq!(cfg.smp.as_ref().unwrap().0, "4");
        assert_eq!(cfg.auto_test, Some(false));
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
        // 顺序冻结：tools_disk → agent → vfio → numa → pmem；去重保首个；
        // 禁用组件的 require 不并入；stage=boot 分区到 boot_extra。
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
        // 直接复用 Config::component_plan 的拼装逻辑太重（要全量 Config），
        // 这里镜像其调用顺序验证 push 语义。
        let td = comps.tools_disk.as_ref().unwrap();
        let ag = comps.agent.as_ref().unwrap();
        let nm = comps.numa.as_ref().unwrap();
        let pm = comps.pmem.as_ref().unwrap();
        plan.push(td.stage, &td.require);
        plan.push(ag.stage, &ag.require);
        // vfio disabled —— require 不并入
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
        // 段缺省 = 关闭：size 访问器返回 None
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
        // 段缺省 = 无段；空段/未写 enabled = 关闭
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
        // 测例套件没有开关语义（auto_test 是全局键）：enabled 必须报错
        assert!(parse("[tests]\nenabled = true\n").is_err());
        assert!(parse("[tests]\nstage = \"boot\"\n").is_err());
    }

    #[test]
    fn unknown_key_is_rejected_at_parse_time() {
        assert!(
            parse("arch = \"arm64\"\nshmp = 4\n").is_err(),
            "拼写错误的键必须在解析期报错"
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
        assert_eq!(plan.runtime, ["nvme-core poll_queues=2"]);
        plan.push(Some(Stage::Boot), &Some(vec!["nvme-core".into()]));
        assert_eq!(plan.boot_extra, ["nvme-core"], "去重只在同 stage 分区内");
    }
}
