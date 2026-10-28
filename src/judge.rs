//! judge — the judge: serial marker-protocol v1 assertion engine, verdict
//! determination, and the verdict.json schema.
//!
//! Protocol v1 (frozen text):
//! `[PASS]/[FAIL]/[SKIP]/[INFO]` assert lines, `--- Running: X ---` /
//! `PASSED:/FAILED: X` (init assembly layer), the `Test Results: N/M passed`
//! summary, and the `TEST_COMPLETE` termination marker.
//!
//! Judging semantics (since Phase 1.5): the exit code remains the interface
//! contract but is **no longer by itself** the pass criterion — under
//! `-no-reboot` a kernel panic makes QEMU exit 0 (a false pass); `judge`
//! goes by the marker protocol and reconciles it with the exit code, with
//! panic/oops as their own verdict categories.
//!
//! Sections: judging core (protocol types + the `judge` reconciliation) →
//! exit code semantics table (the sole semantics table) → marker protocol
//! parser → verdict.json schema.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- Judging core

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TestStatus {
    Pass,
    Fail,
}


#[derive(Debug)]
pub(crate) struct TestResult {
    pub name: String,
    pub status: TestStatus,
    pub assert_pass: u32,
    pub assert_fail: u32,
    pub assert_skip: u32,
}

/// Event envelope for one events.jsonl line (schema v2). `extra` carries the
/// per-kind payload (test_start/test_end: name...; assert: result; summary:
/// passed/total; marker: value; run_end: exit_code/duration_ms/verdict;
/// panic/oops: null).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Event {
    /// Line number in the serial log (1-based); the synthetic run_end event
    /// has no line number → null.
    pub line_no: Option<u64>,
    pub kind: EventKind,
    /// Raw serial line.
    pub text: String,
    /// Appearance order, starting from 0.
    pub seq: usize,
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventKind {
    TestStart,
    TestEnd,
    Assert,
    Summary,
    Marker,
    Panic,
    Oops,
    RunEnd,
}

/// Overall parse result of one serial stream.
#[derive(Debug, Default)]
pub(crate) struct Audit {
    pub tests: Vec<TestResult>,
    /// The (passed, total) reported by init
    pub summary: Option<(u32, u32)>,
    /// Content after TEST_COMPLETE: (None = protocol did not run to completion)
    pub marker: Option<String>,
    pub panics: Vec<String>,
    pub oops: Vec<String>,
    /// Structured event stream (the lines of events.jsonl, in appearance order)
    pub events: Vec<Event>,
}

/// Run verdict — the verdict strings are a frozen interface (runs artifacts
/// and CI judging depend on them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    Passed,
    Failed,
    Timeout,
    Panic,
    Incomplete,
    Interrupted,
    BuildFailed,
    Unknown,
}

impl Verdict {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Verdict::Passed => "passed",
            Verdict::Failed => "failed",
            Verdict::Timeout => "timeout",
            Verdict::Panic => "panic",
            Verdict::Incomplete => "incomplete",
            Verdict::Interrupted => "interrupted",
            Verdict::BuildFailed => "build_failed",
            Verdict::Unknown => "unknown",
        }
    }
}

impl Audit {
    pub(crate) fn passed_count(&self) -> usize {
        self.tests
            .iter()
            .filter(|t| t.status == TestStatus::Pass)
            .count()
    }

    pub(crate) fn failed_count(&self) -> usize {
        self.tests
            .iter()
            .filter(|t| t.status == TestStatus::Fail)
            .count()
    }

    pub(crate) fn skipped_count(&self) -> usize {
        self.tests.iter().filter(|t| t.assert_skip > 0).count()
    }

    /// The verdict expectation for the termination marker; None = protocol
    /// did not run to completion.
    pub(crate) fn marker_verdict(&self) -> Option<bool> {
        match self.marker.as_deref() {
            Some("ALL TESTS PASSED") => Some(true),
            Some("SOME TESTS FAILED") => Some(false),
            _ => None,
        }
    }
}

