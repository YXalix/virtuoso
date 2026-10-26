//! pmem persistent memory provisioning ([components.pmem]): the DT route (arm64/riscv64
//! direct boot has no ACPI, and QEMU nvdimm's NFIT only becomes visible with an EFI
//! boot, hence the pmem-region device tree node to work around it). Three artifacts
//! land in target/build/pmem/:
//! 1) ram.img — the main memory backend file (memory-backend-file share=on); the
//!    carved-out pmem region is backed by a host file, and guest writes persist to disk;
//! 2) <machine>.dtb — dumpdtb (generated per the current -smp/-m) + an fdtput patch
//!    (a pmem-region@<base> injected into the root node), passed via -dtb to replace
//!    the QEMU-generated version;
//! 3) mem_limit — the cmdline gets `mem=<total memory − pmem region>` appended: QEMU
//!    rebuilds /memory in full per -m (the DTB cannot be shrunk), so the kernel side
//!    must exclude the pmem region from the linear memory model (equivalent to x86
//!    memmap= semantics) before devm_memremap_pages can create ZONE_DEVICE struct
//!    pages (otherwise the sparse subsection already exists → -EEXIST).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use crate::launcher::NumaTopology;
use crate::Arch;

use crate::config::Config;

/// Some only when [components.pmem] enabled (the three artifacts are in the module docs).
pub(crate) fn pmem_opt(
    cfg: &Config,
    arch: Arch,
    topo: &NumaTopology,
) -> anyhow::Result<Option<crate::launcher::PmemSpec>> {
    let Some(size) = cfg.pmem_size() else {
        return Ok(None);
    };
    if arch == Arch::X86_64 {
        anyhow::bail!(
            "[components.pmem] x86_64 is not supported yet (QEMU nvdimm uses ACPI NFIT and needs an EFI boot) — DT arches only (arm64/riscv64)"
        );
    }
    let total_mem = topo.total_memory().map_err(anyhow::Error::msg)?;
    let pmem_bytes = memory_bytes(&size, "size")?;
    let total_bytes = memory_bytes(&total_mem, "-m total memory")?;
    if pmem_bytes >= total_bytes {
        anyhow::bail!(
            "[components.pmem] size ({size}) must be smaller than total memory ({total_mem})"
        );
    }
    let limit_bytes = total_bytes - pmem_bytes;
    let mem_limit = render_memory(limit_bytes);

    let dir = cfg.build_dir.join("pmem");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {} failed", dir.display()))?;
    let ram_backend = dir.join("ram.img");
    ensure_sparse(&ram_backend, total_bytes)?;
    let dtb = patch_pmem_dtb(cfg, arch, topo, &total_mem, &dir, pmem_bytes)?;
    Ok(Some(crate::launcher::PmemSpec::new(
        size,
        mem_limit,
        ram_backend,
        dtb,
    )))
}

/// Bytes → "768M" (when evenly divisible by MiB) / bare number (both QEMU and the
/// kernel parse mem= as bytes).
fn render_memory(bytes: u64) -> String {
    if bytes != 0 && bytes.is_multiple_of(1024 * 1024) {
        format!("{}M", bytes / (1024 * 1024))
    } else {
        bytes.to_string()
    }
}

/// "256M"/bare number → bytes (QEMU semantics: bare number = bytes).
fn memory_bytes(s: &str, what: &str) -> anyhow::Result<u64> {
    let (n, unit) = crate::util::parse_memory(s)
        .map_err(|e| anyhow::anyhow!("[components.pmem] invalid {what}: {e}"))?;
    Ok(match unit {
        crate::util::MemUnit::Bare => n,
        _ => unit.to_mib(n) * 1024 * 1024,
    })
}

/// Recreate (sparse) only when the file is missing or its size mismatches; existing
/// content is kept — guest writes persist across runs.
fn ensure_sparse(path: &Path, bytes: u64) -> anyhow::Result<()> {
    let mismatch = std::fs::metadata(path)
        .map(|m| m.len() != bytes)
        .unwrap_or(true);
    if mismatch {
        std::fs::File::create(path)
            .with_context(|| format!("create {} failed", path.display()))?
            .set_len(bytes)
            .with_context(|| format!("failed to size {} to {bytes} bytes", path.display()))?;
    }
    Ok(())
}

/// 32-bit DT cell sequence (high word first) → hex argument list for fdtput -t x.
fn dt_cells(v: u64, cells: u64) -> Vec<String> {
    (0..cells)
        .rev()
        .map(|i| format!("{:#x}", (v >> (32 * i)) & 0xffff_ffff))
        .collect()
}

