//! BusyBox provisioning (Rust takeover of fetch-busybox.sh).
//! Supply chain = GitHub fetch + local cache reuse (tried in order; a hit
//! is served from cache):
//!   0. Local cache target/build/busybox/bin/busybox-<version>-linux-<arch>
//!   1. BUSYBOX_DL_URL   explicit full asset URL (wget/curl)
//!   2. gh release download (built-in auth, works with private repos)
//!   3. Direct wget/curl (public releases)
//!
//! No build-from-source fallback: a full miss is an error (static binaries
//! are published uniformly by .github/workflows/busybox-release.yml).
//! Every download is validated against the ELF magic number.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use crate::Arch;

use crate::util::Progress;

pub(crate) const DEFAULT_VERSION: &str = "1.36.1";

/// Effective version (BUSYBOX_VERSION / toml [busybox] version, defaults to DEFAULT_VERSION).
pub(crate) fn effective_version(supply: &Supply) -> &str {
    supply.version.as_deref().unwrap_or(DEFAULT_VERSION)
}

/// Provisioning config (BUSYBOX_* env vars take priority, falling back to the virtuoso.toml [busybox] section).
#[derive(Debug, Clone, Default)]
pub(crate) struct Supply {
    pub version: Option<String>,
    pub release_repo: Option<String>,
    pub dl_url: Option<String>,
}

/// Cached binary path: the file name mirrors the release asset name
/// (busybox-<version>-linux-<arch>); version is part of the cache key —
/// switching versions is a cache miss, so a stale binary is never misused.
pub(crate) fn cache_bin(build_dir: &Path, version: &str, arch: Arch) -> PathBuf {
    build_dir
        .join("busybox/bin")
        .join(format!("busybox-{version}-linux-{}", arch.name()))
}

/// Ensures a static BusyBox for the target arch is in place and returns its binary path. Any failure is an Err (build aborts).
pub(crate) fn ensure(
    build_dir: &Path,
    arch: Arch,
    supply: &Supply,
    progress: &mut Progress,
) -> anyhow::Result<PathBuf> {
    let version = effective_version(supply);
    let bin = cache_bin(build_dir, version, arch);
    let asset = format!("busybox-{version}-linux-{}", arch.name());
    let tag = format!("busybox-v{version}");

    if bin.is_file() {
        progress.line(&format!("BusyBox: cached {version} ({})", arch.name()));
        return Ok(bin);
    }
    std::fs::create_dir_all(build_dir.join("busybox/bin"))?;

    // 1) explicit URL
    if let Some(url) = &supply.dl_url {
        if try_wget(url, &bin, &asset, progress) {
            return Ok(bin);
        }
    }
    // 2/3) release download
    let repo = resolve_repo(build_dir, &supply.release_repo);
    if let Some(repo) = repo {
        // 2) gh (when auth is available)
        if crate::util::which("gh") && gh_auth_ok() {
            progress.line(&format!("Fetching {asset} via gh from {repo} ({tag})"));
            let tmp = bin.with_extension("tmp");
            if gh_download_asset(&repo, &tag, &asset, &tmp) && crate::util::is_elf(&tmp) {
                std::fs::rename(&tmp, &bin)?;
                crate::util::set_executable(&bin)?;
                progress.line(&format!("BusyBox: downloaded via gh ({})", arch.name()));
                return Ok(bin);
            }
            let _ = std::fs::remove_file(&tmp);
            progress.line("WARN: gh release download failed, trying plain wget");
        }
        // 3) direct link
        let url = format!("https://github.com/{repo}/releases/download/{tag}/{asset}");
        if try_wget(&url, &bin, &asset, progress) {
            return Ok(bin);
        }
        anyhow::bail!(
            "BusyBox {asset} download failed ({repo} @ {tag}; both gh and direct link missed).\n  \
             no build-from-source fallback: static busybox is supplied uniformly via GitHub Release.\n  \
             - check network/proxy, or use BUSYBOX_DL_URL=<asset-url> to fetch the asset directly\n  \
             - self-publish: push tag busybox-v{version} to trigger .github/workflows/busybox-release.yml"
        );
    }
    anyhow::bail!(
        "BusyBox {asset} has no supply source (cache missed and no release repo configured).\n  \
         no build-from-source fallback: static busybox is supplied uniformly via GitHub Release.\n  \
         - set BUSYBOX_RELEASE_REPO=<owner>/<repo> (env or toml [busybox] release_repo)\n  \
         - or use BUSYBOX_DL_URL=<asset-url> to fetch the asset directly\n  \
         - self-publish: push tag busybox-v{version} to trigger .github/workflows/busybox-release.yml"
    );
}

