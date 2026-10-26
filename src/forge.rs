//! forge — kernel forging: containerized kernel source provisioning (named volume +
//! pinned toolchain image).
//!
//! Absorbs `devkit/docker/kernel.sh` (the thin shell is retired; the CI-published
//! Dockerfile.kernel stays in place): the source of truth lives in a named volume
//! (container-side ext4: case-sensitive + build performance), read and written directly
//! by the host through a platform view — macOS = OrbStack view, Linux = the volume
//! itself. The `virtuoso kernel` command group projects onto this module:
//! clone/defconfig/build/shell + list/use (volume management and current switching;
//! active volume state lands in repo-root `.virtuoso/kernel-current.json`). Source
//! editing goes through the VS Code devcontainer (in-container clangd consumes the
//! /ksrc raw-form compile_commands.json produced by build).
//!
//! The test main loop runs natively on the host and follows the active volume
//! automatically (config resolution order: env KERNEL_PATH > current active volume >
//! toml kernel_path), so `virtuoso doctor / build / test` need no manual path.
//!
//! Sections: active volume state (current) → volume host view and engine guard →
//! toolchain image container calls → clone pipeline → devcontainer rendering.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::util::quote as shell_quote;
use crate::util::Progress;

use crate::Arch;

/// Default volume name (each volume carries its own source + .config + incremental
/// artifacts, so switching back needs no rebuild).
pub(crate) const DEFAULT_VOLUME: &str = "virtuoso-kernel";
/// Pinned toolchain image (published to ghcr by kernel-builder.yml; on pull failure
/// falls back to a local build of devkit/docker/Dockerfile.kernel).
pub(crate) const DEFAULT_IMAGE: &str = "ghcr.io/yxalix/virtuoso-kernel:latest";
/// Default clone ref.
pub(crate) const DEFAULT_REF: &str = "master";

// ---------------------------------------------------------------- active volume state (current)

// current stays out of `virtuoso.toml` (the single config surface is reserved for
// persistent launch configuration); a dedicated file records "which volume is current" —
// machine-local session state, git-ignored; written by `kernel use/clone`, read by
// `path/list` and doctor. The sole entry point writing current, [`write_current`], also
// renders `.devcontainer/devcontainer.json` in sync (the devcontainer follows the active
// volume). State file: `.virtuoso/kernel-current.json`.

/// Current active volume (volume + target architecture).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Current {
    pub volume: String,
    pub arch: String,
}

/// State file path (repo-root `.virtuoso/kernel-current.json`).
pub(crate) fn state_path(project_root: &Path) -> PathBuf {
    project_root.join(".virtuoso").join("kernel-current.json")
}

/// Missing = no active volume (raw/preset mode; doctor stays silent); present but
/// invalid = error.
pub(crate) fn read_current(project_root: &Path) -> anyhow::Result<Option<Current>> {
    let p = state_path(project_root);
    if !p.is_file() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&p).with_context(|| format!("read {} failed", p.display()))?;
    serde_json::from_str(&text).map(Some).with_context(|| {
        format!(
            "{} corrupted (expected {{volume, arch}}); repair with `virtuoso kernel use <volume>`",
            p.display()
        )
    })
}

/// Writes the file (creates the directory if missing; the parent `.virtuoso/` is already
/// git-ignored). Also renders `.devcontainer/devcontainer.json` in sync — every change
/// to current must be followed there; keeping it in the sole write entry point prevents
/// future writers from missing the hook.
pub(crate) fn write_current(project_root: &Path, current: &Current) -> anyhow::Result<()> {
    let p = state_path(project_root);
    std::fs::create_dir_all(p.parent().expect("state file must have a parent directory"))?;
    std::fs::write(&p, format!("{}\n", serde_json::to_string_pretty(current)?))?;
    render(project_root, current)?;
    Ok(())
}

// ---------------------------------------------------------------- volume host view and engine guard

