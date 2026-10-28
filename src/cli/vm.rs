//! VM session commands: shell / test.
//! The launch DSL, process governance, and watchdog live in launcher; verdict/judging in judge.
//! This module wires: build (builder) → launch (launcher) → timeout reaping →
//! verdict and artifacts (judge + runs).

use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};
use std::time::Instant;

use anyhow::Context;
use crate::launcher::Accel;
use crate::{Arch, HostOs};

use super::{build_invocation, open_agent_socket, resolve_arch, resolve_topology, LaunchPlan};
use crate::config::Config;
use crate::runs;

/// CLI accel resolution: `--kvm` / `--tcg` are explicit; the default follows platform
/// rules (HVF on same-arch macOS, TCG elsewhere). `--kvm --tcg` are mutually exclusive.
pub(crate) fn resolve_accel(kvm: bool, tcg: bool, arch: Arch) -> anyhow::Result<Accel> {
    if kvm && tcg {
        anyhow::bail!("--kvm and --tcg are mutually exclusive");
    }
    Ok(if kvm {
        Accel::Kvm
    } else if tcg {
        Accel::Tcg
    } else {
        Accel::default_for(arch, HostOs::current(), Arch::host_default())
    })
}

pub fn run_shell(kvm: bool, tcg: bool, gdb: bool) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let arch = resolve_arch(&cfg, None)?;
    if gdb && kvm {
        anyhow::bail!("--gdb and --kvm are mutually exclusive (single-stepping needs TCG)");
    }
    // --gdb: suspend waiting for a GDB connection, always TCG (reliable
    // single-stepping); others follow the platform accel rules
    let accel = if gdb {
        Accel::Tcg
    } else {
        resolve_accel(kvm, tcg, arch)?
    };
    run_vm_session(accel, gdb)
}

/// Interactive session (shell, including the --gdb debug suspend): stdio inherited,
/// no run artifacts produced.
fn run_vm_session(accel: Accel, gdb_stub: bool) -> anyhow::Result<i32> {
    let cfg = Config::load()?;
    let arch = resolve_arch(&cfg, None)?;
    let topo = resolve_topology(&cfg)?;
    if gdb_stub {
        println!("Starting QEMU with GDB stub on port 1234...");
    }
    match accel {
        Accel::Kvm => println!("Starting QEMU with KVM acceleration..."),
        Accel::Hvf => println!("Starting QEMU with HVF acceleration..."),
        Accel::Tcg => {
            if !gdb_stub {
                println!("Starting QEMU...");
            }
        }
    }

    let inv = build_invocation(&LaunchPlan {
        cfg: &cfg,
        arch,
        topo: topo.clone(),
        accel,
        auto_test: false,
        only_tests: Vec::new(),
        gdb_stub,
        agent_socket: open_agent_socket(&cfg, &cfg.target_dir, "agent-shell"),
    })?;

    println!("[LAUNCH] {}", inv.command_line()?);
    println!("Serial console attached — quit with Ctrl-A then X");
    let (mut child, mut sup) = inv.spawn_supervised(false)?;
    let st = child.wait().context("failed to wait for QEMU exit")?;
    sup.finish();
    Ok(crate::judge::normalize(st.code()))
}

/// CI mode: build → launch (launcher) → timeout watchdog (KILL reaping, 124) →
/// judge verdict → run artifacts. Exit codes: 0=pass, 124=timeout, otherwise failure.
/// With `replay_until_fail > 1`, replay retries: stop at the first non-passed verdict.
/// accel defaults to platform rules (HVF on same-arch macOS, always TCG on Linux);
/// `--tcg` forces pure emulation.
/// When `only` is non-empty, the cmdline gets virtuoso.only= appended and init runs
/// only the listed /tests binaries.
pub fn run_test(
    cli_timeout: Option<u64>,
    cli_arch: Option<&str>,
    replay_until_fail: Option<u32>,
    tcg: bool,
    only: &[String],
) -> anyhow::Result<i32> {
    validate_only(only)?;
    let cfg = Config::load()?;
    let timeout_secs = resolve_timeout(&cfg, cli_timeout)?;
    let rounds = replay_until_fail.unwrap_or(1).max(1);
    let mut last_code = 1;
    for round in 1..=rounds {
        if rounds > 1 {
            println!("[REPLAY] round {round}/{rounds}");
        }
        let accel = if tcg { Some(Accel::Tcg) } else { None };
        let (code, _) = test_once(&cfg, cli_arch, timeout_secs, accel, only)?;
        last_code = code;
        if last_code != 0 {
            if rounds > 1 {
                eprintln!("[REPLAY] aborted at round {round}/{rounds} (verdict not passed)");
            }
            return Ok(last_code);
        }
    }
    if rounds > 1 {
        println!("[REPLAY] {rounds}/{rounds} rounds passed — no flaky observed");
    }
    Ok(last_code)
}

/// `--only` list validation: an entry is a test case name (flat basename or
/// `group/case`) or a group selection (trailing slash `group/`) — non-empty, no
/// commas/whitespace (a comma is the init-side list separator, and whitespace would
/// break cmdline tokens).
fn validate_only(only: &[String]) -> anyhow::Result<()> {
    for name in only {
        if name.trim().is_empty() || name.contains(|c: char| c.is_whitespace() || c == ',') {
            anyhow::bail!(
                "invalid --only entry {name:?}: expected a test name (basename or \
                 group/case) or a group selection (group/); no commas/whitespace"
            );
        }
    }
    Ok(())
}

fn resolve_timeout(cfg: &Config, cli_timeout: Option<u64>) -> anyhow::Result<u64> {
    let raw = cli_timeout
        .map(|s| s.to_string())
        .unwrap_or_else(|| cfg.timeout_raw());
    if raw.trim() == "0" {
        anyhow::bail!("Set QEMU_TIMEOUT (e.g., virtuoso test --timeout 60)");
    }
    raw.trim().parse().context("QEMU_TIMEOUT must be a number")
}

