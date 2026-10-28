//! Top-level utility set (the single-file form of the original common base
//! layer after flattening).
//!
//! Sections: interrupt exit code constant → filesystem and process
//! environment → human-readable formatting → progress output → POSIX shell
//! single-arg quoting → time utilities → terminal presentation primitives →
//! memory unit parsing.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- Interrupt exit code

/// Ctrl-C / signal interrupt exit code (matches shell signal semantics).
///
/// guardian (the Ctrl-C guard) does not depend on judge but needs the same
/// constant — so it is pinned here and re-exported by judge (the sole
/// semantics table).
pub(crate) const EXIT_INTERRUPTED: i32 = 130;

// ---------------------------------------------------------------- Filesystem and process environment

/// Env var lookup treating an empty/whitespace-only value as unset — the uniform
/// idiom behind every env > toml resolution chain.
pub(crate) fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Locate an executable's full path (None when not in PATH).
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

/// Whether an executable is in PATH (used by verify).
pub(crate) fn which(bin: &str) -> bool {
    which_path(bin).is_some()
}

/// Set 0o755 (used uniformly for downloaded artifacts / in-VM init / test binaries).
pub(crate) fn set_executable(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
}

/// Read the first 4 bytes of a file (shared primitive for ELF magic checks).
fn read_magic(p: &Path) -> std::io::Result<[u8; 4]> {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(p)?.read_exact(&mut magic)?;
    Ok(magic)
}

/// Whether the file starts with the \x7fELF magic (read failures always count as no).
pub(crate) fn is_elf(p: &Path) -> bool {
    read_magic(p)
        .map(|m| m == [0x7f, b'E', b'L', b'F'])
        .unwrap_or(false)
}

// ---------------------------------------------------------------- Human-readable formatting

/// ls -lh style size (36.7M / 1.5G).
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

// ---------------------------------------------------------------- Build incrementality

/// Content fingerprint of the staging tree: a SipHash concatenation of
/// relative paths + entry types + lengths/symlink targets (std's own
/// DefaultHasher, no new dependency; sorted traversal guarantees
/// determinism). Used for image-level incrementality: an unchanged
/// fingerprint = unchanged input bytes, safe to skip a rebuild. `extra` is
/// folded into the fingerprint (build method/args).
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

/// Image-level incrementality: if the staging tree fingerprint (including
/// the build method `extra`) matches `<out>.stamp` and the artifact exists
/// → skip the rebuild (return false); otherwise run `build` and write a new
/// stamp. The stamp sits next to the artifact (in the git-ignored artifacts
/// directory); deleting the artifact invalidates it.
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

// ---------------------------------------------------------------- Progress output

/// Progress output channel: always prints to stdout, optionally mirrors to a
/// log file synchronously. builder (the build pipeline) and forge (the
/// kernel command group) share this same form.
pub(crate) struct Progress {
    log: Option<std::fs::File>,
}

impl Progress {
    /// Terminal-only form.
    pub(crate) fn stdout() -> Self {
        Self { log: None }
    }

    /// Terminal + append-log form (the run artifact build.log).
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

// ---------------------------------------------------------------- POSIX shell single-arg quoting

// POSIX shell single-arg quoting (shlex.quote semantics): safe characters
// verbatim, everything else wrapped whole in single quotes. Used to render
// argv as a one-line command that can be copied and run directly (the
// launcher's `command_line`), and to embed paths/URLs/refs safely in
// container `sh -c` scripts (forge).

/// Safe character set: alphanumerics plus ordinary filename characters
/// outside the shell metacharacters — a single arg containing no
/// whitespace/quotes/glob characters is equivalent when emitted verbatim.
fn is_safe(arg: &str) -> bool {
    !arg.is_empty()
        && arg
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b'@' | b'%' | b'+' | b','))
}

