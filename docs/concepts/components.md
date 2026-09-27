# 组件机制

VM 能力按组件配置在 `virtuoso.toml` 的 `[components.*]` 段。组件是
**声明式的**：开关 + KO 依赖，builder 负责把启用组件的需求并成模块清单、
launcher 负责生成对应的 QEMU 参数。

## 组件一览

| 组件 | 作用 | 段缺省 | 专属键 |
|---|---|---|---|
| [tools_disk](#tools_disk--vm-内工具盘) | tools.img 数据盘 → `/dev/vdb` 挂 `/tools`（VM 内工具，musl 静态） | **启用** | — |
| [agent](#agent--ai-probe-通道) | AI probe 通道（virtio-serial + guest 侧 virtuoso-agent） | 关闭 | — |
| [vfio](#vfio--pci-直通) | PCI 直通，逐条生成 `-device vfio-pci,host=<bdf>` | 关闭 | `devices` |
| [numa](#numa--多节点拓扑) | 多节点拓扑（每节点一个 socket） | 关闭 | `nodes`、`memory_per_node` |
| [pmem](#pmem--持久内存) | 持久内存（DT 途径 → `/dev/pmem0` + DAX） | 关闭 | `size`、`require` |
| [bpf](#bpf--bpftrace-观测) | bpftrace 随 tools 盘供给（eBPF 内核观测） | 关闭 | — |

`[busybox]` 是全局段（非组件），`[tests]` 是测例套件段（收用例需要的
模块依赖），见[配置参考](../usage/configuration.md)。

## 公共字段

每个 `[components.<name>]` 段支持：

- `enabled`：开关。**段缺省 = 各组件自己的缺省**（tools_disk 缺省启用，其余缺省关闭）。
- `require`：KO 依赖，条目 = conf 行 `"<module> [key=val ...]"`（token 原样透传 insmod）。
- `stage`：`boot`（root 挂载前就要）｜ `runtime`（缺省）。

```toml
[components.agent]          # 段缺省 = 关闭；取消注释即启用
enabled = true
require = ["virtio_console"]
```

仓库根的 `virtuoso.toml` 模板中，可选组件全部以注释形式在场，取消注释即启用。

## 模块清单由组件生成（不手写）

模块供给是组件机制的直接产物（双清单规则的完整版见
[两段式引导](boot.md#3-模块双清单规则)）：

- `infra/modules-boot.conf`（**冻结基础集**：virtio + ext4 及依赖）进 initramfs，
  由 `init-initramfs` 在 pivot 前 insmod；
- 组件条目 `stage = "boot"` 追加到基础集之后；
- 其余来自启用组件的 `require` **并集**（外加 `[tests]` 段的测例模块依赖，
  排在最后），builder 写入 rootfs `/lib/modules/modules.conf`，测试 init 在
  pivot 后加载。

并集规则：schema 固定顺序 tools_disk→agent→vfio→numa→pmem→bpf→`[tests]`，
按首 token 去重保首个。

规则与陷阱：

- **顺序敏感**：按清单顺序 insmod（无自动依赖解析），依赖在前。
- **模块必须已构建**：`kernel_path` 下找不到对应 `.ko` 直接构建报错。
- **迭代中的模块优先 `=m` 而非 `=y`**：免内核重建，回路更快。
- 判断 `stage` 的标准：**这个模块是不是"root 挂上之前就必须在内核里"**？
  是 → `stage = "boot"`，否 → 缺省 runtime。

`virtuoso probe` 恒开 agent 通道（强制并入 `virtio_console`，不依赖组件开关）。

## tools_disk — VM 内工具盘

把 `infra/tools/` workspace 构建的常驻工具装进独立数据盘，随 VM 外挂，
与测试用例（`/tests/`，参与判定）正交分离。

```toml
[components.tools_disk]   # 段缺省 = 启用；显式关闭：
# enabled = false
```

工作方式：builder 构建 `infra/tools/`（std Rust + **musl 静态**）装入 ext4
数据盘 `/bin/`（卷标 `tools`）；启动时作为 rootfs 之后的第一个 virtio-blk
数据盘附加（guest 内 `/dev/vdb`，rootfs 恒 `/dev/vda`）；rootfs 的
`/init-hooks.sh` 把它挂到 `/tools` 并把 `/tools/bin` 注入 `PATH`（位于
devtmpfs 挂载后、insmod / agent 拉起前）。

工具不进 `/tests/`、不参与判定：它们是运行环境的一部分（如 `virtuoso-agent`），
不是被测对象。新工具 = `infra/tools/` 下新 crate，musl 静态产物自动进
tools.img 的 `/bin/`。

降级行为：工具供给缺失（如宿主缺 musl target）时**显式降级非掩盖**——不产出
tools.img、不注入挂载 hook，构建日志给出原因。`enabled = false` 时同理会跳过
整条链路。

## agent — AI probe 通道

宿主与 guest 之间的结构化交互通道：virtio-serial 串口 + guest 侧
`virtuoso-agent`（tools.img 内），用 JSON 行协议双向通信。这是
`virtuoso probe`（[AI 集成](../usage/ai-integration.md)）的底座。

```toml
[components.agent]        # 段缺省 = 关闭（argv 保持冻结基线）
enabled = true
require = ["virtio_console"]
```

`virtuoso probe` **恒开** agent 通道（强制并入 `virtio_console`），不依赖
本组件开关——临时探测不用改配置。

工作方式：启用后 launcher 追加 virtio-serial 设备参数（属调用方增量，追加在
argv 冻结基线之后）；guest 侧 `infra/init` 经 `/init-hooks.sh` 拉起
`/tools/virtuoso-agent`；agent 在 virtio-serial 上收 JSON 行命令、回 JSON 行
事件（结构化事件流，非模拟终端敲键盘）；宿主侧事件流落盘 run 目录的
`agent-events.jsonl`。

```bash
virtuoso probe --cmd 'uname -a' --cmd 'dmesg | tail'
virtuoso probe --cmd-file cmds.txt --json    # 机器可读事件流，供 AI 管道消费
```

`--timeout` 覆盖墙钟总超时（含 TCG 引导与握手）；退出码语义 0/1/124。
probe 运行也写 run 目录（`serial.log` + `qemu-stderr.log` +
`agent-events.jsonl`，无 verdict）。

限制：通道依赖 tools.img 供给（agent 二进制在工具盘里）。

## vfio — PCI 直通

把宿主物理 PCI 设备直通给 guest：每个 BDF 生成一条
`-device vfio-pci,host=<bdf>`（追加在 argv 冻结基线之后）。

```toml
[components.vfio]
enabled = true
devices = ["0000:01:00.0"]        # 宿主设备的 BDF 列表（lspci 查）
require = ["vfio", "vfio_pci"]    # guest 内核 =m 时声明
stage = "boot"                    # root 挂载前就要 → boot（缺省 runtime）
```

前置要求：

- **宿主开 IOMMU**：x86_64 内核参数 `intel_iommu=on`（或 `amd_iommu`）；
  arm64 需要 SMMU 平台。
- 设备已绑定宿主 `vfio-pci` 驱动（`driverctl` 或手动解绑再绑）。
- guest 内核侧 `vfio` / `vfio_pci` 可用：`=y` 无需声明，`=m` 时写进 `require`
  （直通设备若要在 root 挂载前就绪，加 `stage = "boot"`）。

限制：

- **仅 Linux 宿主**——vfio-pci 直通架构性依赖 Linux IOMMU，macOS 上 doctor
  直接拒绝。
- 直通设备参数由装配点单独追加，不混入 `qemu_opts` 透传链（避免与本组件
  的逐设备条目重复）。

## numa — 多节点拓扑

多节点 NUMA 拓扑：每节点一个 socket，guest 内可见真实的节点/内存 locality。

```toml
smp = 8                            # 全局键：vCPU 总数

[components.numa]                  # 段缺省（或 enabled=false）= 单节点
enabled = true
nodes = 2
memory_per_node = "1G"
```

约束（解析期校验，启动期拒绝）：

- `smp` 必须被 `nodes` **整除**（解析期报错；launcher 二次校验）；
- 单节点（组件关闭）时不传 `-numa`，argv 保持冻结基线；
- 总内存 = `nodes × memory_per_node`。

launcher 为每个节点生成一个 socket（CPU 与 memory-backend 一一对应），
`-smp` 取全局值。交叉架构 / 无 KVM 时照常工作（TCG 也支持 NUMA 拓扑）。

## pmem — 持久内存

guest 内的持久内存区域：`/dev/pmem0` + DAX，写入对宿主文件持久落盘。
**仅 arm64 / riscv64**（走设备树途径）。

```toml
[components.pmem]                  # 段缺省 = 关闭
enabled = true
size = "256M"                      # 从 guest RAM 顶部挖出（须小于总内存）
require = ["libnvdimm", "nd_btt", "of_pmem", "nd_pmem"]   # 内核 =m 时声明（含顺序）
```

`require` 是实测最小集：`nd_btt` 对 `libnvdimm` 是硬依赖，顺序错或缺失会
insmod 失败。内核把这些选项编成 `=y` 时无需声明。

工作方式（DT 途径三件套）——QEMU 的 NFIT/NVDIMM 途径需要 EFI，openEuler 的
QEMU 又没有 virtio-pmem，所以走**设备树**。launcher 在启动前生成三件套
（落 `target/build/pmem/`）：

1. **dumpdtb** 导出 QEMU 的原生设备树，`fdtput` 注入 `pmem-region` 节点
   （of_pmem 绑定），启动时经 `-dtb` 回写；
2. **cmdline 追加 `mem=<总内存 − pmem 区>`**：把区间排除出内核线性内存
   模型，`devm_memremap_pages` 才能建 ZONE_DEVICE（不排除会 no-map 冲突
   EEXIST）；
3. **主内存后端换宿主文件**（memory-backend-file）：guest 对该区间的写入
   由 KVM 直接落到宿主文件，即持久化。

guest 内：`/dev/pmem0` 出现后即可 mkfs / dax 挂载，重跑 VM 数据仍在。

限制：仅 arm64 / riscv64（DT 途径），x86_64 无此途径；`size` 必须小于
总内存，否则解析期拒绝；macOS 上为 experimental（doctor WARN），链路
（memory-backend-file + dumpdtb/fdtput）未经 HVF 实测前不要依赖。

## bpf — bpftrace 观测

eBPF 内核观测：bpftrace 随 tools 盘供给，VM 内 `/tools/bin/bpftrace` 直跑
（PATH 已由 tools 盘注入），one-liner 输出经 agent 通道或串口回宿主。

程序态观测（加载现成 `.bpf.o` 的 `bpf-run`）**常驻 tools 盘 `/bin`，与
本组件开关无关、恒默认可用**——bpftrace 负责脚本态临时探索，bpf-run
负责编译好的 BPF 程序的结构化事件流，用法见
[AI 集成 · eBPF 观测](../usage/ai-integration.md#ebpf-观测程序态)。

`bpftool` 同样**常驻 `/bin`、与组件开关无关**：`virtuoso kernel build`
从内核树顺带静态构建（in-tree libbpf 同树链入，版本与被测内核严格匹配），
builder 搬运随 tools 盘——`btf dump` / `prog` / `map` 检视的内核开发控制
面。内核树内无产物（未跑 kernel build，或目标 arch 与工具链容器不同构）
时 WARN 跳过，不挡主流程。

```toml
[components.bpf]                   # 段缺省 = 关闭
enabled = true
```

供给形态：官方 Release 的 AppImage（自含 Nix 闭包，内嵌 LLVM/clang），
**build 期**在容器内解包成纯文件树放入 tools 盘 `/tools/nix`（需要 docker，
与 kernel 供给同依赖），VM 内 `ln -s /tools/nix /nix` 后直跑 store 二进制
—— 零运行时解包、零额外内存。首次启用下载 ~190M 并解包，之后缓存复用
（`target/build/bpftrace/`），tools.img 增至 ~600M。

内核侧要求 BPF/BTF（`CONFIG_BPF_SYSCALL` / `DEBUG_INFO_BTF` 等），
openEuler defconfig 缺省即开，无需重建内核。观测入口示例：

```bash
virtuoso probe --cmd 'bpftrace -e "kprobe:handle_mm_fault { @faults[comm]++ } \
  interval:s:2 { print(@faults); } interval:s:8 { exit(); }"'
```

限制：仅 arm64 / x86_64（官方无 riscv64 资产）；bpftrace 0.27 的 `count()`
走 stdlib 宏有展开缺陷，用 `@[comm]++` 等价写法替代。
