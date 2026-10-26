# Design and Philosophy

> *The kernel is the instrument, Virtuoso is the performer — every patch
> deserves a full solo performance.*

## Positioning

Virtuoso is a QEMU kernel E2E testing harness: it boots a freshly-built kernel
directly in QEMU and runs real test programs against it, answering **"does my
patch actually work?"** with a single command. It is not a test framework (for
assertions and test organization, see
[Writing Test Cases](../usage/writing-tests.md)), nor is it CI (but both CI
and AI consume its results through its verdict interface).

## Design Principles

| Principle | Meaning |
|---|---|
| **A single Rust crate is the sole behavioral authority** | All build and run logic lives in one crate; the `virtuoso` CLI is the only operational surface |
| **Type safety** | The architecture matrix, NUMA topologies, and component dependencies are all strongly typed; invalid configuration errors at parse time (not at runtime) |
| **RAII resource governance** | The QEMU process group and temporary directories are reaped on every exit path (error, panic, Ctrl-C, watchdog) |
| **Protocol stability** | Serial marker protocol v1 and exit-code semantics are frozen ([Frozen Contracts](contracts.md)); CI and AI interfaces evolve with zero awareness |
| **Verdict authority** | The verdict is the single source of truth for judging; the exit code is only an interface contract (under `-no-reboot`, a kernel panic makes QEMU exit 0) |
| **AI native** | Structured event streams (events.jsonl / agent-events.jsonl) + data-interface integration; raw serial text is only a supplement |
| **Self-contained on one machine** | The host needs only a Rust toolchain and QEMU; BusyBox is provisioned from prebuilt Releases, cached per architecture |

## Naming

**Virtuoso** (the master performer) — one word, three puns:

* **virt** — the machine type for arm64/riscv64 is literally `virt`: Virtuoso
  is, in the most literal sense, a "virt-machine virtuoso";
* **virtuoso** — an AI agent is exactly that performer: booting the machine,
  reading the serial console, judging life and death, writing tests;
* **a common word** — internationally understood, easy to remember and
  pronounce, and no other project by this name in kernel testing.

The domain modules are named after their roles: `builder` (builds images),
`launcher` (boots and governs processes), `judge` (renders verdicts),
`forge` (provisions kernels) — each name maps one-to-one onto its
responsibility, so you can say things like "have builder rebuild the initrd"
or "judge is waiting for TEST_COMPLETE".

## Architecture Overview

```text
                 +----------------------------------------------------------+
                 |                  Developer / CI Engine                   |
                 |        virtuoso test  ·  Claude Code (AI Skill)          |
                 +----------------------------------------------------------+
                                              |
                                              v
+-----------------------------------------------------------------------------------+
| Host: Virtuoso (control plane, a single Rust crate)                               |
|                                                                                   |
|+------------------------+      +------------------------+     +----------------+  |
||        virtuoso        | ---> |        builder         | --> |target/artifacts|  |
||  (orchestration CLI)   |      |initrd/rootfs/tools.img |     |   target/runs  |  |
|+------------------------+      +------------------------+     +----------------+  |
|             |                                                                     |
|             v                                                                     |
|               RAII: process-group registry · Ctrl-C guard · wall-clock watchdog   |
|+------------------------+                                                         |
||  launcher (guardian)   |                                                         |
|+------------------------+                                                         |
|             |                                                                     |
|             v                                                                     |
|+------------------------+      +------------------------+     +----------------+  |
||        launcher        |      |   qemu-system-<arch>   |     |    tools.img   |  |
||    boot DSL · QEMU     |      | virt / q35 · KVM · GDB |     |   vfio-pci     |  |
|+------------------------+      +------------------------+     +----------------+  |
|             |                                                                     |
|             v                                                                     |
|+----------------------------+     +---------------------------------------+       |
||            judge           | --> | events.jsonl (structured event stream)| --> AI|
||serial · markers · verdicts |     +---------------------------------------+       |
|+----------------------------+                                                     |
+-----------------------------------------------------------------------------------+
                                          |
                               -nographic | -serial mon:stdio
                                          v
+-----------------------------------------------------------------------------------+
| Guest VM:  virt / q35  ·  SMP×NUMA  ·  initramfs + ext4 rootfs (BusyBox)          |
|                                                                                   |
| [Kernel (under test)] --PID1--> [init: mountfs → insmod modules.conf → /tests/*]  |
|       |                                                                           |
|       +---> [test-xxx: PASS/FAIL/SKIP/INFO ... Test Results: N/M passed]          |
|       +---> [TEST_COMPLETE: ALL TESTS PASSED] --> poweroff                        |
+-----------------------------------------------------------------------------------+
```

## Documentation-Code Boundary

This documentation covers concepts, contracts, and usage; it does not restate
the code layout. Documentation and code comments are maintained independently
and never reference each other. The behavioral authority is the repository's
single Rust crate; the in-VM source assets live in `infra/`
(init, testcases, tools — mostly frozen data). Externally frozen interfaces
are listed in [Frozen Contracts](contracts.md); boot-side concepts are
covered in [Two-Stage Boot](boot.md).
