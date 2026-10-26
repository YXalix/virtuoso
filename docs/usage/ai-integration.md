# AI Integration

Virtuoso treats AI agents as first-class citizens: structured event streams as the source of
truth, skills for knowledge injection, probe for runtime interaction.

## Standard Verification Loop

```
doctor → test
```

**The verdict line printed at the end of `test` is the authority** (the only machine-readable
surface = `verdict.json` in the run directory): `verdict: passed` is the only pass. The exit
code is merely an interface contract (0=pass, 124=timeout, anything else=failure) — under
`-no-reboot`, a kernel panic makes QEMU exit with code 0, so judging by exit code alone yields
a false pass. For the eight verdict states and their semantics, see
[Run Artifacts and Triage](artifacts.md).

## Data Interfaces

| Capability | Input | Output | Integration Point |
|---|---|---|---|
| **Test scaffold generation** | natural-language description / git diff | a case crate (Rust entry + C body) | compiled by the builder, ready to run |
| **Serial log triage** | `events.jsonl` / `verdict.json` | root-cause hypotheses + suggested reproduction commands | read the run directory artifacts directly (the eight-state `verdict.json` + per-event `events.jsonl`) |
| **Interactive probing inside the VM** | batches of shell commands | structured event stream (`agent-events.jsonl`) | `virtuoso probe` (virtio-serial + virtuoso-agent) |

## skill

```bash
virtuoso skill install      # installs the kernel-dev + kernel-virtuoso skills into the kernel tree
virtuoso skill uninstall
```

- `kernel-dev`: teaches the AI to drive the test loop / write cases / parse the serial console / triage failures;
- `kernel-virtuoso`: AI data-interface integration (the probe channel).

Once installed, the AI discovers them automatically inside the kernel tree (the `.claude/skills/` mechanism).

## The probe Channel

`virtuoso probe` always has the agent channel on (independent of component switches); it
delivers command batches via the guest-side virtuoso-agent and returns a structured event
stream:

```bash
virtuoso probe --cmd 'uname -a' --cmd 'cat /proc/iomem' --json
```

See [the agent section of Component Mechanics](../concepts/components.md#agent--ai-probe-channel)
for the channel mechanics.

## eBPF Observation (Program Mode)

The three forms of observation are complementary; the first two ship with the images and are
available by default:

- **Script mode**: `bpftrace` (the `[components.bpf]` component, off by default) for ad-hoc
  exploration — write a one-liner and run it immediately;
- **Program mode**: `bpf-run` lives permanently on the tools disk `/bin` (ships with the disk
  by default, independent of component switches); it consumes ready-made `.bpf.o` objects
  compiled on the host and attaches them per section (kprobe/kretprobe/tracepoint/
  raw_tracepoint), automatically decoding ringbuf events into JSONL — stdout is always a pure
  event stream, landing in `agent-events.jsonl` via the probe channel: the structured
  observation loop for AI;
- **Control plane**: `bpftool` likewise lives in `/bin` (ships with the disk by default) —
  `virtuoso kernel build` builds it statically along the way from the kernel tree (the in-tree
  libbpf version strictly matches the kernel under test); `btf dump` inspects vmlinux/module
  BTF and `prog show` / `map dump` observe load state, making it the inspection companion of
  the bpf-run workflow (if the kernel tree has no artifacts, the build skips it with a WARN).

`.bpf.o` provisioning goes through the rootfs.d drop-in (merged into the rootfs at build
time — "each freshly compiled ready-made program"):

```bash
clang -target bpf -g -O2 -c my.bpf.c -o rootfs.d/bpf/my.bpf.o
virtuoso build
virtuoso probe --cmd 'bpf-run /bpf/my.bpf.o --wait 5'
```

Key points about `bpf-run`:

- Event decoding matches record length against named structs in the object's BTF
  (`--struct NAME` to specify one explicitly); records that cannot be matched fall back to hex.
  **The record struct must be anchored into BTF**: aya rejects `values` annotations on ringbufs,
  and `-O2` strips types used only by reserve — use a one-line extern function anchor to carry
  the struct into BTF:
  `struct event *bpf_anchor_event(struct event *e) { return e; }`
- Attachment failure does not abort the whole run (a warn per program; exit 1 only if all
  fail; the load error chain carries the verifier log — the feedback surface for kernel BPF
  development); `--program NAME` filters, `--wait SECS` bounds the collection window
  (default 5s), and exit code 0 = normal (including zero events).
- Inspect the object's BTF with the dump-btf example (whether structs made it into the table,
  whether maps are recognized as ringbufs): `cargo run --manifest-path infra/tools/bpf-run/Cargo.toml
  --example dump-btf -- <obj.bpf.o>`.

Temporary, one-off kernel-state observation does not require writing BPF programs: ftrace
(tracefs is already mounted at `/sys/kernel/tracing/`) and bpftrace already cover it.

## Safety Boundaries (Architectural Constraints)

- The AI **reads and analyzes** test artifacts and kernel logs read-only; any modification to
  kernel source must be confirmed by a human and executed by the developer;
- `events.jsonl` is the AI's only structured source of truth; the raw serial text serves only
  as supplementary context, keeping triage traceable;
- The interface between skills and the harness is a [frozen contract](../concepts/contracts.md):
  skills depend only on the protocol and the artifact schema, never on the implementation.
