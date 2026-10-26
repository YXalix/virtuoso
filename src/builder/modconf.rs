//! Kernel module (KO) provisioning semantics (Rust takeover of
//! build-initrd.sh + Phase 3 componentization).
//!
//! Two manifests: `modules-boot.conf` (initramfs; the frozen base set
//! needed before mounting root, file lives in infra/, component boot-extra
//! entries are appended after it) and `modules.conf` (rootfs; loaded after
//! switch_root, **generated entirely from the union of component
//! requires**, no hand-written source anymore). Line format
//! `<module> [key=val ...]`: the first token is the module name, the rest
//! are passed verbatim to insmod by the in-VM init; `#` comments and blank
//! lines are skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::util::Progress;

/// Module manifests projected from the component plan: `boot_extra` is
/// appended after the frozen modules-boot.conf base set (initramfs stage);
/// `runtime` generates /lib/modules/modules.conf.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Modules {
    pub boot_extra: Vec<String>,
    pub runtime: Vec<String>,
}

/// Module name (first token) of a conf line / component require entry.
pub(crate) fn module_name(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or_default()
}

/// Parses a conf file: returns the module name list (declaration order preserved — manual dependency ordering is frozen semantics).
pub(crate) fn parse(conf: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(conf) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
}

/// Collects every `<stem>.ko` in the kernel tree in a single walk:
/// stem → path (for duplicate stems the first hit wins, matching the
/// first-hit semantics of per-module `find -print -quit`). Batch copies and
/// presence checks share this single walk — per-module find would amplify
/// into N full-tree scans on slow volumes (e.g. an OrbStack NFS view).
pub(crate) fn collect_kos(kernel_path: &Path) -> BTreeMap<String, PathBuf> {
    let mut map = BTreeMap::new();
    let Ok(out) = std::process::Command::new("find")
        .arg(kernel_path)
        .arg("-name")
        .arg("*.ko")
        .arg("-print")
        .output()
    else {
        return map;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.is_empty() {
            continue;
        }
        let path = PathBuf::from(line);
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            map.entry(stem.to_string()).or_insert(path);
        }
    }
    map
}

/// Fallback when the kernel tree misses: `infra/testcases/<mod>.ko` (prebuilt module).
pub(crate) fn infra_ko(infra_dir: &Path, module: &str) -> Option<PathBuf> {
    let fallback = infra_dir.join("testcases").join(format!("{module}.ko"));
    fallback.is_file().then_some(fallback)
}

/// Set of builtin module names (basenames, e.g. crc16) from the kernel
/// tree's `modules.builtin`. The file is generated at the tree root by
/// `make modules`, decoupled from .config symbol naming (cross-version
/// stem≠symbol differences such as ext4 ↔ EXT4_FS or mbcache ↔ FS_MBCACHE
/// do not matter). Missing file (unbuilt tree) = empty set.
pub(crate) fn builtin_modules(kernel_path: &Path) -> std::collections::BTreeSet<String> {
    let mut set = std::collections::BTreeSet::new();
    let Ok(text) = std::fs::read_to_string(kernel_path.join("modules.builtin")) else {
        return set;
    };
    for line in text.lines() {
        if let Some(stem) = Path::new(line.trim())
            .file_stem()
            .and_then(|s| s.to_str())
        {
            set.insert(stem.to_string());
        }
    }
    set
}

