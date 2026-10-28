# CLI Reference

`virtuoso` is the single CLI entry point (`cargo install --path .` installs it into PATH).

## Command Overview

| Command | Description |
|---|---|
| `doctor [--arch a] [--verbose] [--json]` | Environment health check: one screen of ✓/✗/! component lines (a missing `rootfs.d/` gets an empty directory created automatically, see [Writing Test Cases](writing-tests.md#adding-files-to-the-rootfs-rootfsd)); `--verbose` = typed configuration diagnostics + the full checklist |
| `build [--busybox-only]` | Rebuilds initrd.img / rootfs.img / tools.img; `--busybox-only` prepares only the statically linked BusyBox for the current architecture (fetched from a GitHub Release + local cache reuse) |
| `kernel <subcommand>` | Containerized kernel provisioning: source in a named volume + a pinned-toolchain image (see [Kernel Provisioning (forge)](kernel-provisioning.md) for the full manual) |
| `clean` | Cleans generated images and staging directories |
| `shell [--kvm] [--tcg] [--gdb]` | Interactive VM (BusyBox shell); `--kvm` is Linux-only, `--tcg` forces pure emulation (use when macOS would default to HVF), `--gdb` suspends boot + GDB stub on `:1234` (always TCG) |
| `test [--timeout N] [--arch a] [--replay-until-fail N] [--tcg] [--only a,b]` | build → launch → judge → artifacts persisted to disk; replay mode stops at the first non-passed verdict; on macOS the default is HVF when host and target match; `--only` runs only the listed entries (flat name / `group/case` / trailing slash `group/` for a whole group; comma-separated / repeatable, zero matches counts as failure) |
| `probe --cmd/--cmd-file [--json] [--timeout N]` | AI interaction channel: batches of agent commands over virtio-serial, structured event stream (`--timeout` total wall-clock budget including boot, default 300) |
| `skill install \| uninstall` | Installs / removes AI skills from the kernel tree |

### The `kernel` Subcommand Group

| Subcommand | Description |
|---|---|
| `kernel clone <url> [--ref r] [--as vol] [--arch a] [--full]` | Clone kernel sources into a named volume (creates the volume, renders `.clangd` per arch, sets it current); default ref master, default volume `virtuoso-kernel`; `--full` keeps complete git history (default `--depth 1`, tree only) |
| `kernel defconfig [name] [--arch a]` | Run `make <name>` in the container (`.config` lands in the volume; default target `defconfig`) |
| `kernel build [-j N] [--arch a]` | `make Image/bzImage + modules` in the container + static bpftool + compile_commands.json (`/ksrc` raw form); `-j` defaults to host cores |
| `kernel path [--volume v]` | Print the host-visible path of a volume (default: the current one) — the value QEMU's `kernel_path` consumption resolves to |
| `kernel shell [--arch a]` | Interactive bash inside the container (the toolchain environment of the current volume) |
| `kernel list` | List volumes: content state (empty/cloned/configured) + current marker |
| `kernel use <volume> [--arch a]` | Switch current (writes `.virtuoso/kernel-current.json`; the devcontainer is re-rendered; switching back keeps incremental artifacts) |

## Common Behavior

- `--arch` overrides the same-named key in `virtuoso.toml` (equivalent env var `ARCH`,
  see [Configuration Reference](configuration.md));
- `--json` output can be piped directly (jq etc.);
- Exit-code semantics: 0=pass, 124=timeout (137 normalized), 130=Ctrl-C, anything else=failure —
  see [Frozen Contracts](../concepts/contracts.md) for details;
- A global Ctrl-C guard is always installed: it reaps the QEMU process group and persists any artifacts already produced.

## The Standard AI Verification Loop

```
doctor → test
```

The verdict line printed at the end of `test` is the authority (`verdict: passed` is the only
pass; the only machine-readable surface = `verdict.json` in the run directory). For the
workflow, see [AI Integration](ai-integration.md).

## Makefile Wrapper

The repo-root `Makefile` is a thin forwarding shell over the commands above; it adds no
behavior. Bare `make` lists the targets: `make doctor` / `build` / `test` / `shell` /
`probe C='uname -a'` / `skill` / `clean` / `kbuild` / `kclone URL=<git-url>`, plus
`make clippy` and `make install`. Extra flags ride on `A`
(e.g. `make test A="--replay-until-fail 5"`, `make shell A=--gdb`). `make` folds any
recipe failure to its own exit status 2; the verdict line and `verdict.json` remain the
sole authority, and the exit-code contract applies to `virtuoso` invocations.