fn fdtget_words(dtb: &Path, node: &str, prop: &str) -> anyhow::Result<Vec<u64>> {
    let out = Command::new("fdtget")
        .args(["-t", "x"])
        .arg(dtb)
        .arg(node)
        .arg(prop)
        .output()
        .with_context(|| {
            format!("failed to run fdtget {node} {prop} (host needs the dtc package)")
        })?;
    anyhow::ensure!(
        out.status.success(),
        "fdtget {node} {prop} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(|w| u64::from_str_radix(w.trim_start_matches("0x"), 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("failed to parse fdtget {node} {prop} output: {e}"))
}

fn fdtput(dtb: &Path, args: &[String]) -> anyhow::Result<()> {
    let out = Command::new("fdtput")
        .arg(dtb)
        .args(args)
        .output()
        .with_context(|| format!("failed to run fdtput {args:?} (host needs the dtc package)"))?;
    anyhow::ensure!(
        out.status.success(),
        "fdtput {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Generate the QEMU device tree with dumpdtb and apply the pmem patch: inject
/// pmem-region@<base> into the root node (bound by of_pmem → no ndctl /dev/pmem0
/// needed). Excluding the region from linear memory is done via the cmdline `mem=`
/// (with -dtb, QEMU rebuilds /memory in full per -m; shrinking the DTB has no effect).
fn patch_pmem_dtb(
    cfg: &Config,
    arch: Arch,
    topo: &NumaTopology,
    total_mem: &str,
    dir: &Path,
    pmem_bytes: u64,
) -> anyhow::Result<PathBuf> {
    let machine = arch.machine();
    let dtb = dir.join(format!("{machine}.dtb"));
    let qemu = cfg
        .qemu_override()
        .unwrap_or_else(|| arch.qemu_bin().to_string());
    let mut cmd = Command::new(&qemu);
    cmd.arg("-machine")
        .arg(format!("{machine},dumpdtb={}", dtb.display()));
    cmd.args([
        "-smp",
        &topo.smp.to_string(),
        "-m",
        total_mem,
        "-display",
        "none",
    ]);
    if arch == Arch::Riscv64 {
        cmd.arg("-bios").arg("none");
    }
    let out = cmd
        .output()
        .with_context(|| format!("failed to run {qemu} dumpdtb"))?;
    anyhow::ensure!(
        out.status.success() && dtb.is_file(),
        "dumpdtb failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );

    // Root node cell widths (QEMU virt uses #address-cells=2 / #size-cells=2)
    let read_cells = |prop: &str| -> anyhow::Result<u64> {
        Ok(fdtget_words(&dtb, "/", prop)?.first().copied().unwrap_or(2))
    };
    let addr_cells = read_cells("#address-cells")?;
    let size_cells = read_cells("#size-cells")?;

    // The pmem region sits at the top of RAM: dumpdtb's /memory reg gives the RAM
    // base/size (generation args match boot; QEMU's /memory rewrite at boot is
    // unrelated to the patched node)
    let reg = fdtget_words(&dtb, "/memory", "reg")?;
    anyhow::ensure!(
        reg.len() == (addr_cells + size_cells) as usize,
        "/memory reg has an unexpected shape ({} cells, expected {})",
        reg.len(),
        addr_cells + size_cells
    );
    let join = |words: &[u64]| -> u64 {
        words
            .iter()
            .fold(0u64, |acc, w| (acc << 32) | (w & 0xffff_ffff))
    };
    let base = join(&reg[..addr_cells as usize]);
    let total_bytes = join(&reg[addr_cells as usize..]);
    anyhow::ensure!(
        total_bytes == memory_bytes(total_mem, "-m total memory")?,
        "/memory size ({total_bytes:#x}) does not match -m ({total_mem})"
    );
    let pmem_base = base + total_bytes - pmem_bytes;

    // Root node pmem-region platform device (bound by of_pmem)
    let node = format!("/pmem@{pmem_base:x}");
    fdtput(&dtb, &["-c".to_string(), node.clone()])?;
    fdtput(
        &dtb,
        &[
            "-t".to_string(),
            "s".to_string(),
            node.clone(),
            "compatible".to_string(),
            "pmem-region".to_string(),
        ],
    )?;
    let mut prop_args = vec![
        "-t".to_string(),
        "x".to_string(),
        node.clone(),
        "reg".to_string(),
    ];
    prop_args.extend(dt_cells(pmem_base, addr_cells));
    prop_args.extend(dt_cells(pmem_bytes, size_cells));
    fdtput(&dtb, &prop_args)?;
    Ok(dtb)
}
