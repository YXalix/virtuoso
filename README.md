# Virtuoso — Kernel E2E Virtualization Test Harness

A kernel E2E virtualization test harness: boot a freshly-built kernel directly in
QEMU and run real test programs, answering *"does my patch actually work?"* with a
single command.

Build (builder), launch and process governance (launcher), judging (judge), and
containerized kernel provisioning (forge) all live in a single-package Rust crate;
`virtuoso` is the only CLI entry point.

**📖 Online docs: <https://yxalix.github.io/virtuoso/>**

## Quick Start

See [Quick Start](https://yxalix.github.io/virtuoso/quick-start.html) to get started
from scratch. The harness is designed to be placed inside a kernel source tree
(e.g. `kernel/virtuoso/`); when placed elsewhere, set `kernel_path` in
`virtuoso.toml`.

```bash
cargo install --path .      # installs the canonical binary virtuoso into PATH
virtuoso doctor             # one-screen environment health check (✓/✗ component lines; --verbose for full detail)
virtuoso test --timeout 60  # build → launch → judge → write artifacts to disk
```

## Common Commands

| Command | Purpose |
|---|---|
| `virtuoso doctor` | One-screen environment health check (`--verbose` for full diagnostics, `--json` for machine-readable output) |
| `virtuoso build` | Rebuild initrd.img / rootfs.img / tools.img |
| `virtuoso shell [--kvm] [--gdb]` | Interactive VM (BusyBox shell); `--gdb` suspends boot at the GDB stub `:1234` |
| `virtuoso test [--arch a] [--replay-until-fail N]` | build → launch → judge; `--replay-until-fail` reruns for flaky cases |
| `virtuoso probe --cmd '…'` | AI interaction channel (virtio-serial agent command batch) |

## Documentation

`docs/` is the single source of truth for documentation, published to GitHub Pages
via mdBook; build locally with `mdbook build docs`; pushing main updates the site
automatically.

- [Quick Start](docs/quick-start.md) — from zero to the first `verdict: passed`
- [Design and Philosophy](docs/concepts/design.md) — design principles, architecture overview, [frozen contracts](docs/concepts/contracts.md), [component mechanics](docs/concepts/components.md)
- [Usage](docs/usage/cli.md) — CLI, [configuration](docs/usage/configuration.md), [writing tests](docs/usage/writing-tests.md), [debugging](docs/usage/debugging.md), [artifacts and triage](docs/usage/artifacts.md)
- [Two-Stage Boot](docs/concepts/boot.md) — boot architecture, a "which file to change" handbook, the kernel quirk table
- [Troubleshooting](docs/usage/troubleshooting.md) — Symptom → Solution quick reference
- [AGENTS.md](AGENTS.md) — AI-facing repository map and invariants

## License

[MIT](LICENSE).
