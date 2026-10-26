# Frozen Contracts

The following interfaces are frozen externally: changing any one of them
breaks downstream parsing (CI, AI skills, script pipelines). Evolution must go
through a new protocol version; semantics may not be changed in place.

| Dimension | Contract |
|---|---|
| Serial marker protocol | v1 frozen (appendix): changing the text breaks all downstream parsing |
| Exit codes | 0 = passed, 124 = timeout (137 normalized), anything else = failure |
| QEMU argv | with defaults (no data disk, no agent) the output matches the **frozen baseline of the respective host platform** verbatim, pinned by platform baseline unit tests |
| Static linking | tests must be `-static`; masking failures with `\|\| true` is forbidden |
| Configuration precedence | scalar keys: process environment variables > `virtuoso.toml` (env overrides same-named keys) |
| AI interface | skills depend only on marker protocol v1 and the artifact schemas (verdict.json / events.jsonl), never on harness internals |

## Exit Code Semantics

| Exit code | Meaning |
|---|---|
| `0` | passed (and the verdict must be `passed` to count as a true pass) |
| `124` | wall-clock timeout (QEMU was KILLed; 137 is normalized to 124) |
| `130` | interrupted by Ctrl-C (artifacts already persisted to disk) |
| other | failure |

Note: under `-no-reboot`, a kernel panic makes QEMU exit with **exit 0** —
the exit code is only an interface contract; judging goes by `verdict.json`.

## The Frozen argv Baseline

In the default shape (no data disk, no agent channel) the QEMU argv matches
the established baseline **verbatim**, pinned explicitly by platform baseline
unit tests. **Each host platform keeps its own baseline** (the host platform
type pins the verdict; it does not drift with the compilation target): the
differences between the two platforms converge to two places —

| Segment | Linux | macOS (Darwin) |
|---|---|---|
| Memory backend | `memory-backend-memfd,…,share=off` | `memory-backend-ram,…` (QEMU has no memfd) |
| Hardware acceleration | `shell --kvm` → `-enable-kvm`; test always TCG (no accel flag) | paths other than the same-architecture guest default/`--tcg` → `-accel hvf`; `--tcg` forces pure emulation; accel × platform mismatches (KVM@mac, HVF@linux) error at argv construction time |

Data disks (tools.img etc.) and the agent serial line are caller increments,
appended after the baseline. For manual review:

```bash
QEMU=echo virtuoso shell    # only prints the argv, does not start QEMU
```

## Appendix: Serial Marker Protocol v1 (Frozen Text)

```text
# test binary aggregation layer (printed by infra/init)
--- Running: <name> ---
PASSED: <name>
FAILED: <name>

# individual assertion lines (printed inside the test binary)
[PASS] <message>
[FAIL] <message>
[SKIP] <message>
[INFO] <message>

# per-binary summary (init assembles it from each case's exit code)
Test Results: <N>/<M> passed

# global termination marker (the harness's judging basis)
TEST_COMPLETE: ALL TESTS PASSED     # → exit 0
TEST_COMPLETE: SOME TESTS FAILED    # → exit 1

# timeout (decided on the host side)
exit 124   # wallclock timeout (kernel hang / runaway loop)
```

The judging engine parses the markers above line by line into structured
events. Test cases depend only on assertion macros such as `[PASS]`; the
aggregation-layer markers are emitted by `infra/init`. To write tests, see
[Writing Test Cases](../usage/writing-tests.md).