// The source of truth lives in a named volume; the host reads and writes it directly
// through a platform view: macOS goes through the OrbStack view (Docker Desktop volumes
// live on the VM's virtual disk, invisible to the host); on Linux the volume itself sits
// on the host filesystem, so the engine's authoritative Mountpoint is used (rootless
// lands under $HOME, no root needed). On both platforms the path is a pure host path —
// the test main loop's kernel_path (QEMU consumes the kernel image from it) points
// there; source editing goes through the in-container devcontainer, never the host view.

/// OrbStack's host view of a named volume (exists when OrbStack is the runtime).
pub(crate) fn orbstack_view(home: &Path, volume: &str) -> PathBuf {
    home.join("OrbStack")
        .join("docker")
        .join("volumes")
        .join(volume)
}

/// Host-visible path of a volume.
pub(crate) fn host_view(volume: &str) -> anyhow::Result<PathBuf> {
    match std::env::consts::OS {
        "macos" => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is not set, cannot locate the OrbStack volume view")?;
            Ok(orbstack_view(&home, volume))
        }
        "linux" => {
            let out = Command::new("docker")
                .args(["volume", "inspect", volume, "--format", "{{ .Mountpoint }}"])
                .output()
                .context("failed to run docker volume inspect (is docker present?)")?;
            anyhow::ensure!(
                out.status.success(),
                "volume {volume} does not exist (run `virtuoso kernel clone <git-url>` first): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            let mp = String::from_utf8_lossy(&out.stdout).trim().to_string();
            anyhow::ensure!(!mp.is_empty(), "volume {volume} has an empty Mountpoint");
            Ok(PathBuf::from(mp))
        }
        os => anyhow::bail!(
            "unsupported host platform {os} (docker kernel provisioning supports macOS/Linux)"
        ),
    }
}

/// macOS engine guard: the host view of a volume is provided by OrbStack, so the docker
/// endpoint must point at the OrbStack engine; otherwise clone/build create the volume in
/// another engine's VM where the view can never see it. Always passes on non-macOS (on
/// Linux the volume itself is a host directory; no view-engine mismatch).
pub(crate) fn engine_guard() -> anyhow::Result<()> {
    if std::env::consts::OS != "macos" {
        return Ok(());
    }
    let ep = match std::env::var("DOCKER_HOST") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => docker_context_endpoint()?,
    };
    if endpoint_is_orbstack(&ep) {
        return Ok(());
    }
    anyhow::bail!(
        "the current docker engine on macOS is not OrbStack (endpoint: {ep}).\n  Run 'docker context use orbstack', or for a single command export DOCKER_HOST=unix://$HOME/.orbstack/run/docker.sock"
    )
}

/// Whether the engine endpoint points at OrbStack (unix sockets are `$HOME/.orbstack/...`;
/// remote context hostnames usually contain "orbstack" too — lenient matching prefers
/// letting traffic through, since a false block costs the user a stuck guard).
fn endpoint_is_orbstack(endpoint: &str) -> bool {
    endpoint.contains("orbstack")
}

