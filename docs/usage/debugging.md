# Debugging

## Interactive Shell

```bash
virtuoso shell            # TCG (works across architectures)
virtuoso shell --kvm      # native acceleration (when host = target architecture)
```

To exit: `Ctrl-A` then `x`. All `/tests/*` cases remain in the rootfs, so you can re-run an
individual test by hand to reproduce a failure.

## GDB Source-Level Kernel Debugging

Build the kernel with `CONFIG_DEBUG_INFO=y` (adding `CONFIG_DEBUG_INFO_DWARF5=y` and
`CONFIG_GDB_SCRIPTS=y` is recommended).

```bash
virtuoso shell --gdb      # terminal 1: suspended boot, listening on :1234
cd "$KERNEL_PATH" && \    # terminal 2:
gdb-multiarch vmlinux -ex 'target remote :1234'
```

## AI probe (virtio-serial Channel)

```bash
virtuoso probe --cmd 'uname -a' --cmd 'dmesg | tail'
virtuoso probe --cmd-file cmds.txt --json    # machine-readable event stream
```

Command batches are delivered via the guest-side virtuoso-agent (a JSON line protocol), and the
event stream is written to `agent-events.jsonl` in the run directory. See
[the agent section of Component Mechanics](../concepts/components.md#agent--ai-probe-channel)
for channel details, and [AI Integration](ai-integration.md) for AI workflows.

## Manual argv Review

```bash
QEMU=echo virtuoso shell    # prints only the QEMU argv, does not launch
```

---

For quick symptom lookup, see [Troubleshooting](troubleshooting.md).
