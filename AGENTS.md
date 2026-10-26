# Virtuoso Agent Guide

This file documents only **rules and consensus**: the contracts, command surface, and
validation loop that AI must follow when working in this repository. Design narratives,
component mechanics, the full configuration key list, artifact details, and so on take
`docs/` as the **single source of truth** ([online site](https://yxalix.github.io/virtuoso/));
this file does not duplicate its content.

## Positioning and Authority

Virtuoso is a QEMU kernel E2E test harness: provision a kernel → build BusyBox+musl
static test firmware → boot QEMU and run `/tests/` → judge via the serial marker
protocol → write verdict.json to disk. **A single-package Rust crate is the sole
behavioral authority** (under `src/`: `main` subcommand dispatch + `cli` command
implementations + `config` typed configuration + `runs` run artifacts + the domain
modules `builder` build / `launcher` launch+process governance / `judge` judging /
`forge` containerized kernel provisioning; `arch` (Arch matrix + HostOs) and
`util` (top-level utility set) are the former common, flattened out);
`infra/` holds in-VM source assets (init, testcases, tools, the busybox list —
mostly frozen data); the repo-root `rootfs.d/` is the user drop-in (merged
additively into the rootfs at build time, additive-only; git-ignored, do not commit);
`devkit/` is the kernel development container and AI skill source; build outputs and
run artifacts land in `target/` (git-ignored).

## Quick Commands (copy and run)

`virtuoso` = the canonical binary (`cargo install --path .` installs it into PATH);
after changing CLI source code, rerun the install once to stay up to date.

```bash
virtuoso doctor             # environment health check (✓/✗ component lines; --verbose full diagnostics; --json)
virtuoso build              # builder: rebuild initrd.img / rootfs.img / tools.img (--busybox-only prepares BusyBox only)
virtuoso kernel clone <url> [--ref r] [--as vol] [--arch a] [--full]
                            # forge: kernel source into a named volume (+ .clangd rendering + writes current; --full full history)
virtuoso kernel build       # forge: in-container make Image/modules + static bpftool + CDB (/ksrc in raw form)
                            #   (also defconfig/path/shell/list/use; source editing goes through devkit/docker devcontainer)
virtuoso test --timeout 60  # test: launcher boots → judge judges → artifacts land on disk
virtuoso test --replay-until-fail 5   # flaky rerun: stop at the first non-passed result
virtuoso shell [--kvm|--tcg] [--gdb]
                            # interactive VM; --gdb = suspend waiting for GDB :1234 (always TCG)
virtuoso probe --cmd 'uname -a' [--cmd-file f] [--json] [--timeout s]
                               # AI interaction channel: virtio-serial agent command batch (structured event stream; --timeout defaults to 300)
virtuoso skill install      # installs the kernel-dev + kernel-virtuoso skills into the kernel tree
```

The standard validation loop for AI: `clippy → doctor → test`. After changing Rust
source code, always run `cargo clippy --all-targets -- -D warnings` first (the same
gate as CI; if it fails locally, do not commit), then `doctor → test`.
**The verdict is the verdict line printed at the end of test; the sole
machine-readable surface = `verdict.json` in the run directory**; exit codes are only
an interface contract; `verdict: passed` is the only pass. doctor is the single entry
point for health checks (one-screen presentation + `--verbose` full detail), backed by
the same engine as builder::verify (new prerequisites only touch the engine; the
presentation follows automatically).

## Frozen Invariants (do not break)

1. **Marker protocol v1** (emitted by `infra/init`, frozen text in
   `docs/concepts/contracts.md`):
   `--- Running: X ---`, `PASSED:/FAILED: X`, `Test Results: N/M passed`,
   `TEST_COMPLETE: ALL TESTS PASSED|SOME TESTS FAILED`. Changing the text breaks all
   downstream parsing.
2. **test exit codes**: 0=passed, 124=timeout, anything else=failed.
3. **argv freeze**: the output of `QemuInvocation::argv` is frozen to the
   **per-host-platform dual baselines** (Linux=memfd backend, macOS=ram backend,
   HVF→`-accel hvf`), guarded by the `argv_*` unit tests in
   `src/launcher/qemu/argv.rs`, which pin each platform explicitly;
   for manual review, use `QEMU=echo virtuoso shell` to print argv. The data disk and
   agent channel are caller-side increments: **the default (no disk, no agent) argv
   matches its platform's baseline verbatim**.
4. Tests must be statically linked (`-static`); masking failures with `|| true` is
   forbidden.
5. **Configuration precedence: process environment variables > `virtuoso.toml`**: for
   the same scalar key, the process environment variable wins (CI/command-line
   temporary parameter changes never touch files); persistent configuration is written
   only to `virtuoso.toml`.

## Consensus

- **Docs are the single source of truth**: `docs/` (mdBook book root = `docs/`, built
  with `mdbook build docs`; pushing main auto-publishes gh-pages). Change docs only in
  `docs/`; elsewhere, reference them instead of copying content.
  Entry points: `docs/concepts/` (philosophy/pipeline/boot/contracts/components),
  `docs/usage/` (CLI/configuration/writing tests/debugging/artifacts/AI
  integration/troubleshooting), `docs/quick-start.md`, `docs/contributing.md`.
- **English-only surface**: code comments, docs, and user-visible strings are written
  in English (repo-wide conversion); keep all new text English too.
- **Component-based configuration**: `virtuoso.toml` is the single configuration
  surface (env > toml); `[components.*]` sections declare VM capabilities; the builder
  generates the module manifest from the union of requires of the enabled components.
  For all keys and semantics see `docs/usage/configuration.md`; for the template see
  the comments in the repo-root `virtuoso.toml`.
- **Run artifacts**: `virtuoso test` writes `target/runs/<unix_ms>-<arch>/` (last 20
  runs kept); `verdict.json` is the sole machine-readable verdict surface and
  `events.jsonl` the per-event structured source of truth. See `docs/usage/artifacts.md`
  for the file table and the eight Verdict state semantics.
- **Kernel development container**: the source of truth for kernel source is the named
  volume; the host edits it through the platform view of `virtuoso kernel path`
  (the view is case-faithful; never check out a kernel tree onto APFS, never run two
  makes in parallel). See `devkit/skills/kernel-dev/SKILL.md` and
  `docs/quick-start.md` for details.
- **Known environment quirks (do not "fix")**: see the quirk table in
  `docs/concepts/boot.md` — the empty `/dev` fallback to mknod, virtio/ext4 having to
  be `=m` in the initramfs, and so on are all observed openEuler kernel behavior;
  removing the corresponding fallbacks will re-trigger those pitfalls.