/// Copies every `.ko` declared by the module name list to dest and writes
/// the generated conf text to `dest/<conf_name>` (the in-VM init reads
/// load order and insmod args from the conf; the text supports `#` comments
/// and blank lines, skipped by both inits). A missing `.ko` is a build-time
/// error (non-zero exit, frozen semantics) — exception: when a list entry
/// is **builtin** in the target tree (hit in modules.builtin), the copy is
/// skipped and the line removed from the conf text; init need not insmod a
/// builtin driver (the same list is reused across kernel trees: crc16 is
/// builtin in openEuler, while mainline builds it as a module when ext4=m).
pub(crate) fn copy_module_list(
    dest: &Path,
    modules: &[String],
    conf_text: &str,
    conf_name: &str,
    kernel_path: &Path,
    infra_dir: &Path,
    progress: &mut Progress,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    let builtin = builtin_modules(kernel_path);
    let kos = if modules.is_empty() {
        BTreeMap::new()
    } else {
        collect_kos(kernel_path)
    };
    // Resolve each entry: in-tree .ko → builtin (skip) → infra fallback → error. Declaration order preserved.
    let mut resolved: Vec<(&String, PathBuf)> = Vec::new();
    for module in modules {
        if let Some(ko) = kos.get(module.as_str()).cloned() {
            resolved.push((module, ko));
        } else if builtin.contains(module.as_str()) {
            progress.line(&format!("  {module}: builtin in kernel, skipped"));
        } else if let Some(ko) = infra_ko(infra_dir, module) {
            resolved.push((module, ko));
        } else {
            anyhow::bail!("Module {module}.ko not found");
        }
    }
    if resolved.is_empty() {
        progress.line("  (no kernel modules in this stage's list)");
    } else {
        progress.line(&format!(
            "Copying {} kernel module(s)...",
            resolved.len()
        ));
    }
    for (module, ko) in &resolved {
        std::fs::copy(ko, dest.join(format!("{module}.ko")))
            .with_context(|| format!("copy {} failed", ko.display()))?;
    }
    // Strip builtin entries from the conf text (lines matched by first-token module name).
    let filtered: String = conf_text
        .lines()
        .filter(|l| {
            let name = l.split_whitespace().next().unwrap_or_default();
            !builtin.contains(name)
        })
        .fold(String::new(), |mut acc, l| {
            acc.push_str(l);
            acc.push('\n');
            acc
        });
    std::fs::write(dest.join(conf_name), filtered)
        .with_context(|| format!("write {conf_name} failed"))?;
    Ok(())
}

