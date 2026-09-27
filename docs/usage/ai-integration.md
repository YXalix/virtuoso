# AI 集成

Virtuoso 对 AI 代理是一等公民：结构化事件流做事实源、skill 做知识注入、
probe 做运行时交互。

## 标准验证循环

```
doctor → test
```

**判定以 test 收尾的 verdict 行为准**（机读唯一面 = run 目录下的
`verdict.json`）：`verdict: passed` 才算通过。退出码只是
接口契约（0=通过、124=超时、其余=失败）——`-no-reboot` 下内核 panic 会让
QEMU 以 exit 0 退出，只看退出码会假通过。verdict 八态语义见
[运行工件与分诊](artifacts.md)。

## 数据接口

| 能力 | 输入 | 输出 | 对接点 |
|---|---|---|---|
| **测试脚手架生成** | 自然语言描述 / git diff | 用例 crate（Rust 入口 + C 体） | builder 编译即用 |
| **串口日志分诊** | `events.jsonl` / `verdict.json` | 根因假设 + 建议复现命令 | 直读 run 目录工件（`verdict.json` 八态 + `events.jsonl` 逐事件） |
| **VM 内交互探测** | shell 命令批 | 结构化事件流（`agent-events.jsonl`） | `virtuoso probe`（virtio-serial + virtuoso-agent） |

## skill

```bash
virtuoso skill install      # kernel-dev + kernel-virtuoso skill 装入内核树
virtuoso skill uninstall
```

- `kernel-dev`：教 AI 驱动测试回路 / 写用例 / 解析串口 / 分诊失败；
- `kernel-virtuoso`：AI 数据接口集成（probe 通道）。

装入后 AI 在内核树内自动发现（`.claude/skills/` 机制）。

## probe 通道

`virtuoso probe` 恒开 agent 通道（不依赖组件开关），经 guest 侧
virtuoso-agent 下发命令批，结构化事件流回吐：

```bash
virtuoso probe --cmd 'uname -a' --cmd 'cat /proc/iomem' --json
```

通道机制见[组件机制 agent 一节](../concepts/components.md#agent--ai-probe-通道)。

## eBPF 观测（程序态）

观测的三条形态互补，前两条随镜像供给、默认可用：

- **脚本态**：`bpftrace`（`[components.bpf]` 组件，默认关）做临时探索，
  one-liner 即写即跑；
- **程序态**：`bpf-run` 常驻 tools 盘 `/bin`（默认随盘，与组件开关无关），
  消费宿主编译好的现成 `.bpf.o`，按段挂接（kprobe/kretprobe/tracepoint/
  raw_tracepoint），ringbuf 事件自动解码成 JSONL——stdout 恒为纯事件流，
  经 probe 通道落 `agent-events.jsonl`，即 AI 的结构化观测闭环；
- **控制面**：`bpftool` 同样常驻 `/bin`（默认随盘）——`virtuoso kernel
  build` 从内核树顺带静态构建（in-tree libbpf 版本与被测内核严格匹配），
  `btf dump` 检视 vmlinux/模块 BTF、`prog show` / `map dump` 观察加载态，
  是 bpf-run 工作流的检视伴侣（内核树无产物时构建 WARN 跳过）。

`.bpf.o` 的供给走 rootfs.d drop-in（build 期并入 rootfs，"每次编译好的
现成程序"）：

```bash
clang -target bpf -g -O2 -c my.bpf.c -o rootfs.d/bpf/my.bpf.o
virtuoso build
virtuoso probe --cmd 'bpf-run /bpf/my.bpf.o --wait 5'
```

`bpf-run` 要点：

- 事件解码按记录长度匹配对象 BTF 里的具名 struct（`--struct NAME` 可显式
  指定）；匹配不到的记录 hex 兜底。**record 结构体须挂进 BTF**：aya 拒绝
  ringbuf 的 values 注解，且 `-O2` 会裁掉仅被 reserve 使用的类型——用一行
  extern 函数锚把结构体带进 BTF：
  `struct event *bpf_anchor_event(struct event *e) { return e; }`
- 挂接失败不中止整体（逐程序 warn，全失败才退出 1；load 错误链带
  verifier 日志，即内核 BPF 开发的反馈面）；`--program NAME` 过滤，
  `--wait SECS` 限定采集窗（缺省 5s），退出码 0=正常（含零事件）。
- 对象 BTF 可用 dump-btf 示例检视（struct 是否进表、map 是否识别为
  ringbuf）：`cargo run --manifest-path infra/tools/bpf-run/Cargo.toml
  --example dump-btf -- <obj.bpf.o>`。

临时性、一次性的内核态观测不需要写 BPF 程序：ftrace（tracefs 已挂载，
`/sys/kernel/tracing/`）与 bpftrace 已覆盖。

## 安全边界（架构约束）

- AI **只读分析**测试产物与内核日志；对内核源码的任何修改必须人工确认后由
  开发者执行；
- `events.jsonl` 是 AI 的唯一结构化事实源，串口原文仅作补充上下文，保证分诊
  可回溯；
- skill 与 harness 的接口是[冻结契约](../concepts/contracts.md)：skill 只
  依赖协议与工件 schema，不依赖实现。
