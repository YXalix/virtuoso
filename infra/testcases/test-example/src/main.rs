//! test-example — example test case: Rust framework entry + C test body
//! (FFI).
//!
//! Convention for new test cases: copy this directory, change this crate's
//! `name`, register Rust tests in `TESTS`, put C test bodies in `c/` (called
//! one by one from the `run_c_tests` entry); assertions go through the
//! `coda` macros (Rust side) or the `coda.h` macros (C side), with counting
//! and printing from the same source. The artifact is a musl static ELF,
//! copied by builder into the rootfs `/tests/` for auto-discovery.

use coda::{run_and_exit, TestCase};

fn basic_functionality() -> bool {
    coda::check!(std::process::id() > 0, "guest process alive (pid > 0)");
    coda::check!(
        core::mem::size_of::<usize>() >= 4,
        "usize is at least 32-bit"
    )
}

fn osrelease_readable() -> bool {
    // init already mounted /proc in the rootfs stage; read the kernel
    // release without forking (replaces the old main.c system("uname -r"))
    match std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        Ok(s) => coda::check!(!s.trim().is_empty(), "kernel release: {}", s.trim()),
        Err(e) => coda::check!(false, "read /proc/sys/kernel/osrelease: {e}"),
    }
}

static TESTS: &[TestCase] = &[
    ("basic_functionality", basic_functionality),
    ("osrelease_readable", osrelease_readable),
];

fn main() {
    run_and_exit(TESTS);
}
