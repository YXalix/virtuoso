# Contribution Guidelines

## Verification Loop

Any change to Rust source must first pass `cargo clippy --all-targets -- -D warnings` (the same
gate as CI — if it fails locally, do not commit); then run `virtuoso doctor → virtuoso test`.
The verdict line printed at the end of `test` is the authority; the only machine-readable surface
= `verdict.json` in the run directory; only `verdict: passed` counts as a pass.

## Conventions

- Shell code inside the VM stays POSIX-compatible (`infra/init` / `infra/init-initramfs` run
  under BusyBox `sh`, not bash); after editing, validate syntax with `busybox sh -n` first.
- New behavior exposed by the harness → add a matching test case.
- User-visible behavior change → update this docs site (`docs/`) and
  `devkit/skills/kernel-dev/SKILL.md`.
- Frozen items must not be touched: marker protocol v1, test exit-code semantics, frozen argv
  baseline — see [Frozen Contracts](concepts/contracts.md) and the "Frozen Invariants" section of AGENTS.md.
- Only edit docs in `docs/` (the single source of truth for documentation; elsewhere, reference it
  instead of copying content); pushing main automatically publishes gh-pages.

## CI/CD

Four pipelines in `.github/workflows/`:

| Workflow | Contents |
|---|---|
| `virtuoso-ci.yml` | build / clippy (`-D warnings`) / unit tests; E2E runs on a self-hosted runner (openEuler host + KVM), triggered manually via `workflow_dispatch`; on failure it uploads the entire `target/runs/` artifact |
| `busybox-release.yml` | Publishes prebuilt statically linked BusyBox for three architectures to a GitHub Release; the build environment is pinned to an `ubuntu:22.04` container (the tc applet of busybox 1.36.1 cannot compile against kernel headers ≥ 6.8, and cross-gcc behavior drifts across distros); cross-compilation requires explicitly installing `libc6-dev-<arch>-cross` (`--no-install-recommends` skips it) |
| `kernel-builder.yml` | Publishes the containerized kernel builder image to ghcr (`ghcr.io/yxalix/virtuoso-kernel`, linux/arm64 + linux/amd64 dual base); triggered by devkit/docker/ changes pushed to main or by manual dispatch; the image exists purely for speed — when it cannot be pulled, forge falls back to a local build |
| `docs.yml` | mdBook builds docs/ → GitHub Pages (gh-pages) |