/// Engine endpoint of the current docker context (authoritative when DOCKER_HOST is unset).
fn docker_context_endpoint() -> anyhow::Result<String> {
    let out = Command::new("docker")
        .args(["context", "show"])
        .output()
        .map_err(|_| {
            anyhow::anyhow!("docker unavailable (install and start OrbStack, then retry)")
        })?;
    anyhow::ensure!(
        out.status.success(),
        "docker context show failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    anyhow::ensure!(
        !name.is_empty(),
        "docker context is empty (install and start OrbStack, then retry)"
    );
    let out = Command::new("docker")
        .args([
            "context",
            "inspect",
            "--format",
            "{{ .Endpoints.docker.Host }}",
            &name,
        ])
        .output()
        .context("failed to run docker context inspect")?;
    anyhow::ensure!(
        out.status.success(),
        "docker context inspect {name} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// All volume names in the engine (`docker volume ls`; the engine is the authoritative
/// registry, no separate manifest is kept).
pub(crate) fn list() -> anyhow::Result<Vec<String>> {
    let out = Command::new("docker")
        .args(["volume", "ls", "--format", "{{ .Name }}"])
        .output()
        .context("failed to run docker volume ls (is docker present?)")?;
    anyhow::ensure!(
        out.status.success(),
        "docker volume ls failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Whether a volume exists (engine-authoritative).
pub(crate) fn exists(volume: &str) -> bool {
    Command::new("docker")
        .args(["volume", "inspect", volume])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Volume content state (fs-only probe of the host view; no container is started).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// Empty / unreachable
    Empty,
    /// Source tree present (Makefile), not configured (no .config)
    Cloned,
    /// Makefile + .config present, ready to build
    Configured,
}

impl Status {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Status::Empty => "empty",
            Status::Cloned => "cloned",
            Status::Configured => "configured",
        }
    }
}

/// Probes volume contents via the host view: Makefile+.config = configured,
/// Makefile only = cloned.
pub(crate) fn status(view: &Path) -> Status {
    match (
        view.join("Makefile").is_file(),
        view.join(".config").is_file(),
    ) {
        (true, true) => Status::Configured,
        (true, false) => Status::Cloned,
        _ => Status::Empty,
    }
}

// ---------------------------------------------------------------- toolchain image container calls

// Image provisioning (pull, falling back to a local Dockerfile.kernel build), a generic
// docker run wrapper, and the arch → make env / clangd triple / make image target
// mappings (verbatim from kernel.sh).

/// Toolchain image override env. Must not be named `KERNEL_IMAGE` — that name is already
/// taken by the CLI config surface (launch kernel image override, config.rs).
pub(crate) const IMAGE_ENV: &str = "KERNEL_TOOLCHAIN_IMAGE";

/// Toolchain image resolution: env `KERNEL_TOOLCHAIN_IMAGE` > ghcr-published image.
/// The devcontainer rendering shares this source (keeping edit and build containers
/// on the same image).
pub(crate) fn resolve_image() -> String {
    std::env::var(IMAGE_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_IMAGE.to_string())
}

/// Whether the toolchain image is present (engine-authoritative).
pub(crate) fn image_present(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Image provisioning: short-circuits when present; a pull failure falls back to building
/// devkit/docker/Dockerfile.kernel locally (same pinned toolchain as the ghcr publication
/// in kernel-builder.yml).
pub(crate) fn ensure_image(
    image: &str,
    dockerfile_dir: &Path,
    progress: &mut Progress,
) -> anyhow::Result<()> {
    engine_guard()?;
    if image_present(image) {
        return Ok(());
    }
    progress.line(&format!("Kernel toolchain: pulling {image} ..."));
    let pulled = Command::new("docker")
        .arg("pull")
        .arg(image)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if pulled {
        return Ok(());
    }
    let dockerfile = dockerfile_dir.join("Dockerfile.kernel");
    progress.line(&format!(
        "Kernel toolchain: pull failed, building locally from {} ...",
        dockerfile.display()
    ));
    let st = Command::new("docker")
        .arg("build")
        .arg("-t")
        .arg(image)
        .arg("-f")
        .arg(&dockerfile)
        .arg(dockerfile_dir)
        .status()
        .context("failed to run docker build")?;
    anyhow::ensure!(
        st.success(),
        "local toolchain-image build failed ({dockerfile:?})"
    );
    Ok(())
}

/// Generic container call base: volume mounted at /ksrc (workspace), workdir /ksrc.
fn base_cmd(volume: &str, image: &str, tty: bool) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "-i"]);
    if tty {
        cmd.arg("-t");
    }
    cmd.arg("--entrypoint=")
        .arg("-v")
        .arg(format!("{volume}:/ksrc"));
    cmd.arg("-w").arg("/ksrc").arg(image);
    cmd
}

