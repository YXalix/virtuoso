//! 模块（KO）供给语义（build-initrd.sh 的 Rust 接管 + Phase 3 组件化）。
//!
//! 两个清单：`modules-boot.conf`（initramfs，挂 root 前必需的冻结基础集，
//! 文件在 infra/，boot 阶段附加组件条目追加在其后）与 `modules.conf`
//! （rootfs，switch_root 后加载，**完全由组件 require 并集生成**，不再有
//! 手写源文件）。行格式 `<module> [key=val ...]`：首 token 是模块名，
//! 其余 token 由 VM 内 init 原样传给 insmod；`#` 注释与空行跳过。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::util::Progress;

/// 组件计划投影出的模块清单：`boot_extra` 追加在 modules-boot.conf 冻结
/// 基础集之后（initramfs 阶段），`runtime` 生成 /lib/modules/modules.conf。
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Modules {
    pub boot_extra: Vec<String>,
    pub runtime: Vec<String>,
}

/// conf 行 / 组件 require 条目的模块名（首 token）。
pub(crate) fn module_name(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or_default()
}

/// 解析 conf 文件：返回模块名列表（保持声明顺序 —— 依赖手工排序是冻结语义）。
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

/// 单次全树遍历收集内核树内全部 `<stem>.ko`：stem → 路径（同名 stem 取
/// 首个命中，与逐模块 `find -print -quit` 的首个命中语义一致）。批量拷贝
/// 与存在性检查共享这一遍遍历——逐模块 find 在慢卷（如 OrbStack NFS 视图）
/// 上会放大成 N 次全树扫描。
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

/// 内核树未命中时的兜底：`infra/testcases/<mod>.ko`（预置模块）。
pub(crate) fn infra_ko(infra_dir: &Path, module: &str) -> Option<PathBuf> {
    let fallback = infra_dir.join("testcases").join(format!("{module}.ko"));
    fallback.is_file().then_some(fallback)
}

/// 内核树 `modules.builtin` 里的内建模块名集合（basename，如 crc16）。该
/// 文件由 `make modules` 生成在树根，与 .config 符号命名解耦（ext4 ↔
/// EXT4_FS、mbcache ↔ FS_MBCACHE 这类 stem≠symbol 的跨版本差异不影响）。
/// 文件缺失（未构建的树）= 空集合。
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

/// 拷贝模块名清单声明的全部 `.ko` 到 dest，并把生成的 conf 文本写入
/// `dest/<conf_name>`（VM 内 init 从 conf 读取加载顺序与 insmod 参数；
/// 文本支持 `#` 注释与空行，两个 init 均跳过）。缺 `.ko` 构建期报错
/// （退出码非 0，冻结语义）——例外：清单条目在目标树里是**内建**
/// （modules.builtin 命中）时跳过拷贝并从 conf 文本剔除对应行，init 无需
/// 对已内建驱动 insmod（同一清单要跨内核树复用：crc16 在 openEuler 内建、
/// 主线随 ext4=m 必为模块）。
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
    // 逐条解析：树内 .ko → 内建（跳过）→ infra 兜底 → 报错。顺序保持声明序。
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
    // 内建条目从 conf 文本剔除（按首 token 模块名匹配行）。
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

/// 组装 initramfs 的 boot 清单：modules-boot.conf 冻结基础集 + 组件
/// boot 附加条目（conf 文本 = 基础集原文 + 附加行，顺序即加载顺序）。
/// 返回 (模块名清单, 生成 conf 文本)。
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

/// 由 runtime 条目生成 modules.conf 文本（带生成头注释）。
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
        // 同一 boot 清单跨树复用：crc16 在此树内建（modules.builtin 命中、
        // 无 .ko 文件）→ 跳过拷贝 + conf 剔除；模块化的 virtio 照常拷贝。
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
        assert!(!dest.join("crc16.ko").exists(), "内建模块不得拷贝/insmod");
        assert_eq!(std::fs::read(dest.join("virtio.ko")).unwrap(), b"ko");
        assert_eq!(
            std::fs::read_to_string(dest.join("modules-boot.conf")).unwrap(),
            "# rootfs\nvirtio\n",
            "内建条目必须从 conf 文本剔除"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_module_still_errors_when_absent_everywhere() {
        // modules.builtin 未命中（真缺失/拼写错误）→ 冻结报错语义不变。
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
