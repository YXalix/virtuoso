//! 顶层工具集（原 common 基础层摊平后的单文件形态）。
//!
//! 分节：中断退出码常量 → 文件系统与进程环境 → 人类可读格式化 → 进度输出
//! → POSIX shell 单参引用 → 时间工具 → 终端呈现原语 → 内存单位解析。

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- 中断退出码

/// Ctrl-C / 信号中断退出码（与 shell 信号语义一致）。
///
/// guardian（Ctrl-C 守护）不依赖 judge，但需要同一常量——故钉在这里，
/// judge（唯一语义表）re-export 对外。
pub(crate) const EXIT_INTERRUPTED: i32 = 130;

// ---------------------------------------------------------------- 文件系统与进程环境

/// 定位可执行文件的完整路径（不在 PATH 返回 None）。
pub(crate) fn which_path(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        return Path::new(bin).is_file().then(|| PathBuf::from(bin));
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(bin))
            .find(|p| p.is_file())
    })
}

/// 定位可执行文件是否在 PATH（verify 用）。
pub(crate) fn which(bin: &str) -> bool {
    which_path(bin).is_some()
}

/// 置 0o755（下载产物 / VM 内 init / 测试二进制统一使用）。
pub(crate) fn set_executable(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
}

/// 读取文件前 4 字节（ELF 魔数校验的共用原语）。
fn read_magic(p: &Path) -> std::io::Result<[u8; 4]> {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(p)?.read_exact(&mut magic)?;
    Ok(magic)
}

/// 文件是否以 \x7fELF 魔数开头（读取失败一律视为否）。
pub(crate) fn is_elf(p: &Path) -> bool {
    read_magic(p)
        .map(|m| m == [0x7f, b'E', b'L', b'F'])
        .unwrap_or(false)
}

// ---------------------------------------------------------------- 人类可读格式化

/// ls -lh 风格的大小（36.7M / 1.5G）。
pub(crate) fn human_size_ls(bytes: u64) -> String {
    let (n, unit) = if bytes >= 1 << 30 {
        (bytes as f64 / (1 << 30) as f64, "G")
    } else if bytes >= 1 << 20 {
        (bytes as f64 / (1 << 20) as f64, "M")
    } else if bytes >= 1 << 10 {
        (bytes as f64 / (1 << 10) as f64, "K")
    } else {
        return format!("{bytes}B");
    };
    if n >= 10.0 {
        format!("{n:.0}{unit}")
    } else {
        format!("{n:.1}{unit}")
    }
}

// ---------------------------------------------------------------- 构建增量

/// 暂存树内容指纹：相对路径 + 条目类型 + 长度/符号链接目标的 SipHash 串接
/// （std 自带 DefaultHasher，无新依赖；排序遍历保证确定性）。镜像级增量用：
/// 指纹未变 = 输入字节不变，可安全跳过重建。`extra` 并入指纹（构建方式/参数）。
pub(crate) fn tree_fingerprint(dir: &Path, extra: &str) -> anyhow::Result<String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn walk(h: &mut DefaultHasher, dir: &Path) -> anyhow::Result<()> {
        let mut entries: Vec<std::fs::DirEntry> =
            std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let ft = e.file_type()?;
            e.file_name().hash(h);
            if ft.is_symlink() {
                1u8.hash(h);
                std::fs::read_link(e.path())?.hash(h);
            } else if ft.is_dir() {
                2u8.hash(h);
                walk(h, &e.path())?;
            } else {
                3u8.hash(h);
                e.metadata()?.len().hash(h);
                let mut f = std::fs::File::open(e.path())?;
                let mut buf = [0u8; 64 * 1024];
                loop {
                    let n = std::io::Read::read(&mut f, &mut buf)?;
                    if n == 0 {
                        break;
                    }
                    h.write(&buf[..n]);
                }
            }
        }
        Ok(())
    }

    let mut h = DefaultHasher::new();
    extra.hash(&mut h);
    walk(&mut h, dir)?;
    Ok(format!("{:016x}", h.finish()))
}

