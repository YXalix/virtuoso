//! 配置诊断呈现（`virtuoso doctor --verbose` 的全量输出）。
//! 从 config 层拆出：config 只管解析与取值，本模块负责打印。

use crate::launcher::NumaTopology;
use crate::Arch;

use crate::config::Config;

/// 类型化配置诊断。
pub fn print_diagnostics(cfg: &Config, arch_override: Option<&str>) {
    println!("[CONFIG] Virtuoso — typed config diagnostics");
    match &cfg.toml_path() {
        Some(t) => println!("  virtuoso.toml: {} (single config surface)", t.display()),
        None => println!("  virtuoso.toml: absent (built-in defaults; a fully commented template ships at the repo root)"),
    }

    // 架构
    let arch_raw = arch_override.map(str::to_string).or_else(|| cfg.arch_str());
    let host = Arch::host_default();
    match arch_raw.as_deref().and_then(Arch::parse).or(host) {
        Some(arch) => {
            let src = if arch_override.is_some() {
                "CLI --arch"
            } else if std::env::var("ARCH")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            {
                "env ARCH"
            } else if cfg.arch_str().is_some() {
                "config"
            } else {
                "host default"
            };
            println!(
                "  arch: {} ({src}; qemu={}, machine={}, console={})",
                arch.name(),
                arch.qemu_bin(),
                arch.machine(),
                arch.console()
            );
            if host.is_some_and(|h| h != arch) {
                println!(
                    "  WARN: cross-compile ARCH={} differs from host ({}), ensure cross-toolchain is available",
                    arch.name(),
                    std::env::consts::ARCH
                );
            }
        }
        None => {
            if let Some(raw) = arch_raw {
                println!("  ERROR: Unsupported ARCH={raw}");
            }
        }
    }

    // 内核路径与镜像
    match cfg.kernel_path() {
        Ok((kp, source)) => {
            println!("  kernel_path: {} ({source})", kp.display());
            if let Some(arch) = cfg.arch() {
                let img = kp.join(arch.kernel_img());
                println!(
                    "  kernel_image: {} — {}",
                    arch.kernel_img(),
                    if img.is_file() {
                        "found"
                    } else {
                        "MISSING (build the kernel first)"
                    }
                );
            }
        }
        Err(e) => println!("  ERROR: {e}"),
    }

    // CPU / NUMA
    let (smp, nodes_raw, mem) = cfg.topo_params();
    match NumaTopology::parse(&smp, &nodes_raw, &mem) {
        Ok(t) => {
            let total = t
                .total_memory()
                .unwrap_or_else(|_| format!("{mem} (unparsed)"));
            println!(
                "  cpu/mem: smp={}, numa_nodes={}, per-node={mem}, total={total}",
                t.smp, t.nodes
            );
        }
        Err(e) => println!("  WARN: {e}"),
    }

    // 超时
    let t = cfg.timeout_raw();
    println!(
        "  timeout: {t}s{}",
        if t == "0" {
            " (virtuoso test will reject 0)"
        } else {
            ""
        }
    );

    // QEMU 二进制
    if let Some(arch) = cfg.arch() {
        let override_q = cfg.qemu_override();
        let found = match &override_q {
            Some(q) => crate::util::which(q),
            None => crate::util::which(arch.qemu_bin()),
        };
        let label = override_q.as_deref().unwrap_or(arch.qemu_bin());
        println!(
            "  qemu: {label} — {}",
            if found {
                "found"
            } else {
                "NOT FOUND (install qemu-system or set qemu=)"
            }
        );
    }

    // 组件
    let mut comps: Vec<String> = Vec::new();
    comps.push(format!(
        "tools_disk={}",
        if cfg.tools_disk_enabled() {
            "on"
        } else {
            "off"
        }
    ));
    comps.push(format!(
        "agent={}",
        if cfg.agent_enabled() { "on" } else { "off" }
    ));
    match cfg.vfio() {
        Some(devices) if !devices.is_empty() => {
            comps.push(format!("vfio=[{}]", devices.join(",")));
        }
        _ => comps.push("vfio=off".into()),
    }
    let (_, nodes, _) = cfg.topo_params();
    comps.push(format!(
        "numa={}",
        if nodes != "1" {
            format!("on (nodes={nodes})")
        } else {
            "off".to_string()
        }
    ));
    match cfg.pmem_size() {
        Some(size) => comps.push(format!("pmem=on (size={size})")),
        None => comps.push("pmem=off".into()),
    }
    println!("  components: {}", comps.join("  "));
    let plan = cfg.component_plan();
    if !plan.runtime.is_empty() {
        println!("  modules (runtime): {}", plan.runtime.join(" "));
    }
    if !plan.boot_extra.is_empty() {
        println!("  modules (boot extra): {}", plan.boot_extra.join(" "));
    }
    println!();
}
