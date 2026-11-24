# Kernel Configuration Requirements

Virtuoso boots the kernel under test in QEMU: an initramfs that must reach a
virtio root disk, an ext4 rootfs, a serial console, plus optional VM
components ([Components](../concepts/components.md)). Each of those steps
depends on specific kernel Kconfig symbols. This page is the reference for
**which switches Virtuoso needs, and how they drift across kernel versions
(4.19 → mainline)**.

> **TL;DR — ask the tool, not this page:**
> `virtuoso kernel check-config` evaluates the *active* kernel tree's
> `.config` against the requirement table (version-aware) and cross-checks
> the boot module manifest against the tree. This page is the narrative
> behind that table; the table itself lives in `src/builder/kconfig.rs` and
> is pinned to `infra/modules-boot.conf` by a unit test.

## Querying: `virtuoso kernel check-config`

```bash
virtuoso kernel check-config              # active kernel tree, toml arch
virtuoso kernel check-config --arch x86_64
```

The kernel tree is resolved exactly like build/test (env `KERNEL_PATH` >
active volume > toml `kernel_path` > default; see
[Kernel Provisioning](kernel-provisioning.md)). The report has three layers:

1. **Kconfig symbols** — every requirement, grouped Boot-critical /
   per-component. Component rows reflect the enablement state in
   `virtuoso.toml` (a gap in a disabled component is a `!` warning; in an
   enabled one, a `✗` failure).
2. **Version awareness** — symbols that do not exist in the tree's kernel
   version (e.g. `VIRTIO_PCI_LIB` before 5.15, `DEBUG_INFO_BTF` before 5.2)
   are reported as `– n/a` with guidance instead of false failures.
3. **Boot manifest vs tree** (built trees only) — every module of
   `infra/modules-boot.conf` (+ component `stage = "boot"` extras) is
   resolved exactly like the builder does: in-tree `.ko` → builtin
   (`modules.builtin`, skipped at build) → error with a version-specific
   hint.

Exit code: `0` = no gaps, `1` = a boot-critical or enabled-component symbol
is off, or a boot-manifest module is missing.

Manual spot-checks without the tool:

```bash
grep -E 'CONFIG_(VIRTIO|VIRTIO_PCI|VIRTIO_BLK|EXT4_FS|SERIAL_AMBA_PL011)' .config
./scripts/config --state VIRTIO_BLK        # y / m / n
./scripts/config -m VIRTIO_CONSOLE && make olddefconfig   # toggle + resolve
```

## Boot-Critical Requirements

These must hold for every boot, independent of components. **`=m` is fine
for everything in the boot set** — that is precisely what the two-stage boot
exists for ([Two-Stage Boot](../concepts/boot.md)): `=m` entries are packed
into the initramfs from `infra/modules-boot.conf`; `=y` entries are detected
via `modules.builtin` and skipped. `=y` is only mandatory for the bool
symbols (console, pseudo-filesystems, initrd support).