/// Judge: reconcile the marker protocol with the exit code.
pub(crate) fn judge(exit_code: i32, build_failed: bool, a: &Audit) -> Verdict {
    if build_failed {
        return Verdict::BuildFailed;
    }
    let kernel_broken = !a.panics.is_empty() || !a.oops.is_empty();
    if exit_code == 0 {
        if kernel_broken {
            Verdict::Panic
        } else {
            match (a.marker_verdict(), a.failed_count()) {
                (Some(true), 0) => Verdict::Passed,
                (None, _) => Verdict::Incomplete,
                _ => Verdict::Failed,
            }
        }
    } else if (exit_code == 124 || exit_code == 137) && kernel_broken {
        Verdict::Panic
    } else if exit_code == 130 {
        Verdict::Interrupted
    } else if exit_code == 124 || exit_code == 137 {
        Verdict::Timeout
    } else {
        Verdict::Failed
    }
}

// ---------------------------------------------------------------- Exit code semantics

// Exit code semantics — the sole semantics table (frozen: 0=passed,
// 124=timeout, 137 normalized to 124, 130=interrupted; any other
// nonzero=failed). The `judge` reconciliation, host-side reaping, and
// verdict.json's `exit_semantics` field all take their values from here;
// writing a separate mapping elsewhere is not allowed.

/// Timeout exit code (same 124 semantics as `timeout --signal=KILL`).
pub(crate) const EXIT_TIMEOUT: i32 = 124;
/// Ctrl-C / signal interrupt exit code (the constant is pinned in
/// crate::util — guardian does not depend on judge).
pub(crate) use crate::util::EXIT_INTERRUPTED;

/// Exit code normalization: 137 (SIGKILL) is normalized to 124 with timeout
/// semantics; a missing exit code (death by signal) becomes 130; everything
/// else keeps its real code.
pub(crate) fn normalize(code: Option<i32>) -> i32 {
    match code {
        Some(124) | Some(137) => EXIT_TIMEOUT,
        Some(n) => n,
        None => EXIT_INTERRUPTED,
    }
}

/// Exit code → semantic label (verdict.json's `exit_semantics` field).
pub(crate) fn semantics(code: Option<i32>) -> &'static str {
    match code {
        Some(0) => "ok",
        Some(124) => "timeout",
        Some(130) => "interrupted",
        Some(_) => "error",
        None => "unknown",
    }
}

// ---------------------------------------------------------------- Marker protocol parser

// Marker protocol v1 parser: full serial text → structured Audit
// (tests/summary/marker/panics/oops + the events stream).

/// Parse the full serial text. Line numbers start at 1; events carry
/// line_no and the appearance-order seq.
pub(crate) fn parse(text: &str) -> Audit {
    let mut a = Audit::default();
    let mut current: Option<usize> = None;

    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let t = raw.trim_start();

        let ev = |kind: EventKind, extra: serde_json::Value, a: &mut Audit| {
            let seq = a.events.len();
            a.events.push(Event {
                line_no: Some(line_no as u64),
                kind,
                text: raw.to_string(),
                seq,
                extra,
            });
        };

        if let Some(name) = t
            .strip_prefix("--- Running: ")
            .and_then(|s| s.strip_suffix(" ---"))
        {
            current = Some(a.tests.len());
            a.tests.push(TestResult {
                name: name.trim().to_string(),
                status: TestStatus::Fail,
                assert_pass: 0,
                assert_fail: 0,
                assert_skip: 0,
            });
            ev(
                EventKind::TestStart,
                serde_json::json!({ "name": name.trim() }),
                &mut a,
            );
        } else if let Some(name) = t.strip_prefix("PASSED: ") {
            if let Some(tr) = a.tests.iter_mut().rev().find(|tr| tr.name == name.trim()) {
                tr.status = TestStatus::Pass;
            }
            ev(
                EventKind::TestEnd,
                serde_json::json!({ "name": name.trim(), "status": "pass" }),
                &mut a,
            );
        } else if let Some(name) = t.strip_prefix("FAILED: ") {
            if let Some(tr) = a.tests.iter_mut().rev().find(|tr| tr.name == name.trim()) {
                tr.status = TestStatus::Fail;
            }
            ev(
                EventKind::TestEnd,
                serde_json::json!({ "name": name.trim(), "status": "fail" }),
                &mut a,
            );
        } else if t.starts_with("[PASS] ") {
            if let Some(idx) = current {
                if let Some(tr) = a.tests.get_mut(idx) {
                    tr.assert_pass += 1;
                }
            }
            ev(
                EventKind::Assert,
                serde_json::json!({ "result": "pass" }),
                &mut a,
            );
        } else if t.starts_with("[FAIL] ") {
            if let Some(idx) = current {
                if let Some(tr) = a.tests.get_mut(idx) {
                    tr.assert_fail += 1;
                }
            }
            ev(
                EventKind::Assert,
                serde_json::json!({ "result": "fail" }),
                &mut a,
            );
        } else if t.starts_with("[SKIP] ") {
            if let Some(idx) = current {
                if let Some(tr) = a.tests.get_mut(idx) {
                    tr.assert_skip += 1;
                }
            }
            ev(
                EventKind::Assert,
                serde_json::json!({ "result": "skip" }),
                &mut a,
            );
        } else if let Some(rest) = t.strip_prefix("Test Results: ") {
            let frac = rest.split_whitespace().find(|tok| tok.contains('/'));
            let pair = frac.and_then(|f| {
                let (x, y) = f.split_once('/')?;
                Some((x.parse::<u32>().ok()?, y.parse::<u32>().ok()?))
            });
            if let Some((passed, total)) = pair {
                a.summary = Some((passed, total));
                ev(
                    EventKind::Summary,
                    serde_json::json!({ "passed": passed, "total": total }),
                    &mut a,
                );
            }
        } else if let Some(rest) = t.strip_prefix("TEST_COMPLETE: ") {
            a.marker = Some(rest.trim().to_string());
            ev(
                EventKind::Marker,
                serde_json::json!({ "value": rest.trim() }),
                &mut a,
            );
        } else if raw.contains("Kernel panic - not syncing") {
            a.panics.push(raw.to_string());
            ev(EventKind::Panic, serde_json::Value::Null, &mut a);
        } else if raw.contains("Oops:") {
            a.oops.push(raw.to_string());
            ev(EventKind::Oops, serde_json::Value::Null, &mut a);
        }
    }
    a
}

