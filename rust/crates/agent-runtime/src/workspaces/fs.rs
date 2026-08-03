//! Confined, read-only browsing of a session cwd.
//!
//! Public browser RPCs never see host paths. Callers pass cwd-relative paths;
//! this module opens the session cwd through `cap_std` and stays beneath it.

use std::{
    io::{Read, Seek, SeekFrom},
    path::{Component, Path},
};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use cap_std::{
    ambient_authority,
    fs::{Dir, FileType, MetadataExt},
};

pub const DEFAULT_LIST_LIMIT: u32 = 200;
pub const MAX_LIST_LIMIT: u32 = 500;
pub const DEFAULT_CHUNK_BYTES: u32 = 1024 * 1024;
pub const MAX_CHUNK_BYTES: u32 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirEntryKind {
    File,
    Directory,
    Other,
}

impl DirEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: DirEntryKind,
    pub size: Option<u64>,
    pub mtime_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirListing {
    pub path: String,
    pub entries: Vec<DirEntry>,
    pub next_after_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePrefix {
    pub path: String,
    pub content_base64: String,
    pub byte_len: u64,
    pub total_size: u64,
    pub eof: bool,
    pub mtime_ms: Option<u64>,
}

/// Validate a cwd-relative browse path.
///
/// `""` is the cwd root. Otherwise require slash-separated nonempty normal
/// components with no `.`, `..`, absolute forms, controls, or NUL.
pub fn validate_browse_path(path: &str) -> Result<String> {
    if path.is_empty() {
        return Ok(String::new());
    }
    if path.contains('\0') {
        bail!("path contains NUL");
    }
    if path.starts_with('/') || path.starts_with('\\') {
        bail!("path must be relative");
    }
    if path.chars().any(|ch| ch.is_control() || ch == '\\') {
        bail!("path contains illegal characters");
    }

    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => {
                let name = part
                    .to_str()
                    .ok_or_else(|| anyhow!("path component is not UTF-8"))?;
                if name.is_empty() || name == "." || name == ".." {
                    bail!("path contains illegal component");
                }
                if name.contains('/') {
                    bail!("path contains illegal component");
                }
                parts.push(name.to_string());
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                bail!("path must be relative and normal");
            }
        }
    }
    if parts.is_empty() {
        bail!("path must be relative and normal");
    }
    // Reject empty segments produced by "//" (Path::components collapses them
    // on Unix, so also reject raw doubles / trailing slash forms).
    if path.contains("//") || path.ends_with('/') {
        bail!("path must be relative and normal");
    }
    Ok(parts.join("/"))
}

pub fn list_dir(
    cwd: &Path,
    path: &str,
    after_name: Option<&str>,
    limit: u32,
) -> Result<DirListing> {
    let normalized = validate_browse_path(path)?;
    let limit = clamp_list_limit(limit);
    let root = open_cwd(cwd)?;
    let root_dev = root.dir_metadata()?.dev();
    let dir = open_browse_dir(&root, &normalized, root_dev)?;

    let mut entries = Vec::new();
    for entry in dir.entries().context("read directory entries")? {
        let entry = entry.context("read directory entry")?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("directory entry name is not UTF-8"))?;
        if name == "." || name == ".." {
            continue;
        }
        // DirEntry::metadata/file_type use lstat on Unix and do not follow.
        let meta = entry
            .metadata()
            .with_context(|| format!("stat directory entry {name}"))?;
        if meta.dev() != root_dev {
            // Nested mounts are classified as other and cannot be opened.
            entries.push(DirEntry {
                name,
                kind: DirEntryKind::Other,
                size: None,
                mtime_ms: mtime_ms(&meta),
            });
            continue;
        }
        let kind = classify_file_type(meta.file_type());
        let size = if kind == DirEntryKind::File {
            Some(meta.len())
        } else {
            None
        };
        entries.push(DirEntry {
            name,
            kind,
            size,
            mtime_ms: mtime_ms(&meta),
        });
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let start = match after_name {
        Some(after) => {
            let pos = entries
                .iter()
                .position(|entry| entry.name.as_str() == after)
                .ok_or_else(|| anyhow!("after_name not found in directory"))?;
            pos + 1
        }
        None => 0,
    };
    let end = (start + limit as usize).min(entries.len());
    let page = entries[start..end].to_vec();
    let next_after_name = if end < entries.len() {
        page.last().map(|entry| entry.name.clone())
    } else {
        None
    };

    Ok(DirListing {
        path: normalized,
        entries: page,
        next_after_name,
    })
}

