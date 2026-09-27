# 快速开始

从零到第一个 `verdict: passed`：装依赖 → 构建被测内核 → 安装 virtuoso →
环境体检 → 首跑 → 分诊。

## 1. 前置条件

- 宿主工具：`rust`（stable）、`zig`（C 测试用例编译器）、`make`、`qemu-system-*`
- 内核源码树（本装置设计为放进内核树内运行，如 `kernel/virtuoso/`）
- openEuler / Fedora：`sudo dnf install -y gcc make wget cpio gzip qemu-system-aarch64 qemu-img`，zig 从 [ziglang.org/download](https://ziglang.org/download/) 获取（发行版源一般不收录）
- Debian / Ubuntu：`sudo apt install -y gcc make zig wget cpio gzip qemu-system-arm qemu-utils`

> initrd 打包已 Rust 原生化，`cpio` / `gzip` / `wget` 不再是硬依赖
> （下载层有 curl 回退）——装了更省事，没装 doctor 也不报 ✗。

### macOS（Apple Silicon，M1–M5）前置

```bash
brew install qemu e2fsprogs dtc zig
```

- `qemu`：`qemu-system-aarch64` 带 HVF 加速（Apple Silicon 上近原生）；
- `e2fsprogs`：`mke2fs`（keg-only，virtuoso 自动探测 keg 路径，无需加 PATH）；
- `dtc`：pmem 组件的 dumpdtb/fdtput 工具（不用 pmem 可不装）；
- `zig`：C 测试用例交叉编译（统一编译器路径：`zig cc -target <musl triple>`
  三宿主一致，产 Linux 静态 ELF；testcases 已全走 cargo，无需 cmake）；
- Rust 侧另需 `rustup target add aarch64-unknown-linux-musl`。
- vfio 直通不支持 macOS（架构性依赖 Linux IOMMU），doctor 会直接拒绝。

## 2. 构建被测内核

```bash
git clone https://gitcode.com/openeuler/kernel.git && cd kernel
cp arch/arm64/configs/openeuler_defconfig .config   # 或自己的 .config
make -j"$(nproc)"
make modules -j"$(nproc)"        # 只要有任何模块是 =m
```

装置默认放在内核树内（自动探测）；放别处时在 `virtuoso.toml` 设 `kernel_path`。

### macOS 上的内核供给（容器化内核开发环境）

macOS 本机编不了 Linux 内核，用 `virtuoso kernel`（容器钉死工具链）：源码
进 named volume（容器侧 ext4，规避 APFS 大小写不敏感坑；Linux 上同一套命令，
volume 本体就在宿主文件系统）：

```bash
virtuoso kernel clone https://gitcode.com/openeuler/kernel.git --ref OLK-6.6
virtuoso kernel defconfig openeuler_defconfig
virtuoso kernel build          # Image + modules + 静态 bpftool + compile_commands.json
virtuoso kernel path           # → volume 的宿主可见路径（QEMU 消费）
```

`virtuoso.toml` 里 `kernel_path` 指 **`virtuoso kernel path` 的输出**（纯宿主
路径，virtuoso 直接读 Image 与 .ko），即可跑宿主主循环。源码查看/编辑走
VS Code devcontainer（`kernel use/clone` 自动渲染 repo 根 git 忽略的
`.devcontainer/devcontainer.json`，打开仓库「Reopen in Container」即进
current 卷的 /ksrc；容器内 clangd 吃 build 产出的 CDB；构建走
`virtuoso kernel build`；远端服务器经 Remote-SSH 同一流程）。
多内核切换见 `virtuoso kernel list` / `use`（current 状态文件
`.virtuoso/kernel-current.json`，devcontainer 随 use 自动跟随）。
详见 [devkit/docker/README.md](../devkit/docker/README.md)。

内核树也可在任何 Linux 机器上构建后 rsync 过来——virtuoso 只要求
`KERNEL_PATH` 指向「Makefile + arch/<a>/boot/<Image> + \*.ko」的树。

## 3. 安装与体检

```bash
cargo install --path .      # 规范二进制 virtuoso 装入 PATH（一次）
virtuoso doctor               # 一屏体检：✓/✗/! 组件行，最快确认接线
```

`doctor` 报 ✗ 时加 `--verbose` 看全量清单与类型化配置诊断（同一检查引擎，
只是呈现繁简之别，同参 `--arch`）。

## 4. 首跑与判定

```bash
virtuoso test --timeout 60    # 构建 → 启动 → 判定 → 工件落盘（收尾打印 verdict 行）
```

加速器语义按宿主平台：**Linux 恒 TCG**（`shell --kvm` 交互式开 KVM）；
**macOS 上宿主与目标同构（arm64）时缺省 HVF**，`--tcg` 强制纯模拟，
交叉 guest（x86_64 / riscv64）自动回落 TCG（慢，超时预算酌情放大）。

**判定以 test 收尾的 verdict 行为准**（机读唯一面 = run 目录下的
`verdict.json`）：`verdict: passed` 才算通过。退出码只是
接口契约（0=通过、124=超时、其余=失败）——`-no-reboot` 下内核 panic 会让
QEMU 以 exit 0 退出，只看退出码会假通过；`exit 0` 但
`verdict: incomplete` = 标记协议没走完，同样按失败处理。

verdict 八态全集与语义见[运行工件与分诊](usage/artifacts.md)。

## 5. 下一步

- 日常操作与全部命令：[CLI 参考](usage/cli.md)
- 改配置 / 换架构 / 开组件：[配置参考](usage/configuration.md)
- 写自己的测试用例：[编写测试用例](usage/writing-tests.md)
- 出了问题：[故障排查](usage/troubleshooting.md)
