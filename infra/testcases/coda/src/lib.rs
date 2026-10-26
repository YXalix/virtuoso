//! coda — the Virtuoso test framework (std + musl static linking).
//!
//! The name is the concluding passage of a movement: test cases are the final
//! verdict of the whole build. Semantics align one-to-one with the C-side
//! `PASS/FAIL/SKIP/INFO` macros in `coda.h`: marker protocol v1 (frozen
//! text) treats C and Rust test cases alike — same serial protocol, same
//! judging. The Rust entry point (`run_and_exit`) and the C test bodies (the
//! FFI-exported `coda_pass` and friends) share the same counters and
//! printing path, a single source for the frozen text.
//!
//! Static linking constraint: crt-static is rustc's default for musl targets
//! (same recipe as the tools workspace); the artifact is a pure static ELF —
//! the VM has no dynamic loader, **do not relax this**.
//! Output goes to std stdout (the console is a tty, flushed per line); exit
//! goes through `process::exit`.
//!
//! `run_and_exit` unconditionally references the C section entry
//! `run_c_tests` — the symbol is guaranteed by coda-scaffold (sources in
//! `c/` compile in the real body; an empty `c/` compiles in a stub), so test
//! case crates never declare `extern "C"` themselves.

use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU32, Ordering};

/// One test: returning true = passed. Assertion details are printed straight
/// to the serial console by the macros.
pub type TestCase = (&'static str, fn() -> bool);

/// Assertion counters (single-threaded process; the Atomics only serve the
/// static lifetimes of the main FFI boundary).
static PASSED: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);
static SKIPPED: AtomicU32 = AtomicU32::new(0);

// C test section entry (defined by the test case crate's `c/*.c`; with no C
// sources, coda-scaffold compiles in a stub to hold the symbol).
extern "C" {
    fn run_c_tests();
}

// ---------------------------------------------------------------- serial output

/// Marker protocol v1 assertion line: two-space indent + `[TAG] message`
/// (same shape as the C macro output; the judge trims leading whitespace
/// when parsing).
pub fn emit(tag: &str, args: std::fmt::Arguments<'_>) {
    println!("  [{tag}] {args}");
}

// ---------------------------------------------------------------- protocol macros

#[macro_export]
macro_rules! pass {
    ($($arg:tt)*) => { $crate::emit("PASS", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! fail {
    ($($arg:tt)*) => { $crate::emit("FAIL", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! skip {
    ($($arg:tt)*) => { $crate::emit("SKIP", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::emit("INFO", format_args!($($arg)*)) };
}

/// Assertion: on failure prints [FAIL] and returns false (later assertions in
/// the same test still run, matching the counting semantics of the C-side
/// PASS/FAIL macros).
#[macro_export]
macro_rules! check {
    ($cond:expr, $($arg:tt)*) => {
        if $cond {
            $crate::pass!($($arg)*);
            true
        } else {
            $crate::fail!($($arg)*);
            false
        }
    };
}

// ---------------------------------------------------------------- FFI exports (sole entry for C test bodies)

/// C-side message pointer to a safe string (NULL tolerated as empty).
fn c_msg(msg: *const c_char) -> String {
    if msg.is_null() {
        return String::new();
    }
    // The C side should send valid UTF-8 (snprintf output); replace bad bytes
    // rather than invoking UB/panic.
    unsafe { CStr::from_ptr(msg) }
        .to_string_lossy()
        .trim_end()
        .to_string()
}

/// Shared FFI counting + printing path. Counters increment before printing,
/// so the `[TAG]` lines the judge sees stay strongly consistent with the
/// internal counts.
macro_rules! ffi_emit {
    ($name:ident, $tag:literal, $counter:ident) => {
        #[doc = concat!("`", $tag, "` assertion line (landing spot for the C-side `", stringify!($name), "`).")]
        #[no_mangle]
        pub extern "C" fn $name(msg: *const c_char) {
            $counter.fetch_add(1, Ordering::Relaxed);
            emit($tag, format_args!("{}", c_msg(msg)));
        }
    };
}

ffi_emit!(coda_pass, "PASS", PASSED);
ffi_emit!(coda_fail, "FAIL", FAILED);
ffi_emit!(coda_skip, "SKIP", SKIPPED);

/// `INFO` line (printed only, never counted; matches the C macro semantics).
#[no_mangle]
pub extern "C" fn coda_info(msg: *const c_char) {
    emit("INFO", format_args!("{}", c_msg(msg)));
}

/// Read-only counter snapshots (landing spot for the C-side `coda_count_*`;
/// `unsigned` = u32).
#[no_mangle]
pub extern "C" fn coda_count_passed() -> u32 {
    PASSED.load(Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn coda_count_failed() -> u32 {
    FAILED.load(Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn coda_count_skipped() -> u32 {
    SKIPPED.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------- runner

/// Shared main equivalent: panic hook + banner, then C test section, then
/// Rust test section, then exit by counters. A test case's main only needs
/// to call this one function.
/// `Test Results` / `TEST_COMPLETE` are assembled by init (protocol v1
/// division of labor).
pub fn run_and_exit(tests: &[TestCase]) -> ! {
    install_panic_hook();
    println!("=== Virtuoso E2E Tests ===");
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_string();
    if !kernel.is_empty() {
        println!("Kernel: {kernel}");
    }

    // Run the C section first (its counts land in this framework), then the
    // Rust section; the exit code follows the totals
    unsafe { run_c_tests() };
    let (passed, failed, skipped) = counters();
    info!("C section: passed={passed} failed={failed} skipped={skipped}");

    for (name, f) in tests {
        info!("Test: {name}");
        let before = FAILED.load(Ordering::Relaxed);
        let ok = f();
        if !ok && FAILED.load(Ordering::Relaxed) == before {
            // The function returned false but printed no [FAIL]: emit one so
            // the judging never misses a line
            fail!("{name}: assertion failed without [FAIL] line");
        }
    }
    exit_by_counters()
}

/// Exit code from the counters: any FAIL > 0 means 1, otherwise 0 (init
/// prints PASSED/FAILED based on it).
pub fn exit_by_counters() -> ! {
    let code = if FAILED.load(Ordering::Relaxed) > 0 { 1 } else { 0 };
    std::process::exit(code)
}

/// Snapshot of the assertion counters (for test case mains printing a
/// free-form summary).
pub fn counters() -> (u32, u32, u32) {
    (
        PASSED.load(Ordering::Relaxed),
        FAILED.load(Ordering::Relaxed),
        SKIPPED.load(Ordering::Relaxed),
    )
}

/// Panic handling: prints [FAIL] then aborts (nonzero exit). The profile is
/// `panic = "abort"` and the hook runs before the abort — a panic is a test
/// case failure, never a false pass.
/// `run_and_exit` installs it automatically; call it manually only when used
/// standalone.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()));
        let loc = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        match payload {
            Some(s) => emit("FAIL", format_args!("panic: {s}{loc}")),
            None => emit("FAIL", format_args!("panic:{loc}")),
        }
    }));
}