fn try_wget(url: &str, bin: &Path, asset: &str, progress: &mut Progress) -> bool {
    let tmp = bin.with_extension("tmp");
    progress.line(&format!("Fetching {asset} from {url}"));
    let ok = fetch(url, &tmp) && crate::util::is_elf(&tmp);
    if ok {
        crate::util::set_executable(&tmp).ok();
        let _ = std::fs::rename(&tmp, bin);
        progress.line("BusyBox: downloaded");
        true
    } else {
        let _ = std::fs::remove_file(&tmp);
        false
    }
}

/// Downloads a single gh release asset to `dest` (call only when gh is on
/// PATH with auth working; success = exit 0).
fn gh_download_asset(repo: &str, tag: &str, asset: &str, dest: &Path) -> bool {
    Command::new("gh")
        .args(["release", "download", tag, "-R", repo, "-p", asset, "-O"])
        .arg(dest)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Downloads to `tmp`: wget first, curl fallback when missing (macOS has no wget but ships curl).
pub(crate) fn fetch(url: &str, tmp: &Path) -> bool {
    let (bin, args) = if crate::util::which("wget") {
        ("wget", vec!["-q".to_string(), "-O".to_string()])
    } else {
        ("curl", vec!["-fsSL".to_string(), "-o".to_string()])
    };
    let mut c = Command::new(bin);
    c.args(&args).arg(tmp).arg(url);
    c.status().map(|s| s.success()).unwrap_or(false)
}

/// Applet manifest (drives symlink generation for the busybox tree),
/// resolved in three tiers:
/// 1. `BUSYBOX_APPLETS_FILE` explicit file (for a custom busybox config;
///    content = `busybox --list` output, one applet per line, # comments);
/// 2. `infra/busybox/applets-<version>.txt` — the version manifest frozen
///    into the repo together with the release recipe (defconfig +
///    CONFIG_STATIC=y, CONFIG_TC removed); one file shared by all three
///    arches built with identical config;
/// 3. host can execute the binary directly (Linux, same arch) → live
///    `--list` (fallback).
///
/// Replaces the old `busybox --install` host-execution path: macOS cannot
/// exec a guest Linux ELF; the manifest-driven approach decouples symlink
/// generation from host arch/OS (the frozen invariant "tests must not mask
/// failures" is unaffected — a missing manifest is an explicit error).
pub(crate) fn applet_names(infra_dir: &Path, version: &str, bin: &Path) -> anyhow::Result<Vec<String>> {
    if let Some(f) = std::env::var_os("BUSYBOX_APPLETS_FILE") {
        return read_applet_file(Path::new(&f));
    }
    let frozen = infra_dir
        .join("busybox")
        .join(format!("applets-{version}.txt"));
    if frozen.is_file() {
        return read_applet_file(&frozen);
    }
    if let Ok(out) = Command::new(bin).arg("--list").output() {
        if out.status.success() {
            let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
                .collect();
            if !names.is_empty() {
                return Ok(names);
            }
        }
    }
    anyhow::bail!(
        "cannot determine the BusyBox applet manifest: infra/busybox/applets-{version}.txt missing, \
         and the host cannot execute the binary (cross assembly)\n  \
         provide BUSYBOX_APPLETS_FILE=<file> (`busybox --list` output) or add the manifest file"
    )
}

fn read_applet_file(p: &Path) -> anyhow::Result<Vec<String>> {
    let text =
        std::fs::read_to_string(p).with_context(|| format!("read {} failed", p.display()))?;
    let names: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect();
    anyhow::ensure!(!names.is_empty(), "{} is empty", p.display());
    Ok(names)
}

/// Resolves the release repo: explicit BUSYBOX_RELEASE_REPO, else walk up
/// from `start` to the project root containing `.git` and scan its git
/// remotes for a GitHub one (the GitHub URL of git origin parsed into
/// owner/repo form).
pub(crate) fn resolve_repo(start: &Path, explicit: &Option<String>) -> Option<String> {
    if let Some(repo) = explicit {
        return Some(repo.clone());
    }
    let mut project_root = start.to_path_buf();
    loop {
        if project_root.join(".git").exists() {
            break;
        }
        if !project_root.pop() {
            return None;
        }
    }
    let out = Command::new("git")
        .args(["-C", &project_root.display().to_string(), "remote", "-v"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let _name = it.next()?;
        let url = it.next()?;
        if line.ends_with("(fetch)") && url.contains("github.com") {
            let stripped = url
                .split_once("github.com")
                .map(|(_, rest)| rest.trim_start_matches([':', '/']))
                .unwrap_or(url);
            return Some(stripped.trim_end_matches(".git").to_string());
        }
    }
    None
}

pub(crate) fn gh_auth_ok() -> bool {
    Command::new("gh")
        .arg("auth")
        .arg("status")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
