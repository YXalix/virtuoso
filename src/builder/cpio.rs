//! Native writer for the cpio newc format — Rust takeover of the
//! `find | cpio -H newc | gzip` pipeline.
//!
//! One code path across platforms (Linux/macOS share it), eliminating GNU
//! cpio's `--null` platform differences and the bash/find/gzip external
//! dependencies. Write conventions align with the original pipeline:
//! - entry names carry the `./` prefix (the kernel initramfs unpacker
//!   consumes with cwd=/, same shape as find output);
//! - symlinks store the target text (filesize = target length, mode
//!   S_IFLNK, permission bits always 0777 — Linux symlinks are always 0777
//!   while macOS applies the umask; symlink permissions carry no semantics,
//!   so normalizing is what guarantees byte-identical output across hosts);
//! - mtime/ino carry no semantic value, always written as 0/an incrementing
//!   sequence → **output is deterministically reproducible by content**;
//! - directory entries nlink=2, files/links nlink=1 (the kernel unpacker
//!   does not consume nlink).

use std::fs::File;
use std::io::Write;
use std::path::Path;

use anyhow::Context;

/// Packs the whole `dir` tree into a gzip-compressed newc cpio written to `out`.
pub(crate) fn pack_dir_gzip(dir: &Path, out: &Path) -> anyhow::Result<()> {
    let mut entries: Vec<(String, std::fs::Metadata)> = Vec::new();
    collect_entries(dir, dir, &mut entries)?;
    // Determinism: sort by path (GNU find is readdir order; content-equivalent, order is not part of the contract)
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let out_file = File::create(out).with_context(|| format!("create {} failed", out.display()))?;
    let mut gz = flate2::write::GzEncoder::new(out_file, flate2::Compression::best());
    write_stream(&entries, dir, &mut gz)?;
    gz.finish().with_context(|| "gzip finish failed")?;
    Ok(())
}

/// Recursively collects all entries under `root` (symlinks themselves
/// included, not followed), named as `./`-prefixed paths relative to root
/// (the `.` root entry itself is skipped; this difference from find output
/// does not affect unpacking).
fn collect_entries(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, std::fs::Metadata)>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("read {} failed", dir.display()))?
        .flatten()
    {
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)
            .with_context(|| format!("stat {} failed", path.display()))?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let is_dir = meta.is_dir();
        out.push((format!("./{rel}"), meta));
        if is_dir {
            collect_entries(root, &path, out)?;
        }
    }
    Ok(())
}

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

/// 4-byte alignment (newc header/data segment boundary rule).
fn align4(n: usize) -> usize {
    n + (4 - n % 4) % 4
}

/// Writes the newc stream entry by entry plus TRAILER!!!.
fn write_stream<W: Write>(
    entries: &[(String, std::fs::Metadata)],
    root: &Path,
    w: &mut W,
) -> anyhow::Result<()> {
    for (i, (name, meta)) in entries.iter().enumerate() {
        use std::os::unix::fs::MetadataExt;
        let ftype = meta.mode() & S_IFMT;
        let (mode, size) = match ftype {
            S_IFDIR => (meta.mode() & 0o7777 | S_IFDIR, 0u64),
            S_IFLNK => (0o777 | S_IFLNK, meta.len()),
            _ => (meta.mode() & 0o7777 | S_IFREG, meta.len()),
        };
        let nlink: u64 = if ftype == S_IFDIR { 2 } else { 1 };
        write_header(w, name, i as u64 + 1, mode, nlink, size)?;
        let rel = Path::new(name).strip_prefix(".").unwrap_or(Path::new(name));
        match ftype {
            S_IFLNK => {
                // Symlink: data segment = target path text
                let target = std::fs::read_link(root.join(rel))
                    .with_context(|| format!("readlink {name} failed"))?;
                write_data(w, target.as_os_str().as_encoded_bytes())?;
            }
            S_IFREG => {
                let data =
                    std::fs::read(root.join(rel)).with_context(|| format!("read {name} failed"))?;
                write_data(w, &data)?;
            }
            _ => {} // directories have no data segment
        }
    }
    write_header(w, "TRAILER!!!", 0, 0, 1, 0)?;
    // Pad the tail to 512 (convention; the kernel unpacker stops at TRAILER!!!)
    let pad = 512 - align4(110 + "TRAILER!!!".len() + 1);
    w.write_all(&vec![0u8; pad])?;
    Ok(())
}

