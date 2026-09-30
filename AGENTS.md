# Virtuoso Agent Guide

本文件只写**规则与共识**：AI 在本仓库工作必须遵守的契约、命令面与验证循环。
设计叙事、组件机制、配置全键、工件详解等**一律以 `docs/` 为唯一事实来源**
（[在线站点](https://yxalix.github.io/virtuoso/)），本文件不复制其内容。

## 定位与权威

Virtuoso 是 QEMU 内核 E2E 测试装置：供给内核 → 构建 BusyBox+musl 静态测试固件 →
QEMU 启动跑 `/tests/` → 串口标记协议判定 → verdict.json 落盘。**单包 Rust crate
是唯一行为权威**（`src/` 下：`main` 子命令分发 + `cli` 命令实现 + `config` 类型化
配置 + `runs` 运行工件 + 领域模块 `builder` 构建 / `launcher` 启动+进程治理 /
`judge` 判定 / `forge` 容器化内核供给；`arch`（Arch 矩阵 + HostOs）与
`util`（顶层工具集）为原 common 摊平）；
`infra/` 是 VM 内源资产（init、testcases、tools、busybox 名单——多为冻结数据）；
仓库根 `rootfs.d/` 是用户 drop-in（构建期增量并入 rootfs，只增不覆盖；git 忽略，勿提交）；
`devkit/` 是内核开发容器与 AI skill 源；构建产物与运行工件落 `target/`（git 忽略）。

## 快速命令（复制即用）

`virtuoso` = 规范二进制（`cargo install --path .` 装入 PATH）；改完 CLI 源码
重跑一次安装即可保持最新。

```bash
virtuoso doctor             # 环境体检（✓/✗ 组件行；--verbose 全量诊断；--json）
virtuoso build              # builder：重建 initrd.img / rootfs.img / tools.img（--busybox-only 仅备 BusyBox）
virtuoso kernel clone <url> [--ref r] [--as vol] [--arch a] [--full]
                            # forge：内核源码进 named volume（+ .clangd 渲染 + 写 current；--full 全量历史）
virtuoso kernel build       # forge：容器 make Image/modules + 静态 bpftool + CDB（/ksrc 原始形态）
                            #   （另有 defconfig/path/shell/list/use；源码编辑走 devkit/docker devcontainer）
virtuoso test --timeout 60  # 测试：launcher 启动 → judge 判定 → 工件落盘
virtuoso test --replay-until-fail 5   # flaky 返场：首个非 passed 即停
virtuoso shell [--kvm|--tcg] [--gdb]
                            # 交互式 VM；--gdb = 挂起等 GDB :1234（恒 TCG）
virtuoso probe --cmd 'uname -a' [--cmd-file f] [--json] [--timeout s]
                               # AI 交互通道：virtio-serial agent 命令批（结构化事件流；--timeout 缺省 300）
virtuoso skill install      # 装 kernel-dev + kernel-virtuoso skill 到内核树
```

AI 的标准验证循环：`clippy → doctor → test`。改 Rust 源码必须先跑
`cargo clippy --all-targets -- -D warnings`（CI 同款门禁，本地不过别提交）；
再 `doctor → test`。**判定以 test 收尾的 verdict 行为准，
机读唯一面 = run 目录下的 `verdict.json`**；退出码只是接口契约；
`verdict: passed` 才算通过。doctor 是体检唯一入口（一屏
呈现 + `--verbose` 全量），与 builder::verify 检查引擎同源（新增前置条件只动
引擎，呈现自动跟随）。

## 冻结的不变量（不要破坏）

1. **标记协议 v1**（`infra/init` 输出，`docs/concepts/contracts.md` 冻结文本）：
   `--- Running: X ---`、`PASSED:/FAILED: X`、`Test Results: N/M passed`、
   `TEST_COMPLETE: ALL TESTS PASSED|SOME TESTS FAILED`。改文本等于破坏所有下游解析。
2. **test 退出码**：0=通过、124=超时、其余=失败。
3. **argv 冻结**：`QemuInvocation::argv` 的输出冻结在**按宿主平台的双基线**上
   （Linux=memfd 后端、macOS=ram 后端、HVF→`-accel hvf`），由
   `src/launcher/qemu/argv.rs` 的 `argv_*` 单测显式钉死平台把守；
   人工复核用 `QEMU=echo virtuoso shell` 打印 argv。数据盘与 agent 通道属调用方
   增量：**缺省（无盘无 agent）argv 与所属平台的基线逐字一致**。
4. 测试必须静态链接（`-static`），禁止用 `|| true` 掩盖失败。
5. **配置优先级：进程环境变量 > `virtuoso.toml`**：同名标量键以进程环境变量
   为准（CI/命令行临时改参不动文件）；持久配置只写 `virtuoso.toml`。

## 共识

- **文档唯一事实来源**：`docs/`（mdBook 书根 = `docs/`，`mdbook build docs`
  构建，push main 自动发布 gh-pages）。改动文档只动 `docs/`，别处引用不复制内容。
  入口：`docs/concepts/`（理念/流水线/引导/契约/组件）、`docs/usage/`（CLI/配置/
  测试编写/调试/工件/AI 集成/故障排查）、`docs/quick-start.md`、`docs/contributing.md`。
- **组件化配置**：`virtuoso.toml` 唯一配置面（env > toml），`[components.*]`
  段声明 VM 能力；builder 按启用组件 require 并集生成模块清单。全键与语义见
  `docs/usage/configuration.md`，模板见仓库根 `virtuoso.toml` 注释。
- **运行工件**：`virtuoso test` 写 `target/runs/<unix_ms>-<arch>/`（保留 20 次），
  `verdict.json` 是判定唯一机读面、`events.jsonl` 是逐事件结构化事实源。
  文件表与 Verdict 八态语义见 `docs/usage/artifacts.md`。
- **内核开发容器**：源码权威在 named volume，宿主经 `virtuoso kernel path` 的
  平台视图编辑（视图大小写保真，严禁在 APFS checkout 内核树、严禁双 make
  并行）。详见 `devkit/skills/kernel-dev/SKILL.md` 与 `docs/quick-start.md`。
- **已知环境怪癖（勿"修复"）**：见 `docs/concepts/boot.md` 怪癖表——
  空 `/dev` 回退 mknod、virtio/ext4 必须 `=m` 进 initramfs 等都是 openEuler
  内核实测行为，删掉对应 fallback 会重新踩坑。