/// Runs an in-container script with streaming output (stdio inherited, so long kernel
/// build output stays visible in real time).
pub(crate) fn run_streaming(
    volume: &str,
    image: &str,
    envs: &[(&str, &str)],
    script: &str,
) -> anyhow::Result<()> {
    let mut cmd = base_cmd(volume, image, false);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.arg("sh").arg("-c").arg(script);
    let st = cmd
        .status()
        .with_context(|| format!("failed to run docker (is the engine up? volume {volume})"))?;
    anyhow::ensure!(
        st.success(),
        "in-container command failed (exit {}): {script}",
        st.code().unwrap_or(-1)
    );
    Ok(())
}

/// Interactive container command (shell: TTY + stdio inherited); returns the exit code.
pub(crate) fn run_tty(
    volume: &str,
    image: &str,
    envs: &[(&str, &str)],
    argv: &[&str],
) -> anyhow::Result<i32> {
    let mut cmd = base_cmd(volume, image, true);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.args(argv);
    let st = cmd
        .status()
        .with_context(|| format!("failed to run docker (is the engine up? volume {volume})"))?;
    Ok(st.code().unwrap_or(130))
}

/// Writes a file into the container via stdin (platform-independent; does not rely on
/// the host view being present).
pub(crate) fn write_file(volume: &str, image: &str, dest: &str, content: &str) -> anyhow::Result<()> {
    let mut child = base_cmd(volume, image, false)
        .arg("sh")
        .arg("-c")
        .arg(format!("cat > {dest}"))
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to start docker")?;
    child
        .stdin
        .as_mut()
        .expect("stdin is declared piped")
        .write_all(content.as_bytes())
        .with_context(|| format!("write inside container to {dest} failed"))?;
    let st = child.wait()?;
    anyhow::ensure!(
        st.success(),
        "in-container write to {dest} failed (exit {})",
        st.code().unwrap_or(-1)
    );
    Ok(())
}

/// arch → in-container make env (arm64 on an arm64 container = native front end, the
/// rest cross-compiled; verbatim from kernel.sh — the container base is arm64, so
/// x86_64/riscv64 always cross-compile).
pub(crate) fn make_env(arch: Arch) -> Vec<(&'static str, &'static str)> {
    match arch {
        Arch::Arm64 => vec![("ARCH", "arm64")],
        Arch::X86_64 => vec![("ARCH", "x86_64"), ("CROSS_COMPILE", "x86_64-linux-gnu-")],
        Arch::Riscv64 => vec![("ARCH", "riscv64"), ("CROSS_COMPILE", "riscv64-linux-gnu-")],
    }
}

/// arch → clangd --target triple (replacement for the --target line in .clangd).
pub(crate) fn clangd_target(arch: Arch) -> &'static str {
    match arch {
        Arch::Arm64 => "aarch64-linux-gnu",
        Arch::X86_64 => "x86_64-linux-gnu",
        Arch::Riscv64 => "riscv64-linux-gnu",
    }
}

/// arch → kernel image target for make (x86_64 has no Image target, only bzImage).
pub(crate) fn make_image_target(arch: Arch) -> &'static str {
    match arch {
        Arch::Arm64 | Arch::Riscv64 => "Image",
        Arch::X86_64 => "bzImage",
    }
}

/// arch → container-native form of `uname -m` (used by provisioning-script guards).
fn uname_arch(arch: Arch) -> &'static str {
    match arch {
        Arch::Arm64 => "aarch64",
        Arch::X86_64 => "x86_64",
        Arch::Riscv64 => "riscv64",
    }
}