| Symbol | Module(s) when `=m` | Need | Why |
|---|---|---|---|
| `CONFIG_MODULES` | — | `=y` | insmod; every `=m` entry depends on it |
| `CONFIG_BLK_DEV_INITRD` | — | `=y` | initramfs support for `-initrd` |
| `CONFIG_RD_GZIP` | — | `=y` | initrd.img is a gzip-compressed newc archive |
| `CONFIG_VIRTIO` | `virtio.ko` + `virtio_ring.ko` | y/m | virtio bus core (one symbol builds both) |
| `CONFIG_VIRTIO_PCI` | `virtio_pci.ko` | y/m | QEMU attaches `if=virtio` disks as virtio-blk-pci on all three arches |
| `CONFIG_VIRTIO_PCI_LIB` | `virtio_pci_modern_dev.ko` | y/m | modern virtio-pci core, auto-selected by `VIRTIO_PCI`; **exists since 5.15** |
| `CONFIG_VIRTIO_PCI_LIB_LEGACY` | `virtio_pci_legacy_dev.ko` | y/m | legacy virtio-pci core, auto-selected by `VIRTIO_PCI_LEGACY` (default y); **exists since 5.16** |
| `CONFIG_VIRTIO_BLK` | `virtio_blk.ko` | y/m | rootfs.img (`/dev/vda`) + tools.img (`/dev/vdb`) |
| `CONFIG_PCI` + `CONFIG_PCI_HOST_GENERIC` | — | `=y` | PCIe ECAM host of the QEMU virt machine (arm64/riscv64; x86_64 always has PCI) |
| `CONFIG_EXT4_FS` | `ext4.ko` | y/m | rootfs.img filesystem |
| `CONFIG_JBD2` | `jbd2.ko` | y/m | ext4 journaling, selected by `EXT4_FS` |
| `CONFIG_FS_MBCACHE` | `mbcache.ko` | y/m | hidden tristate; defaults follow `EXT4_FS` |
| `CONFIG_CRC16` | `crc16.ko` | y/m | selected by `EXT4_FS`; openEuler forces `=y` (builtin — skipped at build) |
| `CONFIG_SERIAL_AMBA_PL011` + `_CONSOLE` | — | `=y` | **arm64** console `ttyAMA0` |
| `CONFIG_SERIAL_8250` + `_CONSOLE` | — | `=y` | **x86_64 / riscv64** console `ttyS0` |
| `CONFIG_DEVTMPFS` | — | `=y` | /dev population; pair with `DEVTMPFS_MOUNT=y` (the initramfs falls back to mknod + `mdev -s` otherwise — [boot quirks](../concepts/boot.md#4-known-kernel-quirks-and-required-workarounds-do-not-remove)) |
| `CONFIG_PROC_FS` / `CONFIG_SYSFS` / `CONFIG_TMPFS` | — | `=y` | mounted by both init stages |

No config is needed for **devpts**: it has been unconditional since 2.6.4
(`CONFIG_DEVPTS_FS` was removed then), so `mount -t devpts` always works on
4.19+.

## Component Requirements

Only needed when the component is enabled in `virtuoso.toml` (the check
command knows the enablement state). `=m` entries go into the component's
`require` list; order matters (dependencies first).

| Component | Symbol → module | Notes |
|---|---|---|
| `agent` (probe channel) | `CONFIG_VIRTIO_CONSOLE` → `virtio_console.ko` | `virtuoso probe` force-merges `virtio_console` regardless of the switch |
| `vfio` | `CONFIG_VFIO` → `vfio.ko`, `CONFIG_VFIO_PCI` → `vfio_pci.ko`, `CONFIG_VFIO_IOMMU_TYPE1` → `vfio_iommu_type1.ko` | **since 5.15** also `VFIO_PCI_CORE` → `vfio_pci_core.ko` (auto-selected) — see §version-drift |
| `pmem` | `CONFIG_LIBNVDIMM` → `libnvdimm.ko`, `CONFIG_BLK_DEV_PMEM` → `nd_pmem.ko`, `CONFIG_ND_BTT` → `nd_btt.ko`, `CONFIG_OF_PMEM` → `of_pmem.ko`, `CONFIG_FS_DAX` | the whole family exists across 4.19..mainline; `ND_BTT` is hidden, selected by `BLK_DEV_PMEM` when `BTT=y` (default); symbol name ≠ module name for `nd_pmem` |
| `bpf` | `CONFIG_BPF_SYSCALL`, `CONFIG_DEBUG_INFO_BTF`, `CONFIG_KPROBES`, `CONFIG_KPROBE_EVENTS`, `CONFIG_BPF_EVENTS`, `CONFIG_PERF_EVENTS`, `CONFIG_TRACING`, `CONFIG_DEBUG_FS` | **`DEBUG_INFO_BTF` exists since 5.2** — bpf observability effectively needs ≥ 5.2; tracefs has no own symbol (comes with `TRACING`) |
| `numa` | `CONFIG_NUMA` | x86_64 always; **arm64 since 4.7; riscv64 since 5.12** |
| hooks (init-hooks.sh recipes) | `CONFIG_HUGETLBFS` | hugetlbfs mount + `nr_hugepages` ([recipe](../concepts/boot.md#5-modification-playbook)); `HUGETLB_PAGE` auto-selected |

## Version Drift (4.19 → mainline)

Most requirements are stable across the whole range. The items that
actually changed:

### 1. The virtio-pci module split (the one that bites)

The single `virtio_pci.ko` was split into per-transport modules in two
steps, both **auto-selected** (no new user-visible switch beyond
`VIRTIO_PCI`):

| Kernel | `VIRTIO_PCI=m` builds | `infra/modules-boot.conf` action |
|---|---|---|
| 4.19 / 5.4 / 5.10 | `virtio_pci.ko` only | **remove** `virtio_pci_modern_dev` and `virtio_pci_legacy_dev` |
| 5.15 | + `virtio_pci_modern_dev.ko` (`VIRTIO_PCI_LIB`) | remove `virtio_pci_legacy_dev` only |
| ≥ 5.16 (incl. 6.1 / 6.6 / mainline) | + `virtio_pci_legacy_dev.ko` (`VIRTIO_PCI_LIB_LEGACY`) | frozen base set works verbatim |

On a pre-split kernel the frozen base set references `.ko` files that do not
exist, and the builder fails with `Module virtio_pci_modern_dev.ko not
found` (a missing `.ko` is a hard build error by design). Edit
`infra/modules-boot.conf` as above for old kernels; `check-config` prints
exactly this hint. openEuler note: OLK-5.10 tracks mainline 5.10 (single
module); openEuler 6.6 has both splits.

### 2. `VFIO_PCI_CORE` split (5.15)

Since 5.15, `vfio_pci.ko` depends on `vfio_pci_core.ko` (hidden
`VFIO_PCI_CORE`, auto-selected). The guest-side `require` for the vfio
component becomes:

```toml
# ≤ 5.10
require = ["vfio", "vfio_pci"]
# ≥ 5.15 (order matters — core before pci)
require = ["vfio", "vfio_pci_core", "vfio_pci", "vfio_iommu_type1"]
```

### 3. `DEBUG_INFO_BTF` (5.2)

Kernel BTF (`/sys/kernel/btf/vmlinux`) appeared in 5.2 and needs pahole
(dwarves) at build time. The **bpf** component (bpftrace/bpftool CO-RE) is
effectively unavailable on 4.19 — the symbol does not exist there.

### 4. NUMA floors per arch

`CONFIG_NUMA`: always on x86_64; added on **arm64 in 4.7**, on **riscv64 in
5.12** (DT route; ACPI NUMA came even later, 6.11). The `[components.numa]`
topology only becomes visible in the guest with it.

### 5. Stable across the whole range (no action needed)

- The ext4 select chain: `EXT4_FS` selects `JBD2` + `CRC16`;
  `FS_MBCACHE` (hidden) defaults follow `EXT4_FS` — identical since 4.9.
- `CRC16` value varies by distro, not by version: openEuler forces `=y`
  (builtin), mainline builds it `=m` when ext4 is `=m`. The builder handles
  both via `modules.builtin` (builtin entries are skipped, never insmodded).
- The pmem family (`LIBNVDIMM` / `BLK_DEV_PMEM` / `ND_BTT` / `OF_PMEM`) is
  present and shaped the same from 4.19 through mainline.
- `VIRTIO_CONSOLE`, `SERIAL_AMBA_PL011(_CONSOLE)`, `SERIAL_8250(_CONSOLE)`,
  `DEVTMPFS`, `BLK_DEV_INITRD`, `RD_GZIP`, `PCI_HOST_GENERIC` — unchanged.

### 6. defconfig postures (what you start from)

| Tree | Boot-set shape | Consequence |
|---|---|---|
| openEuler defconfig (4.19 / 5.10 / 6.6) | virtio + ext4 **=m** (`CRC16=y` builtin) | the two-stage boot is mandatory; this is the project's primary target |
| mainline `x86_64_defconfig` | `VIRTIO_BLK/VIRTIO_PCI/EXT4_FS/SERIAL_8250_CONSOLE` all **=y** | boots with an empty initramfs module set |
| mainline `defconfig` (arm64 / riscv64) | virtio + ext4 **=y** (riscv64 also `VIRTIO_MMIO=y`) | same — modules.builtin skip makes the manifest a no-op |

## Version Matrix

Anchors verified against the tagged Kconfig sources (4.19, 5.4, 5.10, 5.14,
5.15, 5.16, 5.19, 6.1, 6.6, master):

| Item | 4.19 | 5.4 | 5.10 | 5.15 | 5.16+ | 6.1 / 6.6 | mainline |
|---|---|---|---|---|---|---|---|
| `virtio_pci.ko` shape | single | single | single | +modern_dev | +legacy_dev | both | both |
| `VFIO_PCI_CORE` | – | – | – | split | split | split | split |
| `DEBUG_INFO_BTF` | – | – | – | ✓ (5.2+) | ✓ | ✓ | ✓ |
| `NUMA` (arm64) | ✓ (4.7+) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `NUMA` (riscv64) | – | – | – | – | ✓ (5.12+) | ✓ | ✓ |
| everything else on this page | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |

## How the Builder Consumes This

- `=m` boot modules are packed from `infra/modules-boot.conf` (+ component
  `stage = "boot"` extras); a missing `.ko` **fails the build** — fix the
  config (`=m` it) or drop the entry for old kernels.
- `=y` modules are detected via the tree's `modules.builtin` and skipped
  (never insmodded) — the same manifest is reusable across trees.
- Runtime component modules land in the rootfs `modules.conf`; see
  [Two-Stage Boot §3](../concepts/boot.md#3-the-dual-module-manifest-rule)
  for the full rule and [Components](../concepts/components.md) for the
  per-component `require` semantics.
- Prefer `=m` for modules under iteration (no kernel rebuild per change).
