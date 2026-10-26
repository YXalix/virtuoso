# Run Artifacts and Triage

Every `virtuoso test` writes to `target/runs/<unix_ms>-<arch>/` (the most recent
20 runs are kept); `probe` also writes a run directory (containing `serial.log` + `qemu-stderr.log` +
`agent-events.jsonl`, with no verdict). These artifacts are the triage data source shared by AI and humans.

## Run Directory Contents

| File | Contents |
|---|---|
| `serial.log` | QEMU stdout: kernel serial console + test markers (including panic stacks), persisted to disk line by line in sync |
| `qemu-stderr.log` | QEMU's own warnings |
| `build.log` | This run's build output (kept even on success) |
| `events.jsonl` | Structured events parsed line by line by the judge (test_start / test_end / assert / summary / marker / panic / oops / run_end) |
| `verdict.json` | Aggregated verdict + run fingerprint (kernel mtime/size, QEMU version, topology, timeout) |

**Terminal presentation separates the streams the same way** (test / probe): while running,
the terminal only echoes the guest serial console (i.e. the contents of `serial.log`);
QEMU's own output never floods the screen — it goes only to `qemu-stderr.log`,
and on a failure or timeout teardown at most its last 40 lines are echoed (the scene of an
early QEMU death or rejected arguments lives in those few lines); on success it stays quiet.
`virtuoso shell` (including the `--gdb` debug suspend) is an interactive session where
stdio is naturally merged, so QEMU output remains visible in real time.

## Verdict Semantics

**The verdict is the authority; the exit code is merely an interface contract**: under
`-no-reboot`, a kernel panic makes QEMU exit with code 0 — judging by exit code alone yields a
false pass; the verdict reconciles the `TEST_COMPLETE` marker against the exit code, and
panic / oops get their own categories.

| verdict | Semantics |
|---|---|
| `passed` | All cases passed; the marker protocol ran to completion |
| `failed` | Some case FAILED (or the summary counts do not add up) |
| `timeout` | Wall-clock timeout; the process group was reaped |
| `panic` | Kernel panic (its own category; exit may be 0 or 124, see [Troubleshooting](troubleshooting.md)) |
| `incomplete` | exit 0 but the marker protocol did not run to completion — treated as failure, must not be counted as a pass |
| `interrupted` | Interrupted by Ctrl-C during teardown (artifacts persisted to disk; verdict.json may be missing) |
| `build_failed` | The build stage failed |
| `unknown` | Unrecognized termination shape |

## Analysis Entry Points

```bash
cat target/runs/<id>/verdict.json        # the only machine-readable verdict surface (eight-state verdict + per-test details)
virtuoso test --replay-until-fail 5      # flaky replay: stops at the first non-passed verdict
```

There is no second command for verdict presentation: `test` prints the verdict line at the end;
the per-event facts live in
`events.jsonl`, and for close human reading go straight to `serial.log` and
`verdict.json` in the run directory. A run interrupted by Ctrl-C has its artifacts persisted
to disk, but verdict.json may be missing
(the marker protocol did not complete; human judgment should treat it as a failure).

For quick symptom lookup, see [Troubleshooting](troubleshooting.md).