// ---------------------------------------------------------------- verdict.json schema

// verdict.json schema and construction (the single schema source): serde
// field order is the persisted key order; construction
// (`VerdictReport::build`) and reading back (serde deserialization) share
// the same struct, so key names/values are compile-time guarantees. IO
// (persisting, directory management) lives in the runs layer; this only
// handles semantics.

/// Top-level verdict.json schema (schema: 2).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct VerdictReport {
    pub schema: u32,
    pub run_id: String,
    pub arch: String,
    pub started_at_unix_ms: Option<u64>,
    pub started_at_utc: Option<String>,
    pub duration_ms: u64,
    pub timeout_s: u64,
    pub exit_code: Option<i32>,
    pub exit_semantics: String,
    pub verdict: Verdict,
    pub kernel: Option<FileFingerprint>,
    pub qemu_version: Option<String>,
    pub guest: serde_json::Value,
    pub tests: Vec<TestEntry>,
    pub summary: SummaryCounts,
    pub marker_complete: Option<String>,
    pub panics: Vec<String>,
    pub oops: Vec<String>,
    pub repro: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Artifacts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict_source: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileFingerprint {
    pub path: String,
    pub size_bytes: u64,
    pub mtime_unix_ms: Option<u64>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct TestEntry {
    pub name: String,
    pub status: TestStatus,
    pub asserts: AssertCounts,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct AssertCounts {
    pub pass: u32,
    pub fail: u32,
    pub skip: u32,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct SummaryCounts {
    pub pass: usize,
    pub fail: usize,
    pub skip: usize,
    pub reported_pass: Option<u32>,
    pub reported_total: Option<u32>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Artifacts {
    pub serial: String,
    pub qemu_stderr: String,
    pub build_log: String,
    pub events: String,
}

/// Metadata of one run (the non-parse parts of verdict.json).
pub(crate) struct RunMeta {
    pub run_id: String,
    pub arch: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timeout_s: String,
    pub kernel: Option<PathBuf>,
    pub qemu_version: Option<String>,
    pub topo: serde_json::Value,
    pub build_failed: bool,
}

impl RunMeta {
    fn started_ms(&self) -> Option<u64> {
        self.run_id.split('-').next().and_then(|s| s.parse().ok())
    }

    fn repro(&self) -> String {
        format!(
            "virtuoso test --timeout {} --arch {}",
            self.timeout_s, self.arch
        )
    }

    /// Verdict via the sole reconciliation (`judge` handles build_failed itself).
    pub(crate) fn verdict_of(&self, audit: &Audit) -> Verdict {
        judge(self.exit_code.unwrap_or(-1), self.build_failed, audit)
    }
}

fn file_fingerprint(p: &Path) -> Option<FileFingerprint> {
    let meta = std::fs::metadata(p).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    Some(FileFingerprint {
        path: p.display().to_string(),
        size_bytes: meta.len(),
        mtime_unix_ms: mtime,
    })
}

impl VerdictReport {
    /// The sole construction site of verdict.json: judge parsing + run
    /// metadata → report.
    pub(crate) fn build(audit: &Audit, meta: &RunMeta) -> VerdictReport {
        VerdictReport {
            schema: 2,
            run_id: meta.run_id.clone(),
            arch: meta.arch.clone(),
            started_at_unix_ms: meta.started_ms(),
            started_at_utc: meta.started_ms().map(crate::util::format_utc),
            duration_ms: meta.duration_ms,
            timeout_s: meta.timeout_s.parse().unwrap_or(0),
            exit_code: meta.exit_code,
            exit_semantics: semantics(meta.exit_code).to_string(),
            verdict: meta.verdict_of(audit),
            kernel: meta.kernel.as_deref().and_then(file_fingerprint),
            qemu_version: meta.qemu_version.clone(),
            guest: meta.topo.clone(),
            tests: audit
                .tests
                .iter()
                .map(|t| TestEntry {
                    name: t.name.clone(),
                    status: t.status,
                    asserts: AssertCounts {
                        pass: t.assert_pass,
                        fail: t.assert_fail,
                        skip: t.assert_skip,
                    },
                })
                .collect(),
            summary: SummaryCounts {
                pass: audit.passed_count(),
                fail: audit.failed_count(),
                skip: audit.skipped_count(),
                reported_pass: audit.summary.map(|(a, _)| a),
                reported_total: audit.summary.map(|(_, b)| b),
            },
            marker_complete: audit.marker.clone(),
            panics: audit.panics.clone(),
            oops: audit.oops.clone(),
            repro: meta.repro(),
            artifacts: None,
            verdict_source: None,
        }
    }
}

// ---------------------------------------------------------------- Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_passing_run() {
        let log = "--- Running: test_a ---\n  [PASS] ok\nPASSED: test_a\n\
                   Test Results: 1/1 passed\nTEST_COMPLETE: ALL TESTS PASSED\n";
        let a = parse(log);
        assert_eq!(a.tests.len(), 1);
        assert_eq!(a.tests[0].status, TestStatus::Pass);
        assert_eq!(a.tests[0].assert_pass, 1);
        assert_eq!(a.summary, Some((1, 1)));
        assert_eq!(a.marker_verdict(), Some(true));
        assert_eq!(judge(0, false, &a), Verdict::Passed);
    }

    #[test]
    fn parses_failure_with_skip_counts() {
        let log = "--- Running: t1 ---\n  [PASS] a\n  [FAIL] b\n  [SKIP] c\nFAILED: t1\n\
                   --- Running: t2 ---\nPASSED: t2\n\
                   Test Results: 1/2 passed\nTEST_COMPLETE: SOME TESTS FAILED\n";
        let a = parse(log);
        assert_eq!(a.tests[0].status, TestStatus::Fail);
        assert_eq!(a.tests[0].assert_fail, 1);
        assert_eq!(a.tests[0].assert_skip, 1);
        assert_eq!(a.tests[1].status, TestStatus::Pass);
        assert_eq!(a.summary, Some((1, 2)));
        assert_eq!(judge(1, false, &a), Verdict::Failed);
        assert_eq!(
            judge(0, false, &a),
            Verdict::Failed,
            "marker says failed → failed even with exit 0"
        );
    }

    #[test]
    fn summary_fraction_parses_exactly_two_numbers() {
        let a = parse("Test Results: 3/7 passed\n");
        assert_eq!(a.summary, Some((3, 7)));
        let b = parse("Test Results: passed\n");
        assert_eq!(b.summary, None);
    }

    #[test]
    fn grouped_names_with_slashes_pair_correctly() {
        // Test groups: the name token = "group/case". Names are opaque
        // strings — Running/PASSED/FAILED pair by exact equality; slashes
        // play no part in any splitting.
        let log = "--- Running: smoke/t1 ---\n  [PASS] a\nPASSED: smoke/t1\n\
                   --- Running: smoke/t2 ---\nFAILED: smoke/t2\n\
                   --- Running: t3 ---\nPASSED: t3\n\
                   Test Results: 2/3 passed\nTEST_COMPLETE: SOME TESTS FAILED\n";
        let a = parse(log);
        assert_eq!(a.tests.len(), 3);
        assert_eq!(a.tests[0].name, "smoke/t1");
        assert_eq!(a.tests[0].status, TestStatus::Pass);
        assert_eq!(a.tests[1].name, "smoke/t2");
        assert_eq!(a.tests[1].status, TestStatus::Fail);
        assert_eq!(a.tests[2].name, "t3");
        assert_eq!(a.summary, Some((2, 3)));
        assert_eq!(judge(1, false, &a), Verdict::Failed);
    }

    #[test]
    fn panic_with_exit_zero_is_false_pass_detected() {
        // -no-reboot: panic triggers reset → QEMU exits 0, no TEST_COMPLETE
        let log = "--- Running: t1 ---\n  [PASS] step\n\
                   [    1.2] Kernel panic - not syncing: boom\n";
        let a = parse(log);
        assert_eq!(a.panics.len(), 1);
        assert_eq!(judge(0, false, &a), Verdict::Panic);
    }

    #[test]
    fn timeout_with_panic_is_panic_not_timeout() {
        let a = parse("Kernel panic - not syncing: boom\n");
        assert_eq!(judge(124, false, &a), Verdict::Panic);
        assert_eq!(judge(137, false, &a), Verdict::Panic);
    }

    #[test]
    fn timeout_without_marker_is_timeout() {
        let a = parse("[    0.0] Linux version 6.6.0\n");
        assert_eq!(judge(124, false, &a), Verdict::Timeout);
    }

    #[test]
    fn missing_marker_with_exit_zero_is_incomplete() {
        let a = parse("--- Running: t1 ---\nPASSED: t1\n");
        assert_eq!(judge(0, false, &a), Verdict::Incomplete);
    }

    #[test]
    fn oops_alone_blocks_pass() {
        let log = "Oops: 0000 [#1]\nTEST_COMPLETE: ALL TESTS PASSED\n";
        let a = parse(log);
        assert_eq!(judge(0, false, &a), Verdict::Panic);
    }

    #[test]
    fn build_failure_short_circuits() {
        let a = parse("TEST_COMPLETE: ALL TESTS PASSED\n");
        assert_eq!(judge(0, true, &a), Verdict::BuildFailed);
    }

    #[test]
    fn events_carry_line_numbers_and_order() {
        let a =
            parse("--- Running: t ---\n  [FAIL] x\nFAILED: t\nTEST_COMPLETE: SOME TESTS FAILED\n");
        let kinds: Vec<EventKind> = a.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [
                EventKind::TestStart,
                EventKind::Assert,
                EventKind::TestEnd,
                EventKind::Marker
            ]
        );
        assert_eq!(a.events[0].line_no, Some(1));
        assert_eq!(a.events[1].line_no, Some(2));
        assert_eq!(a.events[0].seq, 0);
        assert_eq!(a.events[3].seq, 3);
        assert_eq!(a.events[2].text, "FAILED: t");
    }

    #[test]
    fn events_roundtrip_through_jsonl() {
        // events.jsonl lines must round-trip verbatim (regression net for the frozen schema)
        let a = parse("Kernel panic - not syncing: boom\n");
        let line = serde_json::to_string(&a.events[0]).expect("serialize");
        let back: Event = serde_json::from_str(&line).expect("deserialize");
        assert_eq!(back.kind, EventKind::Panic);
        assert_eq!(back.line_no, Some(1));
        assert_eq!(back.extra, serde_json::Value::Null);
    }

    #[test]
    fn normalize_preserves_real_codes() {
        assert_eq!(normalize(Some(0)), 0);
        assert_eq!(normalize(Some(1)), 1);
        assert_eq!(normalize(Some(124)), 124);
        assert_eq!(normalize(Some(137)), 124, "SIGKILL is normalized to timeout semantics");
        assert_eq!(normalize(None), 130);
    }

    #[test]
    fn semantics_labels() {
        assert_eq!(semantics(Some(0)), "ok");
        assert_eq!(semantics(Some(124)), "timeout");
        assert_eq!(semantics(Some(130)), "interrupted");
        assert_eq!(semantics(Some(1)), "error");
        assert_eq!(semantics(None), "unknown");
    }
}