/// POSIX shell single-arg quoting: safe characters verbatim, everything
/// else wrapped in single quotes (embedded quotes escaped).
pub(crate) fn quote(arg: &str) -> String {
    if is_safe(arg) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

// ---------------------------------------------------------------- Time utilities

/// Current Unix milliseconds (degrades to 0 if the clock is behind).
pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Unix milliseconds → "YYYY-MM-DD HH:MM:SS UTC" (civil_from_days algorithm).
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

/// Days since the Unix epoch → (year, month, day) (Howard Hinnant's
/// civil_from_days algorithm).
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

// ---------------------------------------------------------------- Terminal presentation primitives

// Terminal presentation primitives: TTY-aware colors and status icons.
//
// Convention: color only decorates and carries no semantics (non-TTY /
// NO_COLOR automatically degrade to plain text);
// status icons ✓ / ✗ are synonymous with [PASS]/[FAIL].

/// Whether stdout is colored (TERM present and not dumb, and no NO_COLOR;
/// affects colors only, never verdicts).
pub(crate) fn tty() -> bool {
    std::env::var_os("TERM")
        .map(|t| t != "dumb")
        .unwrap_or(false)
        && std::env::var_os("NO_COLOR").is_none()
}

/// ANSI wrapping (returned verbatim when not a TTY).
pub(crate) fn paint(code: &str, s: &str) -> String {
    if tty() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Bold.
pub(crate) fn bold(s: &str) -> String {
    paint("1", s)
}

/// Green (pass).
pub(crate) fn green(s: &str) -> String {
    paint("0;32", s)
}

/// Red (fail).
pub(crate) fn red(s: &str) -> String {
    paint("0;31", s)
}

/// Status icon (by most severe level): ✓ / ✗.
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

// ---------------------------------------------------------------- Memory unit parsing

// Memory amount string parsing (NUMA topology and the pmem component share
// the same table of legal units).

/// Memory unit suffix; `Bare` = a bare number without a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemUnit {
    G,
    M,
    Bare,
}

impl MemUnit {
    /// Value × unit → canonical string ("2G" / "512M" / "1024").
    pub(crate) fn render(self, n: u64) -> String {
        match self {
            MemUnit::G => format!("{n}G"),
            MemUnit::M => format!("{n}M"),
            MemUnit::Bare => n.to_string(),
        }
    }

    /// Value → MiB (G = ×1024; M and bare numbers unchanged).
    pub(crate) fn to_mib(self, n: u64) -> u64 {
        match self {
            MemUnit::G => n * 1024,
            MemUnit::M | MemUnit::Bare => n,
        }
    }
}

/// Parse "1G" / "512M" / "1024" (unit case-insensitive) → (value, unit).
/// Errors on a non-numeric prefix or a unit outside the G/M/bare table.
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

// ---------------------------------------------------------------- Test scratch directories

// Test-only scratch-directory lifecycle (compiled only under cfg(test)).
// One RAII type replaces the per-file
// temp_dir().join(..)/create/remove_dir_all dances and their explicit
// teardown lines, which leaked the tree whenever an assert fired early.

/// Scratch root for tests under the OS temp dir. Construction removes any
/// stale tree left by an earlier run of the same test; Drop removes the tree
/// again (best-effort). A process-wide sequence number in the name keeps
/// same-prefix tests from trampling each other when cargo runs them in
/// parallel threads.
#[cfg(test)]
pub(crate) struct TempRoot(PathBuf);

#[cfg(test)]
impl TempRoot {
    fn reserve(prefix: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        TempRoot(dir)
    }

    /// Reserved path with the directory created — the common shape where the
    /// test populates the tree itself.
    pub(crate) fn new(prefix: &str) -> Self {
        let t = Self::reserve(prefix);
        std::fs::create_dir_all(&t.0).unwrap();
        t
    }

    /// Reserved path guaranteed not to exist — for tests that need absence
    /// ("missing state reads as none", "bails on non-dir") or whose fixtures
    /// create their own tree.
    pub(crate) fn absent(prefix: &str) -> Self {
        Self::reserve(prefix)
    }
}

// Mirrors std: PathBuf derefs to Path (and implements AsRef<Path>), so call
// sites keep `dir.join(..)` / `&dir` shapes unchanged — including generic
// `P: AsRef<Path>` APIs where deref coercion does not apply.
#[cfg(test)]
impl std::ops::Deref for TempRoot {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl AsRef<Path> for TempRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------- Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_safe_chars_verbatim_spaces_quoted() {
        // POSIX single-arg quoting: safe chars verbatim, the rest wrapped whole
        // (consumed by command_line rendering and forge's container scripts)
        assert_eq!(quote("/tmp/a b.img"), "'/tmp/a b.img'");
        assert_eq!(quote("a'b"), "'a'\\''b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn paint_degrades_without_tty() {
        // The test process may have TERM set; both branches must produce a valid string
        let s = paint("1", "x");
        assert!(s == "x" || s == "\x1b[1mx\x1b[0m");
    }

    /// Calendar anchors: epoch boundaries, common/leap year transitions
    /// (civil_from_days only ever errs on these rule classes; pinning these
    /// anchors suffices).
    #[test]
    fn format_utc_calendar_anchors() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(1_000), "1970-01-01 00:00:01 UTC");
        assert_eq!(format_utc(86_399_999), "1970-01-01 23:59:59 UTC");
        // Common→leap year: 2024-02-28 and 02-29
        assert_eq!(format_utc(1_709_107_200_000), "2024-02-28 08:00:00 UTC");
        assert_eq!(format_utc(1_709_193_600_000), "2024-02-29 08:00:00 UTC");
        // Leap→common year fallthrough: 2024-03-01
        assert_eq!(format_utc(1_709_280_000_000), "2024-03-01 08:00:00 UTC");
        // Year-end carry and the 400-year leap year 2000
        assert_eq!(format_utc(1_735_689_599_999), "2024-12-31 23:59:59 UTC");
        assert_eq!(format_utc(946_684_800_000), "2000-01-01 00:00:00 UTC");
    }

    #[test]
    fn unix_ms_is_epoch_scale() {
        let now = unix_ms();
        assert!(now > 1_700_000_000_000); // after 2023-11 (true from install onward)
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

    #[test]
    fn temp_root_lifecycle() {
        let created = TempRoot::new("util-temproot-new");
        std::fs::write(created.join("marker"), b"x").unwrap();
        assert!(created.is_dir());

        let absent = TempRoot::absent("util-temproot-absent");
        assert!(!absent.exists(), "absent must not create the path");

        let (created_path, absent_path) = (created.to_path_buf(), absent.to_path_buf());
        drop(created);
        drop(absent);
        assert!(!created_path.exists(), "drop must remove the whole tree");
        assert!(!absent_path.exists());
    }
}
