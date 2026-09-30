# 配置参考

`virtuoso.toml` 是唯一配置面。仓库根的模板即缺省常规启动配置，可选能力全部
以注释形式在场，取消注释即启用。

- **优先级**：同名标量键以**进程环境变量**为准（env 覆盖 toml，CI / 命令行
  临时改参不动文件）；持久配置只写 toml。
- **严格解析**：未知键 / 非法类型解析期报错，不静默忽略。
- 诊断呈现：`virtuoso doctor`（一屏）或 `virtuoso doctor --verbose`（全量清单 + 类型化配置诊断）。

## 全局键

| 键 | 环境变量 | 说明 |
|---|---|---|
| `arch` | `ARCH` | `x86_64` \| `arm64` \| `riscv64`（缺省 = 宿主架构） |
| `timeout_secs` | `QEMU_TIMEOUT` | 墙钟超时秒数；0 一律拒绝 |
| `smp` | `SMP` | vCPU 总数；多节点 NUMA 时必须被节点数整除（解析期报错，启动期二次校验） |
| `auto_test` | `AUTO_TEST` | true = 跑完 `/tests/` 自动关机；false = 落入交互 shell |
| `kernel_path` | `KERNEL_PATH` | 内核树路径（装置在树内时可自动探测；**docker 模式下被活动卷覆盖**，仅 raw 模式生效——见下节解析序） |
| `kernel_image` | `KERNEL_IMAGE` | 内核镜像覆盖（缺省 = 内核树内 arch 对应镜像） |
| `qemu` | `QEMU` | QEMU 二进制覆盖（`QEMU=echo` 可打印 argv 对照） |
| `qemu_opts` | `QEMU_OPTS` | 透传兜底参数数组（env 为空白切分；vfio 组件设备由装配点单独追加，不混入本链） |

## docker 内核供给的环境变量（无 toml 键）

`virtuoso kernel` 命令组（[容器化内核开发环境](../../devkit/docker/README.md)）
不占 `virtuoso.toml` 键，只走环境变量（优先级同上：env > 状态文件 > 内置缺省）：

| 环境变量 | 说明 |
|---|---|
| `KERNEL_VOLUME` | 活动卷覆盖（压过 current 状态文件 `.virtuoso/kernel-current.json`） |
| `KERNEL_ARCH` | 目标架构覆盖（缺省 = 顶层 `arch`） |
| `KERNEL_REF` | `kernel clone` 缺省 ref（缺省 master） |
| `KERNEL_TOOLCHAIN_IMAGE` | 工具链镜像覆盖（缺省 ghcr.io/yxalix/virtuoso-kernel:latest；与 `KERNEL_IMAGE` 无关） |

**内核树解析序**（build/test/doctor 消费哪棵树，唯一规则）：

```
env KERNEL_PATH > current 活动卷（KERNEL_VOLUME env > 状态文件）> toml kernel_path > 缺省（项目根上一级）
```

`kernel use/clone` 写下的活动卷压过 toml——切卷即切换测试目标，无需手动改
`kernel_path`；env 临时覆盖恒最高。`kernel_path` 键只在 raw 模式（无活动卷）
生效。

## `[components.*]` 组件段

每个组件段的公共字段（`enabled` / `require` / `stage`）与逐组件的专属键见
[组件机制](../concepts/components.md)。

## `[tests]` 段

| 键 | 说明 |
|---|---|
| `require` | 测例套件的内核模块依赖（`"<module> [key=val ...]"`，恒 runtime 阶段；同名模块组件条目优先，只补差集）。给内核特性写测例时被测模块（`=m`）声明的自然归属 |

## `[busybox]` 段

| 键 | 说明 |
|---|---|
| `version` | BusyBox 版本（如 `"1.36.1"`） |
| `release_repo` | 预编译 Release 仓库（缺省扫描 git remotes 找 github.com） |
| `dl_url` | 显式下载 URL（供给链第一优先级） |

供给链 = GitHub Release 拉取 + 本地缓存复用，无源码编译兜底（全部未命中即报错）。
详见[供给与构建流水线](../concepts/pipeline.md)。

## 架构矩阵

| arch | QEMU 二进制 | 内核镜像 | 控制台 | 机型 |
|---|---|---|---|---|
| `arm64`（缺省） | `qemu-system-aarch64` | `arch/arm64/boot/Image` | `ttyAMA0` | `virt` |
| `x86_64` | `qemu-system-x86_64` | `arch/x86/boot/bzImage` | `ttyS0` | `q35` |
| `riscv64` | `qemu-system-riscv64` | `arch/riscv/boot/Image` | `ttyS0` | `virt` |

交叉组合随意（如 arm64 宿主跑 `--arch x86_64`），目标与宿主不同构时
doctor 会告警，启动自动回退 TCG。
