# Quick Start

From zero to your first `verdict: passed`: install dependencies → install virtuoso →
health check → build the kernel under test → first run → triage.

## 1. Prerequisites

- Host tools: `rust` (stable), `zig` (compiler for C test cases), `qemu-system-*`
- A virtuoso checkout (this repository — `git clone` it anywhere; the harness is
  designed to live inside a kernel tree, e.g. `kernel/virtuoso/`, but works from any
  directory: the kernel location is resolved per the rules in
  [Configuration Reference](usage/configuration.md#kernel-tree-resolution-order))
- openEuler / Fedora: `sudo dnf install -y gcc make wget qemu-system-aarch64 qemu-img`; get zig from [ziglang.org/download](https://ziglang.org/download/) (distro repos generally do not carry it)
- Debian / Ubuntu: `sudo apt install -y gcc make zig wget qemu-system-arm qemu-utils`

> initrd packing is now natively implemented in Rust; `cpio` / `gzip` / `wget` are no longer hard
> dependencies (the download layer has a curl fallback) — installing them saves some effort,
> but doctor will not report ✗ if they are missing.

### macOS (Apple Silicon, M1–M5) Prerequisites

```bash
brew install qemu e2fsprogs dtc zig
brew install --cask orbstack   # docker engine: volumes must be host-visible (OrbStack view)
```

- `qemu`: `qemu-system-aarch64` with HVF acceleration (near-native on Apple Silicon);
- `e2fsprogs`: `mke2fs` (keg-only; virtuoso auto-detects the keg path, no PATH changes needed);
- `dtc`: dumpdtb/fdtput tools for the pmem component (optional if you do not use pmem);
- `zig`: cross-compilation of C test cases (unified compiler invocation: `zig cc -target <musl triple>`
  is identical across all three hosts and produces static Linux ELF; testcases have fully moved to cargo, no cmake needed);
- `OrbStack`: the docker engine for `virtuoso kernel` — Docker Desktop hides volumes inside
  its VM disk where the host cannot read them; doctor guards the engine endpoint. Only needed
  for the containerized kernel-provisioning path;
- On the Rust side you additionally need `rustup target add aarch64-unknown-linux-musl`.
- vfio passthrough is not supported on macOS (it architecturally depends on the Linux IOMMU); doctor will reject it outright.

## 2. Install and Health Check

Run from the virtuoso checkout root (the macOS kernel-provisioning commands in the
next step need the binary in place, so install comes first):

```bash
cargo install --path .      # installs the canonical virtuoso binary into PATH (once)
virtuoso doctor               # one-screen health check: ✓/✗/! component lines, fastest way to confirm the wiring
```

When `doctor` reports ✗, add `--verbose` to see the full checklist and typed-configuration
diagnostics (same checking engine, differing only in how much is presented; also takes `--arch`).

## 3. Build the Kernel Under Test

**Linux (native)** — any kernel tree with "Makefile + arch/&lt;a&gt;/boot/&lt;Image&gt; + *.ko":

```bash
git clone https://gitcode.com/openeuler/kernel.git && cd kernel
cp arch/arm64/configs/openeuler_defconfig .config   # or your own .config
make -j"$(nproc)"
make modules -j"$(nproc)"        # as long as any module is =m
```

The harness defaults to living inside the kernel tree (auto-detected); if you place it elsewhere,
set `kernel_path` in `virtuoso.toml`.

**macOS (containerized)** — macOS cannot build a Linux kernel natively, so use
`virtuoso kernel` (toolchain pinned in a container): the source goes into a named volume
(ext4 on the container side, sidestepping the APFS case-insensitivity quirk; on Linux the
same commands work, with the volume itself living on the host filesystem):

```bash
virtuoso kernel clone https://gitcode.com/openeuler/kernel.git --ref OLK-6.6
virtuoso kernel defconfig openeuler_defconfig
virtuoso kernel build          # Image + modules + static bpftool + compile_commands.json
virtuoso kernel path           # → host-visible path of the volume (consumed by QEMU)
```

No `kernel_path` editing needed: the active volume written by `clone`/`use` is picked up
automatically by doctor/build/test (the toml key only takes effect in raw mode, i.e. without
an active volume). For viewing/editing source, use the VS Code devcontainer (`kernel use/clone`
automatically renders a git-ignored `.devcontainer/devcontainer.json` at the repo root;
"Reopen in Container" on the repo drops you into /ksrc of the current volume). See
[Kernel Provisioning (forge)](usage/kernel-provisioning.md) for the full manual.

The kernel tree can also be built on any Linux machine and rsynced over — virtuoso only requires
`KERNEL_PATH` to point at a tree with "Makefile + arch/<a>/boot/<Image> + \*.ko".

## 4. First Run and Verdict

```bash
virtuoso test --timeout 60    # build → launch → judge → artifacts persisted to disk (prints the verdict line at the end)
```

Accelerator semantics follow the host platform: **on Linux, always TCG** (`shell --kvm` enables
KVM for interactive use); **on macOS, HVF is the default when host and target match (arm64)**,
`--tcg` forces pure emulation, and cross guests (x86_64 / riscv64) automatically fall back to
TCG (slower; enlarge the timeout budget accordingly).

**The verdict line printed at the end of `test` is the authority** (the only machine-readable
surface = `verdict.json` in the run directory, `target/runs/<unix_ms>-<arch>/`): only
`verdict: passed` counts as a pass. The exit code is merely an interface contract
(0=pass, 124=timeout, 130=Ctrl-C, anything else=failure) — under
`-no-reboot`, a kernel panic makes QEMU exit with code 0, so judging by exit code alone produces
a false pass; `exit 0` with `verdict: incomplete` = the marker protocol did not run to
completion, which is likewise treated as a failure.

For the full set of eight verdict states and their semantics, see
[Run Artifacts and Triage](usage/artifacts.md).

## 5. Next Steps

- Day-to-day operations and every command: [CLI Reference](usage/cli.md)
- Changing configuration / switching architecture / enabling components: [Configuration Reference](usage/configuration.md)
- Kernel provisioning in containers (volumes, devcontainer, clangd): [Kernel Provisioning (forge)](usage/kernel-provisioning.md)
- Writing your own test cases: [Writing Test Cases](usage/writing-tests.md)
- When something goes wrong: [Troubleshooting](usage/troubleshooting.md)
