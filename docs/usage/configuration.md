# Configuration Reference

`virtuoso.toml` is the single configuration surface. The template at the repo root is the
default regular-launch configuration; every optional capability is present as a comment —
uncomment to enable.

- **Priority**: for same-named scalar keys, **process environment variables** win (env
  overrides toml — CI / one-off command-line tweaks never touch the file); persistent
  configuration is written only to toml.
- **Strict parsing**: unknown keys / invalid types error out at parse time; nothing is
  silently ignored.
- Diagnostic presentation: `virtuoso doctor` (one screen) or `virtuoso doctor --verbose`
  (full checklist + typed configuration diagnostics).

## Global Keys

| Key | Env Var | Description |
|---|---|---|
| `arch` | `ARCH` | `x86_64` \| `arm64` \| `riscv64` (default = host architecture) |
| `timeout_secs` | `QEMU_TIMEOUT` | Wall-clock timeout in seconds; 0 is always rejected |
| `smp` | `SMP` | Total vCPU count; with multi-node NUMA it must be divisible by the node count (parse-time error, re-checked at launch) |
| `auto_test` | `AUTO_TEST` | true = power off automatically after `/tests/` finishes; false = drop into an interactive shell |
| `kernel_path` | `KERNEL_PATH` | Kernel tree path (auto-detected when the harness sits inside the tree; **overridden by the active volume in docker mode**, effective only in raw mode — see the resolution order in the next section) |
| `kernel_image` | `KERNEL_IMAGE` | Kernel image override (default = the arch-appropriate image inside the kernel tree) |
| `qemu` | `QEMU` | QEMU binary override (`QEMU=echo` prints the argv for comparison) |
| `cmdline_extra` | `CMDLINE_EXTRA` | Extra kernel cmdline fragments: the whole string is appended verbatim after the machine-assembled baseline (empty default = baseline unchanged). Kernel parameters are last-wins, so appending overrides (e.g. `nokaslr initcall_debug loglevel=4`; `console=` is the exception: it registers a second console, and the later one becomes preferred) |
| `qemu_opts` | `QEMU_OPTS` | Catch-all pass-through argument array (env is split on whitespace; vfio component devices are appended separately at the assembly point and are never mixed into this chain) |

## Environment Variables for docker Kernel Provisioning (No toml Keys)

The `virtuoso kernel` command group ([Kernel Provisioning (forge)](kernel-provisioning.md))
occupies no `virtuoso.toml` keys — it goes purely through environment variables (priority as
above: env > state file > built-in default):

| Env Var | Description |
|---|---|
| `KERNEL_VOLUME` | Active volume override (takes precedence over the current state file `.virtuoso/kernel-current.json`) |
| `KERNEL_ARCH` | Target architecture override (default = top-level `arch`) |
| `KERNEL_REF` | Default ref for `kernel clone` (default master) |
| `KERNEL_TOOLCHAIN_IMAGE` | Toolchain image override (default ghcr.io/yxalix/virtuoso-kernel:latest; unrelated to `KERNEL_IMAGE`) |

### Kernel Tree Resolution Order

Which tree build/test/doctor consume — the single rule:

```
env KERNEL_PATH > current active volume (KERNEL_VOLUME env > state file) > toml kernel_path > default (one level above the project root)
```

The active volume written by `kernel use/clone` takes precedence over toml — switching volumes
switches the test target, with no need to hand-edit
`kernel_path`; env overrides are always the highest. The `kernel_path` key is only effective
in raw mode (no active volume).

## Env-Only Variables (No toml Key)

| Env Var | Description |
|---|---|
| `NUMA_NODES` / `NUMA_MEMORY` | Override `[components.numa]` `nodes` / `memory_per_node` with the usual env > toml priority — effective whether or not the component section is enabled (legacy path predating the component; the component switch itself still gates the argv) |
| `BUSYBOX_APPLETS_FILE` | Explicit BusyBox applet manifest (`busybox --list` output) for a custom BusyBox config; default = `infra/busybox/applets-<version>.txt` frozen in the repo |

## The `[components.*]` Component Sections

The common fields of every component section (`enabled` / `require` / `stage`) and the
per-component specific keys are described in
[Component Mechanics](../concepts/components.md).

## The `[tests]` Section

| Key | Description |
|---|---|
| `require` | Kernel module dependencies of the test-case suite (`"<module> [key=val ...]"`, always the runtime stage; component entries for same-named modules take precedence, only the difference is filled). The natural home for declaring the module under test (`=m`) when writing test cases for a kernel feature |

## The `[busybox]` Section

| Key | Env Var | Description |
|---|---|---|
| `version` | `BUSYBOX_VERSION` | BusyBox version (e.g. `"1.36.1"`) |
| `release_repo` | `BUSYBOX_RELEASE_REPO` | Prebuilt Release repository (by default scans git remotes for github.com) |
| `dl_url` | `BUSYBOX_DL_URL` | Explicit download URL (highest priority in the supply chain) |

Supply chain = fetch from a GitHub Release + local cache reuse, with no source-build fallback
(everything missing is an error).
See [Provisioning and Build Pipeline](../concepts/pipeline.md) for details.

## Architecture Matrix

| arch | QEMU Binary | Kernel Image | Console | Machine |
|---|---|---|---|---|
| `arm64` (default) | `qemu-system-aarch64` | `arch/arm64/boot/Image` | `ttyAMA0` | `virt` |
| `x86_64` | `qemu-system-x86_64` | `arch/x86/boot/bzImage` | `ttyS0` | `q35` |
| `riscv64` | `qemu-system-riscv64` | `arch/riscv/boot/Image` | `ttyS0` | `virt` |

Any cross combination works (e.g. an arm64 host running `--arch x86_64`); when target and host
architectures differ, doctor warns and launch automatically falls back to TCG.
