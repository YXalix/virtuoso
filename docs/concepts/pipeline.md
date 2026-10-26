# Provisioning and Build Pipeline

`virtuoso build` assembles "kernel tree + test cases + component configuration"
into three images. This chapter covers the supply chain and pipeline concepts,
the shape of the artifacts, and the invariants; for how the images are consumed
inside the VM, see [Two-Stage Boot](boot.md).

## The Three Images

| Image | Contents | Lifecycle |
|---|---|---|
| `initrd.img` | busybox + **boot modules** + pivot init (cpio.gz) | memory freed after switch_root |
| `rootfs.img` | busybox + test init + `/tests/*` + test modules (ext4) | persistent; loop mount and modify freely |
| `tools.img` | resident tools from `infra/tools/` (musl static, ext4 volume label `tools`) | persistent; **not produced** when the tool provisioning is missing — explicit degradation, never masking |

Build inputs: `kernel_path` (the kernel tree, where `.ko` files are found),
`arch`, the `infra/` assets, and the require union of the components from
`virtuoso.toml`. Artifacts land in `target/artifacts/`; the BusyBox cache and
staging directories live in `target/build/`.

## The BusyBox Supply Chain

Static BusyBox is uniformly prebuilt and published by the `busybox-release`
workflow; no source builds happen locally.

### GitHub Release (Generic Artifacts, Deliberately Stable)

Tag `busybox-v<version>`, two assets per architecture:

| Asset | Contents |
|---|---|
| `busybox-<ver>-linux-<arch>` | bare static busybox binary (defconfig + `CONFIG_STATIC=y`) |
| `rootfs-<ver>-linux-<arch>.cpio.gz` | generic busybox rootfs directory tree (as cpio.gz) |

Stability invariants:

- The release rootfs `/init` is a **10-line script frozen into the workflow**
  (assembled via printf) and references no repository scripts — changing any
  `.sh`/`init*` in the repo does **not** require re-publishing the release;
  asset contents only change with the busybox version.
- The release contains **no** test content (no `/tests`, no test init, no
  module manifest). Tests are this repository's scenario injection, done by
  `virtuoso build`.
- The rootfs directory tree: `bin/` (busybox + symlinks for all applets) +
  `sbin usr/{bin,sbin}` + empty skeleton directories + `/etc/passwd`,
  `/etc/group` (root).
- Applet symlinks are manifest-driven (`infra/busybox/applets-<version>.txt`,
  the manifest being a frozen copy of the x86_64 `busybox --list` output) and
  **no guest binaries are executed** — cross-assembly (including on a macOS
  host) needs no matching host architecture; custom versions/configs go
  through `$BUSYBOX_APPLETS_FILE`.

Re-publishing: trigger `workflow_dispatch` manually from the Actions page
(default 1.36.1), or push a `busybox-v*` tag. Publishing is idempotent:
same-named assets are deleted then re-uploaded, and an existing release is
reused.

### Local Fetch (Four Levels, No Fallback)

`virtuoso build --busybox-only` tries in order and caches the first hit to
`target/build/busybox/bin/` (the version is part of the cache key; changing
versions triggers a re-fetch):

```text
0. local cache  →  1. $BUSYBOX_DL_URL  →  2. gh release download (authenticated; works for private repos)  →  3. direct download (ELF magic check)
```

If all levels miss, it errors out — **there is no source-build fallback**
(release repo resolution: `$BUSYBOX_RELEASE_REPO` > scanning git remotes for
one on github.com). In an offline environment, place the binary for the
matching architecture into the cache directory ahead of time.

### The ext4 Sizing Lesson

Never pick a fixed size — ext4 metadata + journal will genuinely fill the
entire size declared at creation time (there was once a lesson learned with a
64MB hole). Image size = measured content + headroom, computed automatically
by the build.

## Assembly Pipeline (Conceptual)

```text
1. BusyBox provisioning → per-arch static busybox (see the four-level fetch above)
2. busybox tree         → binary + manifest-driven applet symlinks + skeleton dirs + passwd/group
3. initramfs tree       → busybox tree + pivot init + boot module set
                          → cpio newc + gzip (mtime always 0 + sorted paths → reproducible, deterministic artifacts)
4. rootfs tree          → busybox tree + test init + test module manifest + /tests/* (musl static, with test-group subdirectories)
                          + init-hooks.sh + rootfs.d merged incrementally (additive-only, never overwrites)
                          → ext4 (auto-sized)
5. tools.img tree       → musl static artifacts of the tools workspace → ext4 (volume label tools)
```

- **Test cases (C over Rust)**: `infra/testcases/` is a standalone workspace;
  coda (std Rust) serves as the framework and the uniform entry point, and the
  C test bodies are compiled by each case crate's build.rs into the same musl
  static binary. Cross-compilation uniformly goes through `zig cc` (host
  differences are flattened). See
  [Writing Test Cases](../usage/writing-tests.md).
- **The `init-hooks.sh` injection point**: the builder generates
  `/init-hooks.sh` inside the rootfs, and the init side sources it behind a
  guard (positioned after devtmpfs is mounted, before insmod/agent startup) —
  tools disk mounting + PATH injection go through this channel. For in-VM
  customization, just write a hook snippet (example in
  [the modification playbook of Two-Stage Boot](boot.md#5-modification-playbook)).
- **The check engine**: build preconditions (toolchain / kernel image / QEMU /
  modules / BusyBox cache / component state) are carried by a typed check
  engine; `doctor` and the build share the same engine — adding a check only
  touches the engine, and the presentation on both sides follows
  automatically.

## File Index

| File | Role |
|---|---|
| `infra/init` / `infra/init-initramfs` | two-phase PID 1 (see [Two-Stage Boot](boot.md)) |
| `infra/modules-boot.conf` | frozen base module set for boot → initramfs |
| `infra/testcases/`, `infra/tools/` | test case and tools workspaces |
| `rootfs.d/` | user drop-ins (merged incrementally into the rootfs, additive-only; git-ignored; may be absent by default) |
| `target/artifacts/*.img` | build artifacts (git-ignored) |
| `.github/workflows/busybox-release.yml` | release generation (including the frozen init) |