/// 镜像级增量：暂存树指纹（含构建方式 `extra`）与 `<out>.stamp` 一致且产物
/// 在 → 跳过重建（返回 false）；否则执行 `build` 并落新 stamp。
/// stamp 放产物旁（git 忽略的 artifacts 目录），删产物即失效。
pub(crate) fn if_changed(
    dir: &Path,
    extra: &str,
    out: &Path,
    label: &str,
    progress: &mut Progress,
    build: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    let fp = tree_fingerprint(dir, extra)?;
    let stamp = std::path::PathBuf::from(format!("{}.stamp", out.display()));
    if out.is_file() && std::fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == fp) {
        progress.line(&format!("{label}: unchanged, skipped"));
        return Ok(false);
    }
    build()?;
    std::fs::write(&stamp, fp)?;
    Ok(true)
}

// ---------------------------------------------------------------- 进度输出

/// 进度输出通道：stdout 恒打，可选同步写日志文件。
/// builder（构建流水线）与 forge（kernel 命令组）共用同一形态。
pub(crate) struct Progress {
    log: Option<std::fs::File>,
}

impl Progress {
    /// 仅终端形态。
    pub(crate) fn stdout() -> Self {
        Self { log: None }
    }

    /// 终端 + 追加日志形态（运行工件 build.log）。
    pub(crate) fn with_log(path: &std::path::Path) -> std::io::Result<Self> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self { log: Some(log) })
    }

    pub(crate) fn line(&mut self, msg: &str) {
        println!("{msg}");
        if let Some(f) = self.log.as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "{msg}");
        }
    }
}

// ---------------------------------------------------------------- POSIX shell 单参引用

// POSIX shell 单参引用（shlex.quote 语义）：安全字符原样，其余整体单引号
// 包裹。用于把 argv 渲染成可直接复制执行的一行命令（launcher 的
// `command_line`），以及把路径/URL/ref 安全嵌入容器 `sh -c` 脚本（forge）。

/// 安全字符集：字母数字与 shell 元字符之外的普通文件名成分——单参不含
/// 空白/引号/glob 字符时原样输出即等价。
fn is_safe(arg: &str) -> bool {
    !arg.is_empty()
        && arg
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b'@' | b'%' | b'+' | b','))
}