/// newc header (110-byte ASCII): magic + 13 eight-digit hex fields.
/// uid/gid/mtime/device numbers are always 0 (the kernel unpacker consumes
/// as root and does not read these fields).
fn write_header<W: Write>(
    w: &mut W,
    name: &str,
    ino: u64,
    mode: u32,
    nlink: u64,
    size: u64,
) -> anyhow::Result<()> {
    let namesize = name.len() + 1; // includes NUL
    let fields = [
        ino,
        mode as u64,
        0,
        0,
        nlink,
        0, // mtime
        size,
        0,
        0, // dev major/minor
        0,
        0, // rdev major/minor
        namesize as u64,
        0, // check
    ];
    let mut hdr = String::with_capacity(110);
    hdr.push_str("070701");
    for f in fields {
        hdr.push_str(&format!("{f:08x}"));
    }
    w.write_all(hdr.as_bytes())?;
    w.write_all(name.as_bytes())?;
    w.write_all(&[0])?;
    // Pad header + name to the 4-byte boundary (newc alignment rule)
    let pad = (4 - (110 + namesize) % 4) % 4;
    w.write_all(&vec![0u8; pad])?;
    Ok(())
}

fn write_data<W: Write>(w: &mut W, data: &[u8]) -> anyhow::Result<()> {
    w.write_all(data)?;
    let pad = (4 - data.len() % 4) % 4;
    w.write_all(&vec![0u8; pad])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempRoot;
    use std::os::unix::fs::PermissionsExt;

    fn mk_tree(dir: &Path) {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("lib/modules")).unwrap();
        std::fs::write(dir.join("init"), b"#!/bin/sh\n").unwrap();
        set_mode(&dir.join("init"), 0o755);
        std::fs::write(dir.join("bin/busybox"), b"\x7fELF....").unwrap();
        set_mode(&dir.join("bin/busybox"), 0o755);
        std::os::unix::fs::symlink("busybox", dir.join("bin/sh")).unwrap();
        std::fs::write(dir.join("lib/modules/modules.conf"), "ext4\n").unwrap();
    }

    fn set_mode(p: &Path, mode: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Parses out the (name, mode, size, data) sequence (alignment mirrors the writer side).
    fn parse(cpio: &[u8]) -> Vec<(String, u32, u64, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 0usize;
        loop {
            let hdr = &cpio[i..i + 110];
            assert_eq!(&hdr[..6], b"070701", "newc magic");
            let hex = |k: usize| {
                u64::from_str_radix(
                    std::str::from_utf8(&hdr[6 + k * 8..6 + (k + 1) * 8]).unwrap(),
                    16,
                )
                .unwrap()
            };
            let mode = hex(1) as u32;
            let size = hex(6);
            let namesize = hex(11) as usize - 1; // strip NUL
            let name = String::from_utf8(cpio[i + 110..i + 110 + namesize].to_vec()).unwrap();
            if name == "TRAILER!!!" {
                break;
            }
            let data_start = i + align4(110 + namesize + 1);
            let data = cpio[data_start..data_start + size as usize].to_vec();
            out.push((name, mode, size, data));
            i = data_start + align4(size as usize);
        }
        out
    }

    #[test]
    fn roundtrip_newc_entries() {
        let dir = TempRoot::new("cpio-rt");
        mk_tree(&dir);
        let out = dir.join("initrd.img");
        pack_dir_gzip(&dir, &out).unwrap();

        // gzip magic
        let raw = std::fs::read(&out).unwrap();
        assert_eq!(&raw[..2], &[0x1f, 0x8b]);

        // Assert entry by entry after decompression
        let mut dec = flate2::read::GzDecoder::new(&raw[..]);
        let mut cpio = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut cpio).unwrap();
        let entries = parse(&cpio);
        let get = |n: &str| entries.iter().find(|(name, ..)| name == n).unwrap();
        let (_, mode, _, data) = get("./init");
        assert_eq!(*mode, 0o100755);
        assert_eq!(data, b"#!/bin/sh\n");
        let (_, mode, _, data) = get("./bin/sh");
        assert_eq!(*mode, 0o120777);
        assert_eq!(data, b"busybox");
        let (_, mode, ..) = get("./lib/modules");
        assert_eq!(*mode, 0o040755);
        for expect in ["./bin", "./bin/busybox", "./init", "./lib", "./lib/modules"] {
            assert!(
                entries.iter().any(|(name, ..)| name == expect),
                "missing {expect}"
            );
        }
    }

    /// Determinism: two packs of the same tree are byte-identical (mtime
    /// always 0 + path sort).
    /// Outputs go outside the tree — writing inside would be swallowed by the next collection.
    #[test]
    fn pack_is_deterministic() {
        let base = TempRoot::new("cpio-det");
        let dir = base.join("tree");
        std::fs::create_dir_all(&dir).unwrap();
        mk_tree(&dir);
        let a = base.join("a.img");
        let b = base.join("b.img");
        pack_dir_gzip(&dir, &a).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100)); // cross a second boundary
        set_mode(&dir.join("init"), 0o755); // trigger an mtime change
        pack_dir_gzip(&dir, &b).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    }

    fn align4(n: usize) -> usize {
        n + (4 - n % 4) % 4
    }
}
