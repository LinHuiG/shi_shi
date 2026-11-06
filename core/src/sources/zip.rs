//! Plugin package extraction. A ZIP is checked as a whole before anything is
//! written: entry count, paths, Windows reserved names, case-insensitive
//! duplicates, links, expanded size and exactly one `plugin.json` at the root
//! or one directory down. Only stored and deflate entries without encryption
//! or ZIP64 are accepted; every entry's CRC is verified.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

const MAX_PACKAGE: u64 = 256 * 1024 * 1024;
pub const ONE_MANIFEST: &str =
    "ZIP 中需要且只能包含一个 plugin.json。请安装编译后的插件 ZIP 包，源码 ZIP 不能直接安装。";
pub const MANIFEST_DEPTH: &str = "该 ZIP 不是可安装的插件包：plugin.json 需要位于 ZIP 根目录或第一层插件目录。若选择了源码 ZIP，请改选包含 DLL 和依赖文件的插件安装包。";

struct Entry {
    name: String,
    dir: bool,
    method: u32,
    crc: u32,
    compressed: u32,
    size: u32,
    local: u32,
}

/// The little-endian integer of `n` bytes at `at`.
fn le(b: &[u8], at: usize, n: usize) -> Result<u32> {
    let bytes = b.get(at..at + n).ok_or_else(|| Error::new("ZIP 结构不完整"))?;
    Ok(bytes.iter().rev().fold(0, |v, &x| v << 8 | x as u32))
}

fn reserved(segment: &str) -> bool {
    let upper = segment.to_ascii_uppercase();
    let stem = upper.split('.').next().unwrap_or("");
    matches!(stem, "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
}

fn check_name(name: &str) -> Result<()> {
    let unsafe_path = || Error(format!("插件包包含不安全的路径：{name}"));
    if name.is_empty() || name.contains('\\') || name.starts_with('/') {
        return Err(unsafe_path());
    }
    for segment in name.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.ends_with(' ')
            || segment.ends_with('.')
            || segment.chars().any(|c| c.is_control() || "<>:\"|?*".contains(c))
            || reserved(segment)
        {
            return Err(unsafe_path());
        }
    }
    Ok(())
}

fn central(bytes: &[u8]) -> Result<Vec<Entry>> {
    let tail = bytes.len().saturating_sub(65_557);
    let eocd = (tail..bytes.len().saturating_sub(21))
        .rev()
        .find(|&i| bytes[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
        .ok_or_else(|| Error::new("不是有效的 ZIP 文件"))?;
    let count = le(bytes, eocd + 10, 2)?;
    let offset = le(bytes, eocd + 16, 4)?;
    if count == 0xFFFF || offset == 0xFFFF_FFFF {
        crate::bail!("不支持 ZIP64 插件包");
    }
    let mut at = offset as usize;
    let mut entries = vec![];
    for _ in 0..count {
        if le(bytes, at, 4)? != 0x0201_4b50 {
            crate::bail!("ZIP 目录损坏");
        }
        let flags = le(bytes, at + 8, 2)?;
        let method = le(bytes, at + 10, 2)?;
        let crc = le(bytes, at + 16, 4)?;
        let compressed = le(bytes, at + 20, 4)?;
        let size = le(bytes, at + 24, 4)?;
        let name_len = le(bytes, at + 28, 2)? as usize;
        let extra_len = le(bytes, at + 30, 2)? as usize;
        let comment_len = le(bytes, at + 32, 2)? as usize;
        let external = le(bytes, at + 38, 4)?;
        let local = le(bytes, at + 42, 4)?;
        let raw = bytes.get(at + 46..at + 46 + name_len).ok_or_else(|| Error::new("ZIP 目录损坏"))?;
        let full = std::str::from_utf8(raw).map_err(|_| Error::new("插件包文件名不是 UTF-8"))?;
        if flags & 1 != 0 {
            crate::bail!("不支持加密的插件包");
        }
        if compressed == 0xFFFF_FFFF || size == 0xFFFF_FFFF || local == 0xFFFF_FFFF {
            crate::bail!("不支持 ZIP64 插件包");
        }
        if (external >> 16) & 0xF000 == 0xA000 {
            crate::bail!("插件包不允许包含链接：{full}");
        }
        let dir = full.ends_with('/');
        entries.push(Entry { name: full.trim_end_matches('/').to_string(), dir, method, crc, compressed, size, local });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

fn data(bytes: &[u8], entry: &Entry) -> Result<Vec<u8>> {
    let at = entry.local as usize;
    if le(bytes, at, 4)? != 0x0403_4b50 {
        crate::bail!("ZIP 条目损坏：{}", entry.name);
    }
    let start = at + 30 + le(bytes, at + 26, 2)? as usize + le(bytes, at + 28, 2)? as usize;
    let raw = bytes
        .get(start..start + entry.compressed as usize)
        .ok_or_else(|| Error(format!("ZIP 条目不完整：{}", entry.name)))?;
    let out = match entry.method {
        0 => raw.to_vec(),
        8 => miniz_oxide::inflate::decompress_to_vec_with_limit(raw, entry.size as usize)
            .map_err(|_| Error(format!("ZIP 条目解压失败：{}", entry.name)))?,
        m => crate::bail!("不支持的 ZIP 压缩方式 {m}：{}", entry.name),
    };
    if out.len() != entry.size as usize || crc32fast::hash(&out) != entry.crc {
        crate::bail!("ZIP 条目校验失败：{}", entry.name);
    }
    Ok(out)
}

fn unpack(package: &Path, destination: &Path) -> Result<PathBuf> {
    let bytes = std::fs::read(package)?;
    let entries = central(&bytes)?;
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    let mut manifests = Vec::new();
    for entry in &entries {
        check_name(&entry.name)?;
        if !seen.insert(entry.name.to_lowercase()) {
            crate::bail!("插件包包含重复路径：{}", entry.name);
        }
        total += entry.size as u64;
        if total > MAX_PACKAGE {
            crate::bail!("插件包解压后超过 256 MiB");
        }
        let last = entry.name.rsplit('/').next().unwrap_or("");
        if !entry.dir && last.eq_ignore_ascii_case("plugin.json") {
            manifests.push(entry.name.clone());
        }
    }
    if manifests.len() != 1 {
        return Err(Error::new(ONE_MANIFEST));
    }
    if manifests[0].split('/').count() > 2 {
        return Err(Error::new(MANIFEST_DEPTH));
    }
    for entry in &entries {
        let path = entry.name.split('/').fold(destination.to_path_buf(), |p, s| p.join(s));
        if entry.dir {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        let content = data(&bytes, entry)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
    }
    Ok(manifests[0].split('/').fold(destination.to_path_buf(), |p, s| p.join(s)))
}

/// Extract `package` into the empty `destination` and return the path of its
/// `plugin.json`. The two package-shape errors keep their own wording; any
/// other failure reads `插件 ZIP 解压或校验失败：…`.
pub fn extract(package: &Path, destination: &Path) -> Result<PathBuf> {
    unpack(package, destination).map_err(|e| {
        if e.0 == ONE_MANIFEST || e.0 == MANIFEST_DEPTH {
            e
        } else {
            Error(format!("插件 ZIP 解压或校验失败：{}", e.0.trim_start_matches("文件错误：")))
        }
    })
}

#[cfg(test)]
pub use super::testing::build;

#[cfg(test)]
mod tests;