/// kernel build extra step: static bpftool inside the kernel tree (in-tree libbpf linked
/// from the same tree, strictly matching the kernel under test — the control plane for
/// `btf dump`/`prog`/`map` inspection). libelf.a's elf_compress pulls in zstd, so static
/// linking needs explicit `-lzstd`; mainline 6.12+ bpftool sign.c pulls in libcrypto
/// (EVP/PEM), needing explicit `-lcrypto` (the openEuler 6.6 tree lacks sign.c; one extra
/// .a is harmless). Only feasible when the container's native arch matches the target
/// (static libelf/z/zstd/crypto packages are installed only for the container-native arch;
/// cross builds lack the static libs). Failure is not fatal — the kernel make exit code is
/// unaffected, only a WARN hint is printed.
pub(crate) fn bpftool_step(jobs: usize, arch: Arch) -> String {
    let uname = uname_arch(arch);
    format!(
        "\
if [ \"$(uname -m)\" = \"{uname}\" ]; then
  if env -u ARCH -u CROSS_COMPILE make -C tools/bpf/bpftool -j{jobs} \\
    LDFLAGS=-static 'LIBS=$(LIBBPF) -lelf -lz -lzstd -lcrypto'; then
    echo 'bpftool: static build ready (tools/bpf/bpftool/bpftool)'
  else
    echo 'WARN: bpftool build failed — refresh the toolchain image (libzstd-dev, see devkit/docker/Dockerfile.kernel) or check the tree state'
  fi
else
  echo 'WARN: bpftool skipped — static supply needs a same-arch container (target {uname})'
fi
"
    )
}

/// compile_commands.json generation (raw form under /ksrc, consumed by in-container
/// clangd/devcontainer). The script ships with the kernel tree (aggregated from .cmd
/// files, no bear needed): mainline/openEuler both use
/// `scripts/clang-tools/gen_compile_commands.py`; some trees may place it at
/// `scripts/compile_commands.py`, used as a fallback.
pub(crate) fn cdb_generate(volume: &str, image: &str) -> anyhow::Result<()> {
    let script = "if [ -f scripts/clang-tools/gen_compile_commands.py ]; then \
                      python3 scripts/clang-tools/gen_compile_commands.py; \
                  else \
                      python3 scripts/compile_commands.py; \
                  fi";
    run_streaming(volume, image, &[], script)
}

// ---------------------------------------------------------------- clone pipeline

// clone pipeline: create volume → git clone into the volume → render .clangd → write current.

/// .clangd template source (devkit/docker/.clangd, the single source of truth; the
/// --target line is replaced by render_clangd per target architecture).
pub(crate) fn clangd_template(project_root: &Path) -> anyhow::Result<String> {
    let path = project_root.join("devkit").join("docker").join(".clangd");
    std::fs::read_to_string(&path).with_context(|| {
        format!(
            "read .clangd template failed (is {} in place?)",
            path.display()
        )
    })
}

