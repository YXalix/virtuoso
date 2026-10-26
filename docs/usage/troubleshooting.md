# Troubleshooting — Symptom → Solution

A symptom-to-solution quick reference shared by AI and humans. Start from the verdict line
printed at the end of the latest `virtuoso test` and
`target/runs/<id>/verdict.json` (the only machine-readable verdict surface); the per-event facts are in
`events.jsonl`, and panic lines plus the raw serial text are in `serial.log`. Each symptom below
assumes you have already looked at the artifacts in the corresponding run directory.
For debugging tools (shell / GDB / probe), see
[Debugging](debugging.md).

## Timeout (exit 124, verdict: timeout)

**Symptom**: `Test timed out after N seconds`, with the serial console stalled somewhere.
**Solution**:
1. Look at the tail of `serial.log`: whatever print it stopped after is the suspect.
2. Is the `panics` field of `verdict.json` non-empty? → go to the "panic" entry (the form where a panic is masked by a timeout).
3. Need live debugging: `virtuoso shell --gdb` + `gdb-multiarch vmlinux -ex 'target remote :1234'`,
   send breakpoints once suspended and inspect `bt`; or use `virtuoso shell` to run `/tests/*` by hand to reproduce.

## panic (verdict: panic)

**Symptom**: the `panics` array is non-empty, e.g. `Kernel panic - not syncing: ...`.
**Solution**:
1. Note that exit may be 0 or 124: under `-no-reboot`, a panic triggers a reset and QEMU exits directly (0);
   or it hangs until the timeout (124). **Do not use the exit code to decide whether a panic happened.**
2. Decode the stack: in the kernel tree, `./scripts/decode_stacktrace.sh vmlinux < serial.log`.
3. Did the panic happen right after a certain test ran? The last `test_start` in events.jsonl is the suspect.

## exit 0 but verdict: incomplete (false pass)

**Symptom**: QEMU exits with code 0, but `marker_complete` is null (`TEST_COMPLETE` was never seen).
**Solution**: The boot flow exited before completing — a panic reset (see above), an init crash,
or the initramfs emergency shell exiting after a rootfs mount failure. Search `serial.log` for
`Kernel panic` or `emergency`;
this form is treated as a failure and **must not** be counted as a pass.

## verdict: failed (FAILED tests present)

**Solution**: The `tests` section of `verdict.json` lists each test's status and assertion counts. Look just above the `[FAIL]` lines in `serial.log`;
to reproduce, enter the VM with `virtuoso shell` and run `/tests/test_x` by hand (all cases remain in the rootfs).

## insmod Failure / Missing Modules

**Symptom**: `serial.log` has `insmod: can't insert 'xxx.ko'`, or the build reports
`Module xxx.ko not found`, or the doctor stage reports Kernel modules missing.
**Solution**:
- The rootfs `modules.conf` is generated from the require union of enabled components (`virtuoso.toml`
  `[components.*]`) — to add a module, change a component's `require`; never hand-write the conf. Entry order is the
  insmod order, with dependees first.
- `modules-boot.conf` (initramfs; critical for the virtio+ext4 boot) remains a hand-written frozen base set;
  when a component needs early-boot loading, append it with `stage = "boot"`.
- After a kernel rebuild the `.ko` paths may change: `make modules INSTALL_MOD_PATH=...` or check
  the module paths reported by `virtuoso doctor --verbose`.

## Test Binary Does Not Appear in /tests/

**Symptom**: `Test Results: 0/0 passed`, or no `--- Running:` lines at all.
**Solution**: Is the case crate directory named `test-<name>` (members use the `test-*`
glob — zero wiring, no workspace files to edit); confirm static linking (the musl target is
static by default; switching to dynamic is forbidden); rebuild with
`virtuoso clean && virtuoso build`;
check the compile warnings in `target/runs/<id>/build.log`.

## kernel image not found / QEMU not found

**Solution**: The precondition diagnostics of `virtuoso doctor --verbose` will name the MISSING items and suggest fixes
(`KERNEL_PATH`, `ARCH`, `QEMU=` overrides). For cross-architecture runs, confirm the matching
`qemu-system-<arch>` is installed.

## SMP not divisible by NUMA_NODES

**Solution**: The launcher validation refuses to launch (SMP must be divisible by the node count). Adjust
`smp` in `virtuoso.toml` or `nodes` in `[components.numa]`.

## Viewing Logs Already Emitted After a Ctrl-C Interrupt

**Solution**: The serial console is persisted to disk line by line in sync: read `serial.log`
under the latest id in `target/runs/` directly. A missing verdict.json means the run did not
finish properly (e.g. interrupted by Ctrl-C) — treat it as a failure.

## BusyBox Download Failure

**Solution**: `virtuoso build --busybox-only` walks the four-level supply chain (local cache →
explicit URL → gh release download → direct download), with **no source-build fallback** —
everything missing is an error. In offline environments, place the busybox for the target
architecture into `target/build/busybox/bin/` in advance;
release assets are validated by ELF magic, and a corrupt one counts as a miss and moves to the
next level. Applet symlinks are driven by the manifest
(`infra/busybox/applets-<version>.txt`); supply `BUSYBOX_APPLETS_FILE` when using a
custom busybox version/configuration. See
[Provisioning and Build Pipeline](../concepts/pipeline.md) for details.

## macOS-Specific

**Symptom**: `Host tools: missing -mke2fs` (doctor ✗)
**Solution**: `brew install e2fsprogs`. It being keg-only and absent from PATH does not matter —
virtuoso auto-detects `/opt/homebrew/opt/e2fsprogs/sbin`.

**Symptom**: `Host tools: missing -zig (brew install zig) or CC`
**Solution**: C test cases are cross-compiled on macOS (Apple clang cannot produce static
Linux ELF): `brew install zig`, or bring your own toolchain with `CC=<cross-gcc>` +
`CARGO_TARGET_<TRIPLE>_LINKER=<linker>`.

**Symptom**: `Components: vfio requires Linux host` (doctor FAIL)
**Solution**: vfio-pci passthrough architecturally depends on the Linux IOMMU and cannot be
supported on macOS — comment out
`[components.vfio]`. pmem on macOS is experimental (doctor WARN); do not rely on the
chain (memory-backend-file + dumpdtb/fdtput) until it has been validated under HVF.

**Symptom**: x86_64 / riscv64 guests on macOS are extremely slow or time out
**Solution**: Cross guests can only use TCG (HVF only for the matching arm64 host/target pair). Enlarge
`timeout_secs` (pure TCG emulation is slower — that is expected).

**Symptom**: `virtuoso kernel build` reports case-conflict errors after git clone / odd kernel tree behavior
**Solution**: Kernel source must live on a case-sensitive filesystem — the named
volume (ext4) of `virtuoso kernel` is correct; bind-mounting an APFS directory yourself will
hit the pitfall, so don't do that.

## Artifacts Filling the Disk

**Solution**: The most recent 20 runs are kept, and `target/runs/` lives under `target`,
wiped together by `cargo clean`; a single run's artifacts are usually < 1 MiB.
