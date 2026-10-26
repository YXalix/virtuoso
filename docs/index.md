# Virtuoso — Kernel E2E Virtualization Test Harness

Virtuoso boots a freshly-built kernel directly in QEMU and runs real test
programs against it, answering **"does my patch actually work?"** with a single
command — provision the kernel → build the test firmware image → boot and run
`/tests/` → judge via the serial marker protocol → persist the verdict to disk.

> *The kernel is the instrument, Virtuoso is the performer — every patch
> deserves a full solo performance.*

For design philosophy, the naming story, and an architecture overview, see
[Design and Philosophy](concepts/design.md).

## The Shortest Path

```bash
virtuoso doctor             # one-screen health check (✓/✗ component lines; --verbose for the full picture)
virtuoso test --timeout 60  # build → boot → judge → artifacts persisted to disk (prints the verdict line at the end)
```

To get started from scratch, see [Quick Start](quick-start.md).

## The Four Stages

| Stage | Command | In one line |
|---|---|---|
| Provision | `virtuoso kernel …` | Containerized kernel build: sources in a named volume + pinned toolchain, producing Image + `.ko` |
| Build | `virtuoso build` | Assemble the three images: initrd.img / rootfs.img / tools.img |
| Launch | `virtuoso shell` / `test` | QEMU two-stage boot (initramfs → rootfs), with the process group governed throughout |
| Judge | `virtuoso test` | Reconcile against serial marker protocol v1 → `verdict.json` persisted to disk (the single verdict source of truth) |

## Documentation Map

| Section | Contents |
|---|---|
| [Getting Started](quick-start.md) | From zero to the first `verdict: passed`: install dependencies → build the kernel → health check → first run |
| [Usage](usage/cli.md) | [CLI Reference](usage/cli.md), [Configuration](usage/configuration.md), [Writing Tests](usage/writing-tests.md), [Debugging](usage/debugging.md), [Artifacts and Triage](usage/artifacts.md), [AI Integration](usage/ai-integration.md), [Troubleshooting](usage/troubleshooting.md) |
| [Concepts and Contracts](concepts/design.md) | [Design and Philosophy](concepts/design.md), [Provisioning and Build Pipeline](concepts/pipeline.md), [Two-Stage Boot](concepts/boot.md), [Frozen Contracts](concepts/contracts.md), [Components](concepts/components.md) |
| [Contributing](contributing.md) | Contribution conventions and the CI pipeline |

The AI-oriented repository map and invariants live in the root
[AGENTS.md](https://github.com/YXalix/virtuoso/blob/main/AGENTS.md).
