# devkit/docker/ —— 容器化内核开发环境

**容器只做宿主做不到/不便做的事**：钉死内核构建工具链、在大小写敏感的
容器侧 ext4 上构建。源码权威存放在 **named volume**，源码编辑统一走 VS Code
devcontainer（容器内 clangd）；宿主经平台视图读 volume 只为消费构建产物
（QEMU 的 kernel_path）。测试资产构建（busybox / 用例 workspace）与 QEMU
运行始终留在宿主原生（macOS 走 HVF、Linux 走 KVM，容器里都没有）。

命令面：`virtuoso kernel` 子命令组
（逻辑在 `src/forge/`）。双平台同一套命令，唯一分支是 volume 的宿主可见
路径：

| | macOS（OrbStack） | Linux（docker-ce） |
|---|---|---|
| volume 宿主路径 | `~/OrbStack/docker/volumes/<卷>`（OrbStack 视图，读写） | `/var/lib/docker/volumes/<卷>/_data`（volume 本体；rootless 落在 `$HOME` 下免 root） |
| `virtuoso kernel path` 输出 | 同上 | 同上（取引擎权威 Mountpoint） |
| QEMU 加速 | HVF | KVM |

Docker Desktop 的 volume 藏在 VM 虚拟盘里宿主不可见——macOS 上请用
OrbStack（doctor 会守卫引擎端点）。

| 文件 | 作用 |
|---|---|
| `Dockerfile.kernel` | 钉死工具链：ubuntu:24.04 + 内核构建依赖 + 三架构 gcc + clangd + Rust for Linux 工具链（rustc/bindgen 钉主线树 `scripts/min-tool-version.sh` 下限，rust-src/clippy/rustfmt 全配） |
| `devcontainer.json` | 静态模板（缺省卷形态）；`kernel use/clone` 另按 current 渲染 git 忽略的 `.devcontainer/devcontainer.json`（repo 根）—— VS Code「Reopen in Container」→ volume 为 workspace 的容器内编辑路径（源码编辑的标准入口） |
| `.clangd` | clangd 配置模板（唯一事实来源）：`kernel clone` 时按目标架构渲染写进源码根 |

## 为什么源码在 named volume 里

1. **大小写敏感**：Linux 内核构建要求大小写敏感文件系统；macOS 的 APFS
   默认不敏感。volume 是容器侧 ext4，天然正确。
2. **性能**：volume 的 I/O 走容器原生文件系统，构建速度接近原生 Linux
   （bind-mount 走 virtiofs 慢 2–5 倍，别用）。
3. **多内核切换**：每卷自含源码 + `.config` + 增量产物，切换 = 换卷名，
   切回旧内核免重编；卷的创建成本接近零（clone 缺省 `--depth 1` 只取树；
   要跟主线学习、需要 `git log/blame` 完整历史时加 `--full`，多下 ~3 GB）。

## 标准流程（首跑）

```bash
# 0) 依赖：brew install qemu e2fsprogs dtc zig + OrbStack（macOS）
#    Linux: 发行版 docker + qemu
# 1) 内核源码进 volume（+ 按架构渲染 .clangd + 写 current）
virtuoso kernel clone https://gitcode.com/openeuler/kernel.git --ref OLK-6.6

# 2) 配置 + 构建（arm64 容器 = 原生前端，其余交叉）
virtuoso kernel defconfig openeuler_defconfig
virtuoso kernel build              # Image/bzImage + modules + compile_commands.json（/ksrc 形态）

# 3) 内核树对宿主可见：`virtuoso kernel path` 打印的目录就是它
virtuoso kernel path
```

内核树与 build/test/doctor 的闭环是**自动的**：`kernel use/clone` 写下的
current 活动卷就是内核源码树（解析序 = env KERNEL_PATH > current 活动卷 >
toml kernel_path > 缺省），OrbStack 视图 / Linux volume 本体都是纯宿主路径，
virtuoso 直接读 Image 与 `.ko`。切卷即切换测试目标，无需手动改 `kernel_path`
（该键只在 raw 模式——无活动卷时——生效）。跑宿主主循环：

```bash
virtuoso doctor && virtuoso build && virtuoso test
```

## 卷管理与多内核切换

```bash
virtuoso kernel clone <url> --ref OLK-6.6 --as ksrc-openEuler-6.6
virtuoso kernel clone <url> --ref master      --as ksrc-mainline --full

virtuoso kernel list        # 全部卷 + 内容状态（empty/cloned/configured）+ current 标记
virtuoso kernel use ksrc-mainline   # 切 current（写 .virtuoso/kernel-current.json）
```

`current` 指针是专门的会话状态文件 `.virtuoso/kernel-current.json`
（git 忽略）：`use`/`clone` 写入，之后所有 `virtuoso kernel` 子命令都作用
于它（`KERNEL_VOLUME` env 可临时压过，语义与其它键一致：env > 状态文件 > 缺省）。
`virtuoso.toml` 的 `kernel_path` 跟着指向对应卷的宿主路径。