/// One complete test run; returns (exit code, verdict string).
/// When `accel_override` is None, follow platform rules (--tcg semantics are folded
/// in by the caller).
fn test_once(
    cfg: &Config,
    cli_arch: Option<&str>,
    timeout_secs: u64,
    accel_override: Option<Accel>,
    only: &[String],
) -> anyhow::Result<(i32, String)> {
    let arch = resolve_arch(cfg, cli_arch)?;
    let accel = match accel_override {
        Some(a) => a,
        None => resolve_accel(false, false, arch)?,
    };
    let topo = resolve_topology(cfg)?;
    let run = runs::create_run_dir(&cfg.project_root, arch.name())?;
    println!("[RUN] artifacts: {}", run.path.display());

    // ---- build (builder) ----
    let build_started = Instant::now();
    if let Err(e) = super::build::build_pair_for(cfg, Some(&run.path.join("build.log")), &[]) {
        let meta = build_failed_meta(
            &run,
            arch,
            timeout_secs,
            build_started.elapsed().as_millis() as u64,
        );
        runs::finalize_run(&run, &meta)?;
        runs::prune(&cfg.project_root, runs::RUNS_KEEP);
        // The single ERROR print site is in main (the bail carries the context and artifact paths)
        return Err(e.context(format!(
            "initrd build failed; artifacts: {}",
            run.path.display()
        )));
    }

    // ---- launch (launcher; verdict in judge) ----
    let started = Instant::now();
    let inv = build_invocation(&LaunchPlan {
        cfg,
        arch,
        topo: topo.clone(),
        accel,
        auto_test: cfg.auto_test(),
        only_tests: only.to_vec(),
        gdb_stub: false,
        agent_socket: open_agent_socket(cfg, &run.path, "agent"),
    })?;
    println!("[LAUNCH] {}", inv.command_line()?);
    println!("Running QEMU test with {timeout_secs}s timeout...");
    let (mut child, mut sup) = inv.spawn_supervised(true)?;

    // Wall-clock watchdog: KILLs the process group when due (equivalent to
    // timeout --signal=KILL's 124 semantics)
    let timed_out = Arc::new(AtomicBool::new(false));
    let watchdog =
        crate::launcher::spawn_watchdog(sup.pgid(), timeout_secs, Arc::clone(&timed_out));

    let pumped = runs::pump_child(
        &mut child,
        &run.path.join("serial.log"),
        &run.path.join("qemu-stderr.log"),
    )?;
    let status = pumped.status;
    let duration_ms = started.elapsed().as_millis() as u64;
    let timed_out_now = timed_out.load(Ordering::SeqCst);
    timed_out.store(true, Ordering::SeqCst);
    let _ = watchdog.join();

    // Exit codes: timeout 124; signal kills normalized (137→124, signal death→130);
    // others keep the real code.
    // The canonical semantics table lives in crate::judge.
    let code = if timed_out_now {
        crate::judge::EXIT_TIMEOUT
    } else {
        let code = crate::judge::normalize(status.code());
        if status.code().is_none() {
            sup.kill_now();
        }
        code
    };
    sup.finish();

    // QEMU's own output and the guest serial are presented separately: on failure/timeout,
    // show the stderr tail (the scene of an early QEMU death or a rejected argument);
    // on success, stay quiet.
    pumped.report_tail_on_failure(code);

    // ---- verdict and artifacts (judge + runs) ----
    let meta = runs::RunMeta {
        run_id: run.id.clone(),
        arch: arch.name().to_string(),
        exit_code: Some(code),
        duration_ms,
        timeout_s: timeout_secs.to_string(),
        kernel: Some(super::kernel_image_path(cfg, arch)?),
        qemu_version: crate::launcher::qemu_version(arch),
        topo: serde_json::json!({
            "smp": topo.smp.to_string(),
            "numa_nodes": topo.nodes.to_string(),
            "memory_per_node": topo.memory_per_node,
            "accel": accel.label(),
            "auto_test": cfg.auto_test().to_string(),
        }),
        build_failed: false,
    };
    let verdict = runs::finalize_run(&run, &meta)?;
    runs::prune(&cfg.project_root, runs::RUNS_KEEP);

    // Exit code reconciliation (frozen contract 0=pass): QEMU always exits 0 on clean
    // shutdown; an in-guest failure only shows up in the verdict (marker protocol) —
    // non-passed folds to 1; timeout 124 / interrupt 130 are kept as-is. exit_code in
    // verdict.json remains QEMU's raw code (audit fact).
    let code = if code == 0 && verdict != "passed" { 1 } else { code };

    match code {
        0 => println!("Test completed successfully!"),
        124 => eprintln!("ERROR: Test timed out after {timeout_secs} seconds"),
        _ => {}
    }
    println!(
        "[RUN] verdict: {verdict} (exit {code}, {:.1}s) — verdict.json: {}",
        duration_ms as f64 / 1000.0,
        run.path.join("verdict.json").display()
    );
    Ok((code, verdict))
}

/// Verdict metadata persisted when the build fails (recorded with build_failed, exit code 1).
fn build_failed_meta(
    run: &runs::RunDir,
    arch: Arch,
    timeout_secs: u64,
    duration_ms: u64,
) -> runs::RunMeta {
    runs::RunMeta {
        run_id: run.id.clone(),
        arch: arch.name().to_string(),
        exit_code: Some(1),
        duration_ms,
        timeout_s: timeout_secs.to_string(),
        kernel: None,
        qemu_version: None,
        topo: serde_json::Value::Null,
        build_failed: true,
    }
}
