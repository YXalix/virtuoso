# CLI Reference

`virtuoso` is the single CLI entry point (`cargo install --path .` installs it into PATH).

## Command Overview

| Command | Description |
|---|---|
| `doctor [--arch a] [--verbose] [--json]` | Environment health check: one screen of ✓/✗/! component lines (a missing `rootfs.d/` gets an empty directory created automatically, see [Writing Test Cases](writing-tests.md#adding-files-to-the-rootfs-rootfsd)); `--verbose` = typed configuration diagnostics + the full checklist |
| `build [--busybox-only]` | Rebuilds initrd.img / rootfs.img / tools.img; `--busybox-only` prepares only the statically linked BusyBox for the current architecture (fetched from a GitHub Release + local cache reuse) |
| `kernel clone/defconfig/build/path/shell` | Containerized kernel provisioning: source in a named volume + a pinned-toolchain image (see [devkit/docker/README.md](../../devkit/docker/README.md) for details) |
| `kernel list` / `kernel use <volume>` | Volume management: list volumes (status + current marker) / switch current (state file `.virtuoso/kernel-current.json`) |
| `clean` | Cleans generated images and staging directories |
| `shell [--kvm] [--tcg] [--gdb]` | Interactive VM (BusyBox shell); `--kvm` is Linux-only, `--tcg` forces pure emulation (use when macOS would default to HVF), `--gdb` suspends boot + GDB stub on `:1234` (always TCG) |
| `test [--timeout N] [--arch a] [--replay-until-fail N] [--tcg] [--only a,b]` | build → launch → judge → artifacts persisted to disk; replay mode stops at the first non-passed verdict; on macOS the default is HVF when host and target match; `--only` runs only the listed entries (flat name / `group/case` / trailing slash `group/` for a whole group; comma-separated / repeatable, zero matches counts as failure) |
| `probe --cmd/--cmd-file [--json] [--timeout N]` | AI interaction channel: batches of agent commands over virtio-serial, structured event stream |
| `skill install \| uninstall` | Installs / removes AI skills from the kernel tree |

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
