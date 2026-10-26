# Components

VM capabilities are configured as components in the `[components.*]` sections
of `virtuoso.toml`. Components are **declarative**: a switch + KO
dependencies — the builder merges the requirements of the enabled components
into a module manifest, and the launcher generates the corresponding QEMU
arguments.

## Component Overview

| Component | Purpose | Section default | Dedicated keys |
|---|---|---|---|
| [tools_disk](#tools_disk--in-vm-tool-disk) | tools.img data disk → `/dev/vdb` mounted at `/tools` (in-VM tools, musl static) | **enabled** | — |
| [agent](#agent--ai-probe-channel) | AI probe channel (virtio-serial + guest-side virtuoso-agent) | off | — |
| [vfio](#vfio--pci-passthrough) | PCI passthrough, emitting one `-device vfio-pci,host=<bdf>` per entry | off | `devices` |
| [numa](#numa--multi-node-topology) | multi-node topology (one socket per node) | off | `nodes`, `memory_per_node` |
| [pmem](#pmem--persistent-memory) | persistent memory (device-tree route → `/dev/pmem0` + DAX) | off | `size`, `require` |
| [bpf](#bpf--bpftrace-observability) | bpftrace shipped with the tools disk (eBPF kernel observability) | off | — |

`[busybox]` is a global section (not a component), `[tests]` is the test suite
section (collecting the module dependencies the test cases need); see the
[Configuration Reference](../usage/configuration.md).

## Common Fields

Every `[components.<name>]` section supports:

- `enabled`: the switch. **Section default = each component's own default**
  (tools_disk defaults to enabled, everything else defaults to off).
- `require`: KO dependencies; an entry = a conf line
  `"<module> [key=val ...]"` (tokens passed through to insmod verbatim).
- `stage`: `boot` (needed before root is mounted) | `runtime` (default).

```toml
[components.agent]          # section default = off; uncomment to enable
enabled = true
require = ["virtio_console"]
```

In the `virtuoso.toml` template at the repo root, every optional component is
present as a comment; uncomment to enable.

## Module Manifests Are Generated from Components (Never Hand-Written)

Module provisioning is a direct product of the component mechanism (for the
full version of the dual manifest rule, see
[Two-Stage Boot](boot.md#3-the-dual-module-manifest-rule)):

- `infra/modules-boot.conf` (the **frozen base set**: virtio + ext4 and their
  dependencies) goes into the initramfs, insmodded by `init-initramfs` before
  the pivot;
- component entries with `stage = "boot"` are appended after the base set;
- everything else comes from the **union** of the enabled components'
  `require` (plus the `[tests]` section's test module dependencies, placed
  last); the builder writes it to the rootfs `/lib/modules/modules.conf`, and
  the test init loads it after the pivot.

Union rule: fixed schema order tools_disk→agent→vfio→numa→pmem→bpf→`[tests]`,
dedupe by first token keeping the first occurrence.

Rules and pitfalls:

- **Order-sensitive**: modules are insmodded in manifest order (no automatic
  dependency resolution); dependencies come first.
- **Modules must already be built**: a `.ko` missing under `kernel_path`
  fails the build outright.
- **Prefer `=m` over `=y` for modules under iteration**: no kernel rebuild,
  a faster loop.
- The criterion for `stage`: **does this module have to be in the kernel
  "before root is mounted"**? Yes → `stage = "boot"`; no → the default
  runtime.

`virtuoso probe` always opens the agent channel (force-merging
`virtio_console`, independent of the component switch).

## tools_disk — In-VM Tool Disk

Puts the resident tools built from the `infra/tools/` workspace onto a
separate data disk attached alongside the VM, cleanly separated from the test
cases (`/tests/`, which participate in judging).

```toml
[components.tools_disk]   # section default = enabled; to disable explicitly:
# enabled = false
```

How it works: the builder builds `infra/tools/` (std Rust + **musl static**)
into the `/bin/` of an ext4 data disk (volume label `tools`); at boot it is
attached as the first virtio-blk data disk after the rootfs (`/dev/vdb` inside
the guest; the rootfs is always `/dev/vda`); the rootfs's `/init-hooks.sh`
mounts it at `/tools` and injects `/tools/bin` into `PATH` (after devtmpfs is
mounted, before insmod / agent startup).

Tools do not go into `/tests/` and do not participate in judging: they are
part of the runtime environment (e.g. `virtuoso-agent`), not the thing under
test. A new tool = a new crate under `infra/tools/`; its musl static artifact
lands in tools.img's `/bin/` automatically.

Degradation: when the tool provisioning is missing (e.g. the host lacks the
musl target), the behavior is **explicit degradation, never masking** —
tools.img is not produced, the mount hook is not injected, and the build log
states the reason. `enabled = false` likewise skips the entire chain.

## agent — AI probe channel

A structured interaction channel between host and guest: a virtio-serial
console + the guest-side `virtuoso-agent` (inside tools.img), speaking a
line-based JSON protocol in both directions. This is the foundation of
`virtuoso probe` ([AI Integration](../usage/ai-integration.md)).

```toml
[components.agent]        # section default = off (argv keeps the frozen baseline)
enabled = true
require = ["virtio_console"]
```

`virtuoso probe` **always** opens the agent channel (force-merging
`virtio_console`), independent of this component's switch — ad-hoc probing
needs no configuration changes.

How it works: when enabled, the launcher appends the virtio-serial device
arguments (a caller increment, appended after the frozen argv baseline); the
guest-side `infra/init` starts `/tools/virtuoso-agent` via `/init-hooks.sh`;
the agent receives line-based JSON commands on the virtio-serial and answers
with line-based JSON events (a structured event stream, not simulated
keystrokes on a terminal); on the host side the event stream is persisted to
the run directory as `agent-events.jsonl`.

```bash
virtuoso probe --cmd 'uname -a' --cmd 'dmesg | tail'
virtuoso probe --cmd-file cmds.txt --json    # machine-readable event stream, for AI pipelines to consume
```

`--timeout` overrides the total wall-clock timeout (including TCG boot and
handshake); exit code semantics are 0/1/124. A probe run also writes a run
directory (`serial.log` + `qemu-stderr.log` + `agent-events.jsonl`, with no
verdict).

Limitation: the channel depends on the tools.img provisioning (the agent
binary lives on the tools disk).

## vfio — PCI passthrough

Passes host physical PCI devices through to the guest: each BDF emits one
`-device vfio-pci,host=<bdf>` (appended after the frozen argv baseline).

```toml
[components.vfio]
enabled = true
devices = ["0000:01:00.0"]        # BDFs of the host devices (look them up with lspci)
require = ["vfio", "vfio_pci"]    # declare when the guest kernel has them as =m
stage = "boot"                    # needed before root is mounted → boot (default runtime)
```

Prerequisites:

- **IOMMU enabled on the host**: x86_64 kernel parameter `intel_iommu=on`
  (or `amd_iommu`); arm64 needs an SMMU platform.
- The device is bound to the host `vfio-pci` driver (via `driverctl`, or a
  manual unbind then rebind).
- `vfio` / `vfio_pci` available in the guest kernel: with `=y` no declaration
  is needed; with `=m` list them in `require` (if the passthrough device must
  be ready before root is mounted, add `stage = "boot"`).

Limitations:

- **Linux hosts only** — vfio-pci passthrough architecturally depends on the
  Linux IOMMU; on macOS doctor rejects it outright.
- Passthrough device arguments are appended separately at the assembly point,
  not mixed into the `qemu_opts` pass-through chain (avoiding duplicates with
  this component's per-device entries).

## numa — Multi-node topology

Multi-node NUMA topology: one socket per node, with real node/memory locality
visible inside the guest.

```toml
smp = 8                            # global key: total vCPU count

[components.numa]                  # section default (or enabled=false) = single node
enabled = true
nodes = 2
memory_per_node = "1G"
```

Constraints (validated at parse time, rejected at launch time):

- `smp` must be evenly divisible by `nodes` (parse-time error; the launcher
  double-checks);
- with a single node (component off), no `-numa` is passed and argv keeps the
  frozen baseline;
- total memory = `nodes × memory_per_node`.

The launcher generates one socket per node (CPUs and memory backends map
one-to-one), and `-smp` takes the global value. Works as usual across
architectures / without KVM (TCG also supports NUMA topologies).

## pmem — Persistent memory

A persistent memory region inside the guest: `/dev/pmem0` + DAX; writes
persist to a host file. **arm64 / riscv64 only** (via the device tree).

```toml
[components.pmem]                  # section default = off
enabled = true
size = "256M"                      # carved out of the top of guest RAM (must be smaller than total memory)
require = ["libnvdimm", "nd_btt", "of_pmem", "nd_pmem"]   # declare when the kernel has them as =m (order included)
```

`require` is the smallest set verified by testing: `nd_btt` is a hard
dependency of `libnvdimm`; a wrong order or an omission makes insmod fail.
When the kernel builds these options `=y`, no declaration is needed.

How it works (the device-tree trio) — QEMU's NFIT/NVDIMM route needs EFI, and
openEuler's QEMU has no virtio-pmem either, so this goes through the **device
tree**. The launcher generates the trio before boot (into
`target/build/pmem/`):

1. **dumpdtb** exports QEMU's native device tree and `fdtput` injects the
   `pmem-region` node (the of_pmem binding), written back at boot via `-dtb`;
2. **cmdline append `mem=<total memory − pmem region>`**: excludes the region
   from the kernel's linear memory model so that `devm_memremap_pages` can
   create ZONE_DEVICE (without the exclusion, no-map conflicts with EEXIST);
3. **the main memory backend switches to a host file**
   (memory-backend-file): guest writes to the region land directly in the
   host file via KVM — that is the persistence.

Inside the guest: once `/dev/pmem0` appears you can mkfs / mount with dax;
data survives VM reruns.

Limitations: arm64 / riscv64 only (device-tree route); x86_64 has no such
route; `size` must be smaller than total memory, otherwise the parse-time
check rejects it; on macOS this is experimental (doctor WARN) — do not rely
on the chain (memory-backend-file + dumpdtb/fdtput) until it has been tested
under HVF.

## bpf — bpftrace observability

eBPF kernel observability: bpftrace ships with the tools disk and runs
directly in the VM at `/tools/bin/bpftrace` (PATH already injected by the
tools disk); one-liner output returns to the host via the agent channel or
the serial console.

Program-mode observation (`bpf-run`, which loads ready-made `.bpf.o`) is
**always resident on the tools disk at `/bin`, independent of this component's
switch, available by default** — bpftrace handles script-mode ad-hoc
exploration, bpf-run handles structured event streams for compiled BPF
programs; for usage see
[AI Integration · eBPF observation](../usage/ai-integration.md#ebpf-observation-program-mode).

`bpftool` is likewise **resident at `/bin`, independent of the component
switch**: `virtuoso kernel build` builds it statically along the way from the
kernel tree (in-tree libbpf linked from the same tree, version strictly
matching the kernel under test), and the builder ferries it onto the tools
disk — the kernel-development control plane for `btf dump` / `prog` / `map`
inspection. When the kernel tree has no products (kernel build not run, or
the target arch differs from the toolchain container), it WARNs and skips
without blocking the main flow.

```toml
[components.bpf]                   # section default = off
enabled = true
```

Provisioning shape: the official Release's AppImage (a self-contained Nix
closure with embedded LLVM/clang), **unpacked at build time** inside the
container into a pure file tree placed on the tools disk at `/tools/nix`
(needs docker, same dependency as kernel provisioning); inside the VM,
`ln -s /tools/nix /nix` and the store binaries run directly — zero runtime
unpacking, zero extra memory. The first enablement downloads ~190M and
unpacks it; afterwards it is cached and reused (`target/build/bpftrace/`),
growing tools.img to ~600M.

The kernel side requires BPF/BTF (`CONFIG_BPF_SYSCALL` / `DEBUG_INFO_BTF`
etc.); the openEuler defconfig has these on by default, so no kernel rebuild
is needed. Example observation entry point:

```bash
virtuoso probe --cmd 'bpftrace -e "kprobe:handle_mm_fault { @faults[comm]++ } \
  interval:s:2 { print(@faults); } interval:s:8 { exit(); }"'
```

Limitations: arm64 / x86_64 only (no official riscv64 assets); bpftrace
0.27's `count()` goes through a stdlib macro with an expansion defect — use
the equivalent `@[comm]++` form instead.