/// --target=<triple> line replacement (retires kernel.sh's sed: `s/--target=[a-z0-9_-]*/…/`).
pub(crate) fn render_clangd(template: &str, target: &str) -> String {
    const NEEDLE: &str = "--target=";
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find(NEEDLE) {
        out.push_str(&rest[..i + NEEDLE.len()]);
        rest = &rest[i + NEEDLE.len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'))
            .unwrap_or(rest.len());
        out.push_str(target);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Inputs of the full clone job (assembled once, no long parameter list).
pub(crate) struct CloneJob<'a> {
    pub project_root: &'a Path,
    pub volume_name: &'a str,
    pub arch: Arch,
    pub url: &'a str,
    pub ref_name: &'a str,
    pub image: &'a str,
    pub dockerfile_dir: &'a Path,
    /// Full-history clone (default false = `--depth 1`, tree only; set true for the main
    /// learning volume, where git log/blame need the full history).
    pub full: bool,
}

/// Full clone: image provisioning → idempotent volume creation (refuses existing content)
/// → git clone (default --depth 1, --full for full history) → .clangd rendered per arch
/// into the source root → write current. Returns the host-visible path.
pub(crate) fn clone_kernel(job: &CloneJob, progress: &mut Progress) -> anyhow::Result<PathBuf> {
    let CloneJob {
        project_root,
        volume_name,
        arch,
        url,
        ref_name,
        image,
        dockerfile_dir,
        full,
    } = *job;
    ensure_image(image, dockerfile_dir, progress)?;

    let out = std::process::Command::new("docker")
        .args(["volume", "create", volume_name])
        .output()
        .context("failed to run docker volume create (is docker present?)")?;
    anyhow::ensure!(
        out.status.success(),
        "docker volume create {volume_name} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );

    let view = host_view(volume_name)?;
    anyhow::ensure!(
        status(&view) == Status::Empty,
        "volume {volume_name} already has content (use `--as <new-volume>` instead, or switch over with `virtuoso kernel use` and build incrementally)"
    );

    let depth = if full { "" } else { "--depth 1 " };
    progress.line(&format!(
        "Kernel: cloning {url}@{ref_name} → volume {volume_name}{} ...",
        if full { " (full history)" } else { "" }
    ));
    let script = format!(
        "git clone --progress {depth}--branch {} {} /tmp/k && cp -a /tmp/k/. /ksrc/",
        shell_quote(ref_name),
        shell_quote(url)
    );
    run_streaming(volume_name, image, &[], &script)?;

    progress.line(&format!(
        "Kernel: rendering .clangd (--target={}) ...",
        clangd_target(arch)
    ));
    let template = clangd_template(project_root)?;
    write_file(
        volume_name,
        image,
        "/ksrc/.clangd",
        &render_clangd(&template, clangd_target(arch)),
    )?;

    write_current(
        project_root,
        &Current {
            volume: volume_name.to_string(),
            arch: arch.name().to_string(),
        },
    )?;
    Ok(view)
}

// ---------------------------------------------------------------- devcontainer rendering

// current switch → `.devcontainer/devcontainer.json` (git-ignored).
//
// devkit/docker/devcontainer.json is a static template (workspaceMount pinned to the
// default volume virtuoso-kernel) and is unaware of multi-volume `--as` naming — the
// coexistence of several named-volume kernels conflicts with a fixed volume name. Here,
// inside [`write_current`] (the sole entry point writing current from clone/use), an
// active-volume-specific devcontainer.json is rendered in sync: workspaceMount points at
// the current volume, image = the pinned toolchain image (reusing the
// `KERNEL_TOOLCHAIN_IMAGE` override; the first entry skips a local Dockerfile.kernel build).
//
// The landing spot must be repo-root `.devcontainer/`: VS Code's auto-discovery contract
// only scans `.devcontainer/` under the opened workspace (or a root-level
// devcontainer.json) — placing it in `.virtuoso/` leaves manually picking a hidden
// directory as the only option (and the macOS folder picker hides dotfiles by default).
// Open the repo root → "Reopen in Container" to enter the current volume's /ksrc.

/// Template (JSONC; the devcontainer.json spec allows comments): {VOLUME} / {IMAGE} are
/// replaced by render_template; customizations stay in sync with the static template.
const TEMPLATE: &str = r#"// Auto-rendered by `virtuoso kernel use/clone` — do not hand-edit; re-run `virtuoso kernel use` when switching volumes.
// Open the repo root: VS Code → "Reopen in Container" (auto-discovers this file) to enter /ksrc.
// workspace = /ksrc (the current active named volume); image = the pinned toolchain
// (env KERNEL_TOOLCHAIN_IMAGE substituted at render time).
{
  "name": "virtuoso-kernel",
  "image": "{IMAGE}",
  "workspaceMount": "src={VOLUME},dst=/ksrc,type=volume",
  "workspaceFolder": "/ksrc",
  "remoteUser": "root",
  "customizations": {
    "vscode": {
      "extensions": [
        "llvm-vs-code-extensions.vscode-clangd",
        "rust-lang.rust-analyzer",
        "ms-azuretools.vscode-docker"
      ],
      "settings": {
        // clangd runs in-container: consumes /ksrc/.clangd + compile_commands.json
        "clangd.arguments": [
          "--background-index",
          "--clang-tidy=false",
          "--header-insertion=never",
          "--query-driver=/usr/bin/*"
        ],
        // Rust-side indexing: make rust-analyzer in the container emits rust-project.json, picked up by the extension
        // Kernel C style: 8-space indent; Rust side follows standard rustfmt 4-space
        "editor.tabSize": 8,
        "[c]": { "editor.insertSpaces": false },
        "[rust]": { "editor.tabSize": 4 },
        "files.exclude": {
          "**/*.o": true,
          "**/*.cmd": true,
          "**/*.ko": true,
          "**/.*.cmd": true
        }
      }
    }
  }
}
"#;

/// Rendered artifact path (repo-root `.devcontainer/devcontainer.json`, git-ignored; the
/// landing spot is dictated by VS Code's auto-discovery contract, see above).
pub(crate) fn devcontainer_path(project_root: &Path) -> PathBuf {
    project_root.join(".devcontainer").join("devcontainer.json")
}

/// Fills the template (volume + image → JSONC text). Kept separate from the disk write
/// so tests can consume it directly.
pub(crate) fn render_template(volume: &str, image: &str) -> String {
    TEMPLATE
        .replace("{VOLUME}", volume)
        .replace("{IMAGE}", image)
}

/// Renders per current and writes to disk (image resolution: env KERNEL_TOOLCHAIN_IMAGE >
/// ghcr-published image, same source as the kernel command group's build container).
/// Returns the artifact path. Artifacts at the legacy `.virtuoso/` landing spot are
/// removed as well (the auto-discovery contract never scans it; leaving them only misleads).
pub(crate) fn render(project_root: &Path, current: &Current) -> anyhow::Result<PathBuf> {
    let body = render_template(&current.volume, &resolve_image());
    let p = devcontainer_path(project_root);
    std::fs::create_dir_all(p.parent().expect("render path must have a .devcontainer parent"))?;
    std::fs::write(&p, body)?;
    let legacy = project_root.join(".virtuoso").join("devcontainer.json");
    if legacy.is_file() {
        let _ = std::fs::remove_file(&legacy);
    }
    Ok(p)
}

// ---------------------------------------------------------------- tests

/// Strips JSONC comment lines (template header + inline notes) so tests can validate
/// the structure with serde_json.
#[cfg(test)]
fn json_body(jsonc: &str) -> String {
    jsonc
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test scratch dir (parallel tests share the pid in the name and would trample
    /// each other).
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("forge-state-{name}-{}", std::process::id()))
    }

    #[test]
    fn missing_state_file_reads_as_none() {
        let dir = scratch("missing");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_current(&dir).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = scratch("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let cur = Current {
            volume: "ksrc-oe66".into(),
            arch: "arm64".into(),
        };
        write_current(&dir, &cur).unwrap();
        assert_eq!(read_current(&dir).unwrap(), Some(cur));
        assert!(dir.join(".virtuoso").join("kernel-current.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_file_is_an_error_with_hint() {
        let dir = scratch("corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".virtuoso")).unwrap();
        std::fs::write(state_path(&dir), "{ not json").unwrap();
        let err = format!("{}", read_current(&dir).unwrap_err());
        assert!(err.contains("kernel use"), "hint missing: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_clangd_swaps_target_triple() {
        let template =
            "CompileFlags:\n  Add:\n    - --target=aarch64-linux-gnu\n    - -fno-spell-checking\n";
        let rendered = render_clangd(template, "riscv64-linux-gnu");
        assert!(rendered.contains("--target=riscv64-linux-gnu"));
        assert!(!rendered.contains("aarch64-linux-gnu"));
        assert!(rendered.contains("-fno-spell-checking"));
    }

    #[test]
    fn render_clangd_is_idempotent_for_default() {
        let template = "Add:\n    - --target=aarch64-linux-gnu\n    - -fno-spell-checking\n";
        assert_eq!(render_clangd(template, "aarch64-linux-gnu"), template);
    }

    #[test]
    fn make_env_table_matches_kernel_sh() {
        assert_eq!(make_env(Arch::Arm64), vec![("ARCH", "arm64")]);
        assert_eq!(
            make_env(Arch::X86_64),
            vec![("ARCH", "x86_64"), ("CROSS_COMPILE", "x86_64-linux-gnu-")]
        );
        assert_eq!(
            make_env(Arch::Riscv64),
            vec![("ARCH", "riscv64"), ("CROSS_COMPILE", "riscv64-linux-gnu-")]
        );
    }

    #[test]
    fn clangd_target_table() {
        assert_eq!(clangd_target(Arch::Arm64), "aarch64-linux-gnu");
        assert_eq!(clangd_target(Arch::X86_64), "x86_64-linux-gnu");
        assert_eq!(clangd_target(Arch::Riscv64), "riscv64-linux-gnu");
    }

    #[test]
    fn make_image_target_table() {
        assert_eq!(make_image_target(Arch::Arm64), "Image");
        assert_eq!(make_image_target(Arch::X86_64), "bzImage");
        assert_eq!(make_image_target(Arch::Riscv64), "Image");
    }

    #[test]
    fn bpftool_step_pins_recipe_and_arch_guard() {
        let s = bpftool_step(16, Arch::Arm64);
        // same-arch guard + static recipe (-lzstd/-lcrypto at the end of LIBS; order matters)
        assert!(s.contains("[ \"$(uname -m)\" = \"aarch64\" ]"));
        assert!(s.contains("env -u ARCH -u CROSS_COMPILE make -C tools/bpf/bpftool -j16"));
        assert!(s.contains("LDFLAGS=-static"));
        assert!(s.contains("'LIBS=$(LIBBPF) -lelf -lz -lzstd -lcrypto'"));
        for a in [Arch::X86_64, Arch::Riscv64] {
            assert!(bpftool_step(4, a).contains(uname_arch(a)));
        }
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        // semantics pinned in crate::util (shlex.quote: safe chars verbatim, the rest wrapped whole)
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("/home/u/ksrc"), "/home/u/ksrc");
    }

    #[test]
    fn orbstack_view_layout() {
        let p = orbstack_view(Path::new("/Users/u"), "ksrc-oe66");
        assert_eq!(
            p,
            PathBuf::from("/Users/u/OrbStack/docker/volumes/ksrc-oe66")
        );
    }

    #[test]
    fn engine_endpoint_matching() {
        assert!(endpoint_is_orbstack(
            "unix:///Users/u/.orbstack/run/docker.sock"
        ));
        assert!(endpoint_is_orbstack("tcp://orbstack.internal:2375"));
        assert!(!endpoint_is_orbstack("unix:///var/run/docker.sock"));
        assert!(!endpoint_is_orbstack("npipe:////./pipe/docker_engine"));
    }

    #[test]
    fn status_probe_by_makefile_and_config() {
        let dir = std::env::temp_dir().join(format!("forge-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(status(&dir), Status::Empty);
        std::fs::write(dir.join("Makefile"), "").unwrap();
        assert_eq!(status(&dir), Status::Cloned);
        std::fs::write(dir.join(".config"), "").unwrap();
        assert_eq!(status(&dir), Status::Configured);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_targets_current_volume_and_image() {
        let out = render_template("ksrc-oe66", "ghcr.io/yxalix/virtuoso-kernel:latest");
        let v: serde_json::Value =
            serde_json::from_str(&json_body(&out)).expect("rendered output must be valid JSON");
        assert_eq!(v["workspaceMount"], "src=ksrc-oe66,dst=/ksrc,type=volume");
        assert_eq!(v["workspaceFolder"], "/ksrc");
        assert_eq!(v["image"], "ghcr.io/yxalix/virtuoso-kernel:latest");
        // first entry fast path: image uses the pinned toolchain, no local Dockerfile.kernel build
        assert!(v.get("build").is_none());
        assert_eq!(v["remoteUser"], "root");
        let ext = v["customizations"]["vscode"]["extensions"]
            .as_array()
            .unwrap();
        assert_eq!(ext.len(), 3);
        assert!(ext.iter().any(|e| e == "rust-lang.rust-analyzer"));
    }

    #[test]
    fn render_template_is_idempotent() {
        let a = render_template("ksrc-mainline", "img:1");
        let b = render_template("ksrc-mainline", "img:1");
        assert_eq!(a, b);
        assert!(!a.contains("{VOLUME}") && !a.contains("{IMAGE}"));
    }
}
