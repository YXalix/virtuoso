# Quick Start

From zero to your first `verdict: passed`: install dependencies → build the kernel under test → install virtuoso →
health check → first run → triage.

## 1. Prerequisites

- Host tools: `rust` (stable), `zig` (compiler for C test cases), `make`, `qemu-system-*`
- A kernel source tree (the harness is designed to run inside a kernel tree, e.g. `kernel/virtuoso/`)
- openEuler / Fedora: `sudo dnf install -y gcc make wget cpio gzip qemu-system-aarch64 qemu-img`; get zig from [ziglang.org/download](https://ziglang.org/download/) (distro repos generally do not carry it)
- Debian / Ubuntu: `sudo apt install -y gcc make zig wget cpio gzip qemu-system-arm qemu-utils`

> initrd packing is now natively implemented in Rust; `cpio` / `gzip` / `wget` are no longer hard
> dependencies (the download layer has a curl fallback) — installing them saves some effort,
> but doctor will not report ✗ if they are missing.

### macOS (Apple Silicon, M1–M5) Prerequisites

```bash
brew install qemu e2fsprogs dtc zig
```

- `qemu`: `qemu-system-aarch64` with HVF acceleration (near-native on Apple Silicon);
- `e2fsprogs`: `mke2fs` (keg-only; virtuoso auto-detects the keg path, no PATH changes needed);
- `dtc`: dumpdtb/fdtput tools for the pmem component (optional if you do not use pmem);
- `zig`: cross-compilation of C test cases (unified compiler invocation: `zig cc -target <musl triple>`
  is identical across all three hosts and produces static Linux ELF; testcases have fully moved to cargo, no cmake needed);
- On the Rust side you additionally need `rustup target add aarch64-unknown-linux-musl`.
- vfio passthrough is not supported on macOS (it architecturally depends on the Linux IOMMU); doctor will reject it outright.

## 2. Build the Kernel Under Test

```bash
git clone https://gitcode.com/openeuler/kernel.git && cd kernel
cp arch/arm64/configs/openeuler_defconfig .config   # or your own .config
make -j"$(nproc)"
make modules -j"$(nproc)"        # as long as any module is =m
```

The harness defaults to living inside the kernel tree (auto-detected); if you place it elsewhere,
set `kernel_path` in `virtuoso.toml`.

### Kernel Provisioning on macOS (Containerized Kernel Development Environment)

macOS cannot build a Linux kernel natively, so use `virtuoso kernel` (toolchain pinned in a container):
the source goes into a named volume (ext4 on the container side, sidestepping the APFS
case-insensitivity quirk; on Linux the same commands work, with the volume itself living on the
host filesystem):

```bash
virtuoso kernel clone https://gitcode.com/openeuler/kernel.git --ref OLK-6.6
virtuoso kernel defconfig openeuler_defconfig
virtuoso kernel build          # Image + modules + static bpftool + compile_commands.json
virtuoso kernel path           # → host-visible path of the volume (consumed by QEMU)
```

In `virtuoso.toml`, point `kernel_path` at **the output of `virtuoso kernel path`** (a pure host
path; virtuoso reads the Image and .ko files directly) and the host-side main loop is ready to run.
For viewing/editing source, use the VS Code devcontainer (`kernel use/clone` automatically renders
a git-ignored `.devcontainer/devcontainer.json` at the repo root; "Reopen in Container" on the
repo drops you into /ksrc of the current volume; clangd inside the container consumes the CDB
produced by build; builds go through `virtuoso kernel build`; remote servers follow the same flow
via Remote-SSH).
To switch between kernels see `virtuoso kernel list` / `use` (the current state file is
`.virtuoso/kernel-current.json`; the devcontainer follows `use` automatically).
See [devkit/docker/README.md](../devkit/docker/README.md) for details.

The kernel tree can also be built on any Linux machine and rsynced over — virtuoso only requires
`KERNEL_PATH` to point at a tree with "Makefile + arch/<a>/boot/<Image> + \*.ko".

## 3. Install and Health Check

```bash
cargo install --path .      # installs the canonical virtuoso binary into PATH (once)
virtuoso doctor               # one-screen health check: ✓/✗/! component lines, fastest way to confirm the wiring
```

When `doctor` reports ✗, add `--verbose` to see the full checklist and typed-configuration
diagnostics (same checking engine, differing only in how much is presented; also takes `--arch`).

## 4. First Run and Verdict

```bash
virtuoso test --timeout 60    # build → launch → judge → artifacts persisted to disk (prints the verdict line at the end)
```

Accelerator semantics follow the host platform: **on Linux, always TCG** (`shell --kvm` enables
KVM for interactive use); **on macOS, HVF is the default when host and target match (arm64)**,
`--tcg` forces pure emulation, and cross guests (x86_64 / riscv64) automatically fall back to
TCG (slower; enlarge the timeout budget accordingly).

**The verdict line printed at the end of `test` is the authority** (the only machine-readable
surface = `verdict.json` in the run directory): only `verdict: passed` counts as a pass. The
exit code is merely an interface contract (0=pass, 124=timeout, anything else=failure) — under
`-no-reboot`, a kernel panic makes QEMU exit with code 0, so judging by exit code alone produces
a false pass; `exit 0` with `verdict: incomplete` = the marker protocol did not run to
completion, which is likewise treated as a failure.

For the full set of eight verdict states and their semantics, see
[Run Artifacts and Triage](usage/artifacts.md).

## 5. Next Steps

- Day-to-day operations and every command: [CLI Reference](usage/cli.md)
- Changing configuration / switching architecture / enabling components: [Configuration Reference](usage/configuration.md)
- Writing your own test cases: [Writing Test Cases](usage/writing-tests.md)
- When something goes wrong: [Troubleshooting](usage/troubleshooting.md)