pub fn read_file_range(cwd: &Path, path: &str, offset: u64, max_bytes: u32) -> Result<FilePrefix> {
    let normalized = validate_browse_path(path)?;
    if normalized.is_empty() {
        bail!("path must name a regular file");
    }
    let max_bytes = clamp_chunk_bytes(max_bytes);
    let root = open_cwd(cwd)?;
    let root_dev = root.dir_metadata()?.dev();

    let meta = root
        .symlink_metadata(&normalized)
        .with_context(|| format!("stat file {normalized}"))?;
    if meta.file_type().is_symlink() {
        bail!("refusing to follow symlink");
    }
    if !meta.file_type().is_file() {
        bail!("path is not a regular file");
    }
    if meta.dev() != root_dev {
        bail!("refusing to read across a mount boundary");
    }

    let mut file = root
        .open(&normalized)
        .with_context(|| format!("open file {normalized}"))?;
    // Re-check after open: require a regular file descriptor.
    let opened = file
        .metadata()
        .with_context(|| format!("fstat file {normalized}"))?;
    if !opened.is_file() {
        bail!("path is not a regular file");
    }
    if opened.dev() != root_dev {
        bail!("refusing to read across a mount boundary");
    }

    let total_size = opened.len();
    if offset > total_size {
        bail!("offset is past end of file");
    }
    let remaining = total_size - offset;
    let to_read = (max_bytes as u64).min(remaining) as usize;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("seek file {normalized}"))?;
    }
    let mut buf = vec![0_u8; to_read];
    let mut read_total = 0usize;
    while read_total < to_read {
        match file.read(&mut buf[read_total..]) {
            Ok(0) => break,
            Ok(n) => read_total += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context(format!("read file {normalized}")),
        }
    }
    buf.truncate(read_total);

    Ok(FilePrefix {
        path: normalized,
        content_base64: BASE64.encode(&buf),
        byte_len: read_total as u64,
        total_size,
        eof: offset + (read_total as u64) >= total_size,
        mtime_ms: mtime_ms(&opened),
    })
}

fn open_cwd(cwd: &Path) -> Result<Dir> {
    Dir::open_ambient_dir(cwd, ambient_authority())
        .with_context(|| format!("open session cwd {}", cwd.display()))
}

fn open_browse_dir(root: &Dir, path: &str, root_dev: u64) -> Result<Dir> {
    if path.is_empty() {
        let meta = root.dir_metadata()?;
        if meta.dev() != root_dev {
            bail!("session cwd crossed a mount boundary");
        }
        return root.try_clone().context("clone session cwd directory");
    }

    // Walk one component at a time so each step can reject symlinks and mounts.
    let mut current = root.try_clone().context("clone session cwd directory")?;
    for component in path.split('/') {
        let meta = current
            .symlink_metadata(component)
            .with_context(|| format!("stat path component {component}"))?;
        if meta.file_type().is_symlink() {
            bail!("refusing to follow symlink");
        }
        if !meta.file_type().is_dir() {
            bail!("path is not a directory");
        }
        if meta.dev() != root_dev {
            bail!("refusing to traverse a nested mount");
        }
        current = current
            .open_dir(component)
            .with_context(|| format!("open directory {component}"))?;
        let opened = current.dir_metadata()?;
        if opened.dev() != root_dev {
            bail!("refusing to traverse a nested mount");
        }
    }
    Ok(current)
}

fn classify_file_type(file_type: FileType) -> DirEntryKind {
    if file_type.is_dir() {
        DirEntryKind::Directory
    } else if file_type.is_file() {
        DirEntryKind::File
    } else {
        DirEntryKind::Other
    }
}

fn mtime_ms(meta: &cap_std::fs::Metadata) -> Option<u64> {
    // Prefer the Unix mtime seconds field; convert to ms.
    Some((meta.mtime() as u64).saturating_mul(1000))
}

