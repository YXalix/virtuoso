//! Test case build script: all generic implementation lives in coda-scaffold
//! (CC probing, c/*.c compilation, empty stub, per-crate
//! compile_commands.json). Every test case's build.rs is this one line.

fn main() {
    coda_scaffold::run()
}