/// POSIX shell 单参引用：安全字符原样，其余整体单引号包裹（内嵌单引号转义）。
pub(crate) fn quote(arg: &str) -> String {
    if is_safe(arg) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

// ---------------------------------------------------------------- 时间工具

/// 当前 Unix 毫秒（时钟回拨时退化为 0）。
pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Unix 毫秒 → "YYYY-MM-DD HH:MM:SS UTC"（civil_from_days 算法）。
pub(crate) fn format_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// 公历日数 → (年, 月, 日)（Howard Hinnant 的 civil_from_days 算法）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------- 终端呈现原语

// 终端呈现原语：TTY 感知颜色与状态图标。
//
// 约定：颜色只修饰不承载语义（非 TTY / NO_COLOR 自动退化纯文本）；
// 状态图标 ✓ / ✗ 与 [PASS]/[FAIL] 同义。

/// stdout 是否着色（TERM 存在且非 dumb，且无 NO_COLOR；仅影响颜色不影响判定）。
pub(crate) fn tty() -> bool {
    std::env::var_os("TERM")
        .map(|t| t != "dumb")
        .unwrap_or(false)
        && std::env::var_os("NO_COLOR").is_none()
}

/// ANSI 包裹（非 TTY 原样返回）。
pub(crate) fn paint(code: &str, s: &str) -> String {
    if tty() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// 粗体。
pub(crate) fn bold(s: &str) -> String {
    paint("1", s)
}

/// 绿（通过）。
pub(crate) fn green(s: &str) -> String {
    paint("0;32", s)
}

/// 红（失败）。
pub(crate) fn red(s: &str) -> String {
    paint("0;31", s)
}

/// 状态图标（按最严重级别取）：✓ / ✗。
pub(crate) fn icon(level: Icon) -> String {
    match level {
        Icon::Pass => green("✓"),
        Icon::Fail => red("✗"),
    }
}

pub(crate) enum Icon {
    Pass,
    Fail,
}

// ---------------------------------------------------------------- 内存单位解析

// 内存量字符串解析（NUMA 拓扑与 pmem 组件共用同一张合法单位表）。

/// 内存单位后缀；`Bare` = 无单位裸数字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemUnit {
    G,
    M,
    Bare,
}

impl MemUnit {
    /// 数值 × 单位 → 规范字符串（"2G" / "512M" / "1024"）。
    pub(crate) fn render(self, n: u64) -> String {
        match self {
            MemUnit::G => format!("{n}G"),
            MemUnit::M => format!("{n}M"),
            MemUnit::Bare => n.to_string(),
        }
    }

    /// 数值 → MiB（G = ×1024；M 与裸数字原值）。
    pub(crate) fn to_mib(self, n: u64) -> u64 {
        match self {
            MemUnit::G => n * 1024,
            MemUnit::M | MemUnit::Bare => n,
        }
    }
}

/// 解析 "1G" / "512M" / "1024"（单位大小写不敏感）→ (数值, 单位)。
/// 非数字前缀或单位不在 G/M/裸数字表内时报错。
pub(crate) fn parse_memory(s: &str) -> Result<(u64, MemUnit), String> {
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n: u64 = s[..digits]
        .parse()
        .map_err(|_| format!("cannot parse memory value: {s}"))?;
    let unit = match s[digits..].trim() {
        "" => MemUnit::Bare,
        "G" | "g" => MemUnit::G,
        "M" | "m" => MemUnit::M,
        other => return Err(format!("unsupported memory unit: {other}")),
    };
    Ok((n, unit))
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_degrades_without_tty() {
        // 测试进程 TERM 可能存在；两个分支都要产出合法字符串
        let s = paint("1", "x");
        assert!(s == "x" || s == "\x1b[1mx\x1b[0m");
    }

    /// 历法锚点：纪元边界、平年/闰年切换（civil_from_days 只在这几类
    /// 规则上出错，锚点钉死即够）。
    #[test]
    fn format_utc_calendar_anchors() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(1_000), "1970-01-01 00:00:01 UTC");
        assert_eq!(format_utc(86_399_999), "1970-01-01 23:59:59 UTC");
        // 平年→闰年：2024-02-28 与 02-29
        assert_eq!(format_utc(1_709_107_200_000), "2024-02-28 08:00:00 UTC");
        assert_eq!(format_utc(1_709_193_600_000), "2024-02-29 08:00:00 UTC");
        // 闰年→平年回落：2024-03-01
        assert_eq!(format_utc(1_709_280_000_000), "2024-03-01 08:00:00 UTC");
        // 年末进位与 400 年闰 2000
        assert_eq!(format_utc(1_735_689_599_999), "2024-12-31 23:59:59 UTC");
        assert_eq!(format_utc(946_684_800_000), "2000-01-01 00:00:00 UTC");
    }

    #[test]
    fn unix_ms_is_epoch_scale() {
        let now = unix_ms();
        assert!(now > 1_700_000_000_000); // 2023-11 之后（装机即真）
    }

    #[test]
    fn parse_memory_units() {
        assert_eq!(parse_memory("2G").unwrap(), (2, MemUnit::G));
        assert_eq!(parse_memory("512m").unwrap(), (512, MemUnit::M));
        assert_eq!(parse_memory("1024").unwrap(), (1024, MemUnit::Bare));
        assert_eq!(MemUnit::G.render(2), "2G");
        assert_eq!(MemUnit::G.to_mib(2), 2048);
        assert!(parse_memory("1X").is_err());
        assert!(parse_memory("x1").is_err());
    }
}