fn clamp_list_limit(limit: u32) -> u32 {
    if limit == 0 {
        DEFAULT_LIST_LIMIT
    } else {
        limit.min(MAX_LIST_LIMIT)
    }
}

fn clamp_chunk_bytes(max_bytes: u32) -> u32 {
    if max_bytes == 0 {
        DEFAULT_CHUNK_BYTES
    } else {
        max_bytes.min(MAX_CHUNK_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn validate_accepts_root_and_normal_paths() {
        assert_eq!(validate_browse_path("").unwrap(), "");
        assert_eq!(validate_browse_path("src/main.rs").unwrap(), "src/main.rs");
    }

    #[test]
    fn validate_rejects_escapes() {
        assert!(validate_browse_path("..").is_err());
        assert!(validate_browse_path("/abs").is_err());
        assert!(validate_browse_path("a//b").is_err());
        assert!(validate_browse_path("a/").is_err());
        assert!(validate_browse_path("a\0b").is_err());
        assert_eq!(validate_browse_path(".pi-handoff").unwrap(), ".pi-handoff");
        assert_eq!(
            validate_browse_path(".pi-handoff/x").unwrap(),
            ".pi-handoff/x"
        );
    }

    #[test]
    fn list_and_read_regular_files() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), b"fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("README.md"), b"# hi\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".pi-handoff")).unwrap();
        std::fs::write(dir.path().join(".pi-handoff/note.md"), b"handoff\n").unwrap();

        let listing = list_dir(dir.path(), "", None, 200).unwrap();
        let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"README.md"));
        assert!(names.contains(&"src"));
        assert!(names.contains(&".pi-handoff"));

        let file = read_file_range(dir.path(), "README.md", 0, 1024).unwrap();
        assert_eq!(file.byte_len, 5);
        assert!(file.eof);
        assert_eq!(BASE64.decode(&file.content_base64).unwrap(), b"# hi\n");

        let handoff = read_file_range(dir.path(), ".pi-handoff/note.md", 0, 1024).unwrap();
        assert_eq!(
            BASE64.decode(&handoff.content_base64).unwrap(),
            b"handoff\n"
        );
    }

    #[test]
    fn refuses_symlinks() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), b"ok").unwrap();
        symlink("real.txt", dir.path().join("link.txt")).unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        symlink("..", dir.path().join("subdir/up")).unwrap();

        let listing = list_dir(dir.path(), "", None, 200).unwrap();
        let link = listing
            .entries
            .iter()
            .find(|e| e.name == "link.txt")
            .unwrap();
        assert_eq!(link.kind, DirEntryKind::Other);

        assert!(read_file_range(dir.path(), "link.txt", 0, 1024).is_err());
        assert!(list_dir(dir.path(), "subdir/up", None, 200).is_err());
    }

    #[test]
    fn pages_directory_entries() {
        let dir = tempdir().unwrap();
        for name in ["a", "b", "c", "d"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let page1 = list_dir(dir.path(), "", None, 2).unwrap();
        assert_eq!(
            page1
                .entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(page1.next_after_name.as_deref(), Some("b"));
        let page2 = list_dir(dir.path(), "", Some("b"), 2).unwrap();
        assert_eq!(
            page2
                .entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["c", "d"]
        );
        assert!(page2.next_after_name.is_none());
    }

    #[test]
    fn reads_file_ranges() {
        let dir = tempdir().unwrap();
        let body = vec![b'x'; 100];
        std::fs::write(dir.path().join("big.bin"), &body).unwrap();
        let prefix = read_file_range(dir.path(), "big.bin", 0, 16).unwrap();
        assert_eq!(prefix.byte_len, 16);
        assert_eq!(prefix.total_size, 100);
        assert!(!prefix.eof);
        let mid = read_file_range(dir.path(), "big.bin", 90, 16).unwrap();
        assert_eq!(mid.byte_len, 10);
        assert!(mid.eof);
        assert_eq!(BASE64.decode(&mid.content_base64).unwrap(), vec![b'x'; 10]);
    }
}
