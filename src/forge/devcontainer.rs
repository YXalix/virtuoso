//! devcontainer rendering (current switch → `.devcontainer/devcontainer.json`,
//! git-ignored).
//!
//! devkit/docker/devcontainer.json is a static template (workspaceMount pinned to the
//! default volume virtuoso-kernel) and is unaware of multi-volume `--as` naming — the
//! coexistence of several named-volume kernels conflicts with a fixed volume name. Here,
//! inside [`super::write_current`] (the sole entry point writing current from clone/use),
//! an active-volume-specific devcontainer.json is rendered in sync: workspaceMount points
//! at the current volume, image = the pinned toolchain image (reusing the
//! `KERNEL_TOOLCHAIN_IMAGE` override; the first entry skips a local Dockerfile.kernel build).
//!
//! The landing spot must be repo-root `.devcontainer/`: VS Code's auto-discovery contract
//! only scans `.devcontainer/` under the opened workspace (or a root-level
//! devcontainer.json) — placing it in `.virtuoso/` leaves manually picking a hidden
//! directory as the only option (and the macOS folder picker hides dotfiles by default).
//! Open the repo root → "Reopen in Container" to enter the current volume's /ksrc.

use std::path::{Path, PathBuf};

use super::{resolve_image, Current};

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