/// Assembles the initramfs boot manifest: the frozen modules-boot.conf base
/// set + component boot-extra entries (conf text = base verbatim + extra
/// lines; order is load order). Returns (module name list, generated conf text).
pub(crate) fn boot_set(base_conf: &Path, extra: &[String]) -> anyhow::Result<(Vec<String>, String)> {
    let mut names = parse(base_conf);
    let mut text = std::fs::read_to_string(base_conf).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for line in extra {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        text.push_str(line);
        text.push('\n');
        let name = module_name(line);
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    Ok((names, text))
}

/// Generates the modules.conf text from runtime entries (with a generated header comment).
pub(crate) fn runtime_conf_text(entries: &[String]) -> String {
    let mut text =
        String::from("# generated by builder from virtuoso.toml [components.*] require\n");
    for line in entries {
        text.push_str(line.trim());
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comments_and_args() {
        let dir = std::env::temp_dir().join(format!("builder-modconf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("modules.conf");
        std::fs::write(
            &conf,
            "# comment\ncrc64\nnvme-core  poll_queues=2\n\n  # indented\next4\n",
        )
        .unwrap();
        let mods = parse(&conf);
        assert_eq!(mods, ["crc64", "nvme-core", "ext4"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn boot_set_appends_extra_without_duplicating_names() {
        let dir = std::env::temp_dir().join(format!("builder-bootset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("modules-boot.conf");
        std::fs::write(&conf, "# base\nvirtio_blk\next4\n").unwrap();
        let (names, text) = boot_set(
            &conf,
            &["crc64".into(), "virtio_blk".into(), "x  arg=1".into()],
        )
        .unwrap();
        assert_eq!(names, ["virtio_blk", "ext4", "crc64", "x"]);
        assert!(text.starts_with("# base\nvirtio_blk\next4\n"));
        assert!(text.contains("crc64\n"));
        assert!(text.ends_with("x  arg=1\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_conf_text_has_generated_header() {
        let text = runtime_conf_text(&["virtio_console".into(), "crc64".into()]);
        assert!(text.starts_with("# generated by builder"));
        assert!(text.contains("virtio_console\ncrc64\n"));
    }

    #[test]
    fn collect_kos_maps_stems_from_nested_tree() {
        let dir = std::env::temp_dir().join(format!("builder-collectkos-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tree/drivers/virtio")).unwrap();
        std::fs::create_dir_all(dir.join("tree/fs")).unwrap();
        std::fs::write(dir.join("tree/drivers/virtio/virtio.ko"), b"ko").unwrap();
        std::fs::write(dir.join("tree/fs/ext4.ko"), b"ko").unwrap();
        let map = collect_kos(&dir.join("tree"));
        assert_eq!(map.get("virtio").unwrap().file_name().unwrap(), "virtio.ko");
        assert_eq!(map.get("ext4").unwrap().file_name().unwrap(), "ext4.ko");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_module_list_takes_tree_hit_and_infra_fallback() {
        let dir =
            std::env::temp_dir().join(format!("builder-copymods-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tree/drivers")).unwrap();
        std::fs::create_dir_all(dir.join("infra/testcases")).unwrap();
        std::fs::write(dir.join("tree/drivers/virtio.ko"), b"ko").unwrap();
        std::fs::write(dir.join("infra/testcases/crc64.ko"), b"ko").unwrap();
        let dest = dir.join("dest");
        copy_module_list(
            &dest,
            &["virtio".into(), "crc64".into()],
            "virtio\ncrc64\n",
            "modules.conf",
            &dir.join("tree"),
            &dir.join("infra"),
            &mut Progress::stdout(),
        )
        .unwrap();
        assert_eq!(std::fs::read(dest.join("virtio.ko")).unwrap(), b"ko");
        assert_eq!(std::fs::read(dest.join("crc64.ko")).unwrap(), b"ko");
        assert_eq!(std::fs::read_to_string(dest.join("modules.conf")).unwrap(), "virtio\ncrc64\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_module_is_skipped_from_copy_and_conf() {
        // The same boot manifest reused across trees: crc16 is builtin in
        // this tree (modules.builtin hit, no .ko file) → copy skipped +
        // line stripped from conf; the modular virtio copies as usual.
        let dir =
            std::env::temp_dir().join(format!("builder-copybuiltin-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tree/drivers")).unwrap();
        std::fs::write(dir.join("tree/drivers/virtio.ko"), b"ko").unwrap();
        std::fs::write(dir.join("tree/modules.builtin"), "kernel/lib/crc16.ko\n").unwrap();
        let dest = dir.join("dest");
        copy_module_list(
            &dest,
            &["crc16".into(), "virtio".into()],
            "# rootfs\ncrc16\nvirtio\n",
            "modules-boot.conf",
            &dir.join("tree"),
            &dir.join("infra"),
            &mut Progress::stdout(),
        )
        .unwrap();
        assert!(!dest.join("crc16.ko").exists(), "builtin modules must not be copied/insmodded");
        assert_eq!(std::fs::read(dest.join("virtio.ko")).unwrap(), b"ko");
        assert_eq!(
            std::fs::read_to_string(dest.join("modules-boot.conf")).unwrap(),
            "# rootfs\nvirtio\n",
            "builtin entries must be stripped from the conf text"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_module_still_errors_when_absent_everywhere() {
        // modules.builtin miss (truly missing/typo) → the frozen error semantics are unchanged.
        let dir = std::env::temp_dir().join(format!("builder-copymiss2-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tree")).unwrap();
        std::fs::write(dir.join("tree/modules.builtin"), "kernel/lib/crc16.ko\n").unwrap();
        let err = copy_module_list(
            &dir.join("dest"),
            &["ghost".into()],
            "ghost\n",
            "modules.conf",
            &dir.join("tree"),
            &dir.join("infra"),
            &mut Progress::stdout(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("ghost.ko not found"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_module_list_bails_on_missing_module() {
        let dir = std::env::temp_dir().join(format!("builder-copymiss-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tree")).unwrap();
        let err = copy_module_list(
            &dir.join("dest"),
            &["ghost".into()],
            "ghost\n",
            "modules.conf",
            &dir.join("tree"),
            &dir.join("infra"),
            &mut Progress::stdout(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("ghost.ko not found"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