## AI 闭环（单 AI 单工作区）

源码查看/编辑在 VS Code devcontainer 里进行（workspace 即 /ksrc）；构建
是 AI 眼里的一条普通 shell 命令（`virtuoso kernel build`），验证在宿主：

```bash
virtuoso kernel build                           # 容器 make（volume 原生 I/O）
virtuoso doctor && virtuoso build && virtuoso test
```

判定以 test 收尾的 verdict 行为准（机读唯一面 = run 目录下的
`verdict.json`，`verdict: passed` 才算通过）；配合
`virtuoso skill install`（KERNEL_PATH 指向宿主可见路径）注入 kernel-dev /
kernel-virtuoso 两个 skill，AI 即具备驱动测试回路 / 分诊 / 写用例的知识。

## clangd / 浏览内核源码

源码浏览/编辑统一走 **VS Code devcontainer**。`kernel use/clone` 写 current
时自动渲染 git 忽略的 `.devcontainer/devcontainer.json`（repo 根）：
workspaceMount 指向 current 卷，image 直用 ghcr 钉死工具链镜像——首进免本地
build `Dockerfile.kernel`（`KERNEL_TOOLCHAIN_IMAGE` 渲染时已代入）：

**打开 repo 根 → 命令面板「Dev Containers: Reopen in Container」**（VS Code
自动发现 `.devcontainer/`，无需手动选隐藏目录），workspace 即 `/ksrc`
（current 卷）。切卷 = `virtuoso kernel use <卷>` 后 Reopen/Rebuild Container。
`devkit/docker/` 目录的静态模板挂缺省卷 `virtuoso-kernel`，仅在未用 `--as`
命名时可用。落点必须叫 `.devcontainer/`——VS Code 的自动发现契约只扫打开
工作区下的 `.devcontainer/`（或根级 devcontainer.json），放 `.virtuoso/`
就只剩手动选隐藏目录一条路。

clangd 在容器内跑，吃 `build` 末尾自动产出的 `/ksrc` 原始形态
`compile_commands.json` 与源码根的 `.clangd`（clone 时按架构渲染）——跳转/
补全/悬停开箱即用，无需任何改写步骤。改了 `.config` 或索引滞后时，在容器
集成终端重跑 `virtuoso kernel build`（或
`python3 scripts/clang-tools/gen_compile_commands.py`）。

注意容器只在你显式打开 devcontainer 时常驻：`virtuoso kernel build` 等命令
跑一次性容器（`--rm`），结束即退——「Attach to Running Container」列表里
没有它们是正常现象，进容器走上面的 Reopen in Container。VS Code server 与
扩展由扩展自动持久在 named volume `vscode`（挂 `/vscode`）——同版本 VS Code
重开/重建容器不重装；别手动删这个卷。日志里每次开窗出现的「Downloading VS
Code Server」多为例行落地：tarball 走宿主缓存（`serverCache`），真正重新
下载只发生在 VS Code 升级（commit 变化）之后。

**远端服务器（Remote-SSH）同一套流程**：先在服务器上 `virtuoso kernel
clone/use`（渲染产物落在服务器侧 repo），本地 VS Code Remote-SSH 连上 →
打开 repo 根 → 首次「Reopen in Container」时 Dev Containers 扩展自动装进
远端 → 容器起在服务器上（Linux 分支：volume 本体 + KVM 同机可用）。本地
macOS 与远端 Linux 命令面完全一致，差别只是 docker 引擎在哪台机器。

**双容器共享卷的纪律**：构建容器（一次性）与 devcontainer（常驻）可同时
挂同一卷，互不排他——但同一棵树**绝不允许两个 `make` 并行**（增量状态
`.*.cmd`/`.o`/`Module.symvers` 无锁，会真损坏）；构建期间可以编辑源码，
但构建结果别当真，改完增量重跑。AI/编辑器在宿主视图（ext4 直通，大小写
保真）上直接改代码，`virtuoso kernel path` 输出即该路径。

## 镜像发布

`kernel-builder.yml` 在 `devkit/docker/` 变更推 main 时构建并发布
`ghcr.io/yxalix/virtuoso-kernel`（linux/arm64 + linux/amd64）。
`virtuoso kernel` 拉不动镜像时自动回落本地构建 `Dockerfile.kernel`；
镜像可用 `KERNEL_TOOLCHAIN_IMAGE` 覆盖（`KERNEL_IMAGE` 与此无关——那是
virtuoso 启动内核镜像的配置键）。

## 换目标架构

架构缺省取 `virtuoso.toml` 的顶层 `arch`（测哪个编哪个），
`KERNEL_ARCH` env 或 `--arch` 临时覆盖：

```bash
virtuoso kernel clone <url> --arch riscv64
virtuoso kernel build --arch riscv64
```
