# Kernel Provisioning (forge)

Containerized kernel development environment — the manual of the `virtuoso kernel`
command group (assets live in `devkit/docker/`; this page is the documentation source
of truth).

**The container only does what the host cannot do, or cannot do conveniently**:
pin the kernel build toolchain and build on the case-sensitive container-side
ext4. The authoritative kernel source lives in a **named volume**; source
editing always goes through the VS Code devcontainer (clangd in-container);
the host reads the volume through a platform view only to consume build
artifacts (QEMU's kernel_path). Test asset builds (busybox / test-case
workspace) and QEMU runs always stay native on the host (macOS uses HVF,
Linux uses KVM — neither is available inside the container).

Both platforms share the same commands; the only branch point
is the host-visible path of the volume:

| | macOS (OrbStack) | Linux (docker-ce) |
|---|---|---|
| Volume host path | `~/OrbStack/docker/volumes/<volume>` (OrbStack view, read-write) | `/var/lib/docker/volumes/<volume>/_data` (the volume body; rootless lives under `$HOME`, no root needed) |
| `virtuoso kernel path` output | Same as above | Same as above (takes the engine's authoritative Mountpoint) |
| QEMU acceleration | HVF | KVM |

Docker Desktop hides volumes inside the VM's virtual disk, invisible to the
host — on macOS use OrbStack (doctor guards the engine endpoint).

| File | Purpose |
|---|---|
| `Dockerfile.kernel` | Pinned toolchain: ubuntu:24.04 + kernel build dependencies + three-architecture gcc + clangd + the Rust for Linux toolchain (rustc/bindgen pinned to the mainline tree's `scripts/min-tool-version.sh` floor; rust-src/clippy/rustfmt all included) |
| `devcontainer.json` | Static template (default volume form); `kernel use/clone` additionally renders the git-ignored `.devcontainer/devcontainer.json` (repo root) from the current volume — VS Code "Reopen in Container" → the in-container editing path with the volume as workspace (the standard entry point for source editing) |
| `.clangd` | clangd config template (single source of truth): rendered per target architecture and written into the source root by `kernel clone` |

## Why the Source Lives in a Named Volume

1. **Case sensitivity**: Linux kernel builds require a case-sensitive
   filesystem; macOS APFS is case-insensitive by default. The volume is
   container-side ext4, correct by construction.
2. **Performance**: volume I/O goes through the container's native
   filesystem, with build speeds close to native Linux (bind-mounts go through
   virtiofs and are 2–5x slower — don't use them).
3. **Multi-kernel switching**: each volume is self-contained with source +
   `.config` + incremental artifacts; switching = switching the volume name,
   and switching back to an old kernel needs no rebuild. Volume creation cost
   is near zero (clone defaults to `--depth 1`, fetching only the tree; add
   `--full` when following mainline development or when you need full
   `git log/blame` history — downloads ~3 GB more).

## Standard Workflow (First Run)

```bash
# 0) Dependencies: brew install qemu e2fsprogs dtc zig + OrbStack (macOS)
#    Linux: distro docker + qemu
# 1) Kernel source into a volume (+ .clangd rendered per arch + current written)
virtuoso kernel clone https://gitcode.com/openeuler/kernel.git --ref OLK-6.6

# 2) Configure + build (arm64 container = native frontend, the rest cross-compiled)
virtuoso kernel defconfig openeuler_defconfig
virtuoso kernel build              # Image/bzImage + modules + compile_commands.json (/ksrc form)

# 3) The kernel tree is host-visible: the directory `virtuoso kernel path` prints is it
virtuoso kernel path
```

The kernel tree's closure with build/test/doctor is **automatic**: the active
volume that `kernel use/clone` writes as current is the kernel source tree
(resolution order = env KERNEL_PATH > current active volume > toml
kernel_path > default). Both the OrbStack view and the Linux volume body are
pure host paths, and virtuoso reads the Image and `.ko` files directly.
Switching volumes switches the test target — no manual `kernel_path` edits
needed (that key only takes effect in raw mode, i.e. when there is no active
volume). Run the host main loop:

```bash
virtuoso doctor && virtuoso build && virtuoso test
```

## Volume Management and Multi-Kernel Switching

```bash
virtuoso kernel clone <url> --ref OLK-6.6 --as ksrc-openEuler-6.6
virtuoso kernel clone <url> --ref master      --as ksrc-mainline --full

virtuoso kernel list        # all volumes + content state (empty/cloned/configured) + current marker
virtuoso kernel use ksrc-mainline   # switch current (writes .virtuoso/kernel-current.json)
```

The `current` pointer is a dedicated session state file,
`.virtuoso/kernel-current.json` (git-ignored): written by `use`/`clone`,
after which all `virtuoso kernel` subcommands act on it (the `KERNEL_VOLUME`
env var can temporarily override it; semantics match the other keys:
env > state file > default).

## AI Loop (One AI, One Workspace)

Source viewing/editing happens in the VS Code devcontainer (the workspace is
/ksrc); the build is just an ordinary shell command to the AI
(`virtuoso kernel build`), and verification happens on the host:

```bash
virtuoso kernel build                           # container make (native volume I/O)
virtuoso doctor && virtuoso build && virtuoso test
```

The authoritative verdict is the verdict line printed at the end of `test`
(the only machine-readable surface is `verdict.json` in the run directory;
only `verdict: passed` counts as passing). With `virtuoso skill install`
(KERNEL_PATH pointing at the host-visible path) injecting the kernel-dev and
kernel-virtuoso skills, the AI has the knowledge to drive the test loop,
triage failures, and write test cases.

## clangd / Browsing the Kernel Source

Source browsing/editing always goes through the **VS Code devcontainer**.
When writing current, `kernel use/clone` automatically renders the git-ignored
`.devcontainer/devcontainer.json` (repo root): workspaceMount points at the
current volume and the image is the ghcr pinned-toolchain image directly —
no local build of `Dockerfile.kernel` on first entry
(`KERNEL_TOOLCHAIN_IMAGE` is substituted at render time):

**Open the repo root → Command Palette "Dev Containers: Reopen in
Container"** (VS Code auto-discovers `.devcontainer/`; no need to manually
pick a hidden directory). The workspace is `/ksrc` (the current volume).
Switching volumes = `virtuoso kernel use <volume>` then Reopen/Rebuild
Container. The static template in `devkit/docker/` mounts the default volume
`virtuoso-kernel` and is only usable when no `--as` name was given. The
location must be named `.devcontainer/` — VS Code's auto-discovery contract
only scans `.devcontainer/` under the opened workspace (or a root-level
devcontainer.json); placed in `.virtuoso/`, manually picking the hidden
directory is the only option left.

clangd runs inside the container and consumes the raw `/ksrc` form of
compile_commands.json produced automatically at the end of `build`, plus
the `.clangd` at the source root (rendered per architecture at clone time) —
jump/completion/hover work out of the box with no rewriting step. If you
changed `.config` or the index is stale, rerun `virtuoso kernel build` (or
`python3 scripts/clang-tools/gen_compile_commands.py`) in the container's
integrated terminal.

Note that a container only persists while you explicitly have the devcontainer
open: commands like `virtuoso kernel build` run one-shot containers (`--rm`)
that exit when done — their absence from the "Attach to Running Container"
list is normal; enter the container via Reopen in Container above. The VS Code
server and extensions are automatically persisted by the extension in the
named volume `vscode` (mounted at `/vscode`) — reopening/rebuilding the
container with the same VS Code version does not reinstall them; do not
delete that volume manually. The "Downloading VS Code Server" message in the
logs on every window open is usually routine placement: the tarball goes
through the host cache (`serverCache`); a real re-download only happens after
a VS Code upgrade (commit change).

**Remote servers (Remote-SSH) use the same flow**: first run
`virtuoso kernel clone/use` on the server (rendered artifacts land in the
server-side repo), connect with local VS Code Remote-SSH → open the repo
root → on the first "Reopen in Container" the Dev Containers extension is
installed into the remote automatically → the container runs on the server
(Linux branch: volume body + KVM available on the same machine). The command
surface is identical between local macOS and remote Linux; the only
difference is which machine hosts the docker engine.

**Discipline for two containers sharing a volume**: the build container
(one-shot) and the devcontainer (long-running) may mount the same volume at
the same time, non-exclusively — but **two parallel `make` runs on the same
tree are never allowed** (the incremental state
`.*.cmd`/`.o`/`Module.symvers` is unlocked and will genuinely corrupt). You
may edit source during a build, but don't trust the build result; rerun
incrementally after editing. The AI/editor edits code directly on the host
view (ext4 passthrough, case fidelity), and the output of
`virtuoso kernel path` is exactly that path.

## Image Publishing

`kernel-builder.yml` builds and publishes `ghcr.io/yxalix/virtuoso-kernel`
(linux/arm64 + linux/amd64) when changes under `devkit/docker/` are pushed to
main. When `virtuoso kernel` cannot pull the image it automatically falls
back to a local build of `Dockerfile.kernel`; the image can be overridden
with `KERNEL_TOOLCHAIN_IMAGE` (`KERNEL_IMAGE` is unrelated — that is the
config key for the kernel image virtuoso boots).

## Changing the Target Architecture

The architecture defaults to the top-level `arch` in `virtuoso.toml`
(build what you test); the `KERNEL_ARCH` env var or `--arch` overrides it
temporarily:

```bash
virtuoso kernel clone <url> --arch riscv64
virtuoso kernel build --arch riscv64
```
