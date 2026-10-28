# devkit/docker/ — Containerized Kernel Development Environment

Assets for containerized kernel provisioning (pinned toolchain image, static
devcontainer template, clangd config template):

| File | Purpose |
|---|---|
| `Dockerfile.kernel` | Pinned toolchain: ubuntu:24.04 + kernel build dependencies + three-architecture gcc + clangd + the Rust for Linux toolchain |
| `devcontainer.json` | Static template (default volume form); the per-volume git-ignored `.devcontainer/devcontainer.json` is rendered from it by `kernel use/clone` |
| `.clangd` | clangd config template (single source of truth), rendered per target architecture by `kernel clone` |

The full manual — volume model, standard workflow, volume management, the
devcontainer/clangd editing path, Remote-SSH, image publishing — lives in the
docs book (the single source of truth):

- In-repo: [docs/usage/kernel-provisioning.md](../../docs/usage/kernel-provisioning.md)
- Online: <https://yxalix.github.io/virtuoso/usage/kernel-provisioning.html>
