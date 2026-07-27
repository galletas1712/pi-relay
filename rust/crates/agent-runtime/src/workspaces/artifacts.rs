//! The bounded, symlink-free artifact copy used to hand a read-only subagent's
//! staging tree to its parent before the child subvolume is destroyed.
//!
//! The walk is curated rather than a `cp -a` plus a repair pass: only regular
//! files are copied and only real directories are traversed. The child owns the
//! staging tree and keeps running while the walk does, so every entry is
//! resolved once, with `openat(O_NOFOLLOW)` relative to the directory fd we are
//! already holding, and both the type check and the size come from `fstat` on
//! that same fd. Nothing is ever looked up by name a second time, so a rename
//! racing the walk can only turn an entry into `skipped` — it can never redirect
//! a copy at a file outside the tree. Order is the sorted relative path, so a
//! truncated copy is reproducible.

use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use agent_runtime_protocol::{CopiedArtifactFile, CopiedArtifacts};
use anyhow::{Context, Result};
use rustix::fs::{FileType, Mode, OFlags, Stat};

const MAX_ARTIFACT_FILES: usize = 200;
const MAX_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
/// The walk is recursive, so an agent-authored `mkdir -p a/a/a/...` chain would
/// otherwise overflow the runtime's stack — a hard `abort()`, not a catchable
/// panic. Both bounds match the read vocabulary (`MAX_ARTIFACT_PATH_SEGMENTS`
/// and `MAX_ARTIFACT_PATH_LEN` in the daemon), so nothing is ever copied that a
/// later `read_handoff_file` would refuse to name.
const MAX_ARTIFACT_DEPTH: usize = 16;
const MAX_ARTIFACT_PATH_LEN: usize = 512;

/// `O_NOFOLLOW` makes a symlink fail to open rather than resolve, and
/// `O_NONBLOCK` keeps a staged fifo from parking the walk until a writer shows
/// up. The opened fd is the only handle used afterwards, for both the `fstat`
/// that decides the entry's type and size and the read that follows.
const OPEN_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// Copy every regular file under `source` into `target`, preserving relative
/// paths. Directories in `target` are created lazily, so an empty source tree
/// writes nothing at all. Returns `None` when `source` does not exist (the agent
/// removed the staging dir).
pub(super) async fn copy_artifact_tree(
    source: &Path,
    target: &Path,
) -> Result<Option<CopiedArtifacts>> {
    let source = source.to_path_buf();
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || copy_artifact_tree_blocking(&source, &target))
        .await
        .context("copy artifact tree task failed")?
}

fn copy_artifact_tree_blocking(source: &Path, target: &Path) -> Result<Option<CopiedArtifacts>> {
    let source_fd = match open_nofollow(rustix::fs::CWD, &path_cstring(source)?) {
        Ok(fd) => fd,
        // A symlinked (ELOOP) or absent staging dir is never followed; treat it
        // as absent, like the agent having removed it.
        Err(rustix::io::Errno::LOOP) | Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", source.display()));
        }
    };
    let metadata =
        rustix::fs::fstat(&source_fd).with_context(|| format!("inspect {}", source.display()))?;
    if file_type(&metadata) != FileType::Directory {
        return Ok(None);
    }
    let mut copied = CopiedArtifacts::default();
    let mut bytes = 0u64;
    copy_dir(source_fd, target, Path::new(""), 0, &mut copied, &mut bytes)?;
    Ok(Some(copied))
}

/// Copy the contents of the already-opened directory `source` into `target`.
///
/// `depth` is the number of path segments already walked, so an entry of this
/// directory sits at `depth + 1`. Taking the source as an owned fd rather than a
/// path is what makes the walk race-proof: children are reached with `openat`
/// relative to it, never by re-resolving a path the child can rename underneath
/// us.
fn copy_dir(
    source: OwnedFd,
    target: &Path,
    relative: &Path,
    depth: usize,
    copied: &mut CopiedArtifacts,
    bytes: &mut u64,
) -> Result<()> {
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(&source).context("read artifact dir")? {
        let entry = entry.context("read artifact dir entry")?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        names.push(OsString::from_vec(name.to_vec()));
    }
    names.sort();

    for name in names {
        if copied.truncated {
            return Ok(());
        }
        // `skipped` is written into the parent's durable workspace just like
        // `files`, so the two share one bound.
        if copied.files.len() + copied.skipped.len() >= MAX_ARTIFACT_FILES {
            copied.truncated = true;
            return Ok(());
        }
        let child_relative = relative.join(&name);
        let display = child_relative.to_string_lossy().to_string();
        // Anything the reader could not later name is recorded, never copied:
        // `safe_handoff_path_segment` trims each segment, so a name that is not
        // valid UTF-8 or that is not equal to its own trim would be written
        // under one name and read under another.
        let nameable = name
            .to_str()
            .is_some_and(|name| name.trim() == name && !name.is_empty());
        if !nameable || depth + 1 > MAX_ARTIFACT_DEPTH || display.len() > MAX_ARTIFACT_PATH_LEN {
            copied.skipped.push(display);
            continue;
        }
        // A rename racing this open can only make it fail (a symlink now:
        // ELOOP) or hand back the new inode — never redirect a later by-name
        // lookup, because there is no later lookup.
        let Ok(child) = openat_nofollow(&source, &name) else {
            copied.skipped.push(display);
            continue;
        };
        let Ok(metadata) = rustix::fs::fstat(&child) else {
            copied.skipped.push(display);
            continue;
        };
        if file_type(&metadata) == FileType::Directory {
            copy_dir(
                child,
                &target.join(&name),
                &child_relative,
                depth + 1,
                copied,
                bytes,
            )?;
        } else if file_type(&metadata) == FileType::RegularFile {
            let size = metadata.st_size as u64;
            if copied.files.len() >= MAX_ARTIFACT_FILES || *bytes + size > MAX_ARTIFACT_BYTES {
                copied.truncated = true;
                return Ok(());
            }
            // Target directories are created lazily, so an empty staging tree
            // leaves no empty `artifacts/` dir behind in the parent.
            std::fs::create_dir_all(target)
                .with_context(|| format!("create artifact dir {}", target.display()))?;
            let child_target = target.join(&name);
            let written = copy_file(child, size, &child_target).with_context(|| {
                format!("copy artifact {display} to {}", child_target.display())
            })?;
            *bytes += written;
            copied.files.push(CopiedArtifactFile {
                path: display,
                bytes: written,
            });
        } else {
            copied.skipped.push(display);
        }
    }
    Ok(())
}

/// Copy at most `size` bytes of the already-opened `source` to `target`,
/// returning what was actually written. The child may still be appending, so the
/// cap is what keeps the manifest's byte accounting equal to the bytes on disk
/// and inside the bound that was checked against `size`.
fn copy_file(source: OwnedFd, size: u64, target: &Path) -> Result<u64> {
    use std::io::Read;

    let mut source = std::fs::File::from(source).take(size);
    let mut target = std::fs::File::create(target)?;
    Ok(std::io::copy(&mut source, &mut target)?)
}

fn open_nofollow(dir: BorrowedFd<'_>, name: &CString) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat(dir, name.as_c_str(), OPEN_FLAGS, Mode::empty())
}

fn openat_nofollow(dir: &OwnedFd, name: &OsStr) -> Result<OwnedFd> {
    Ok(open_nofollow(
        dir.as_fd(),
        &CString::new(name.as_bytes()).context("artifact name contains a NUL")?,
    )?)
}

fn file_type(metadata: &Stat) -> FileType {
    FileType::from_raw_mode(metadata.st_mode)
}

fn path_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("artifact path contains a NUL")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
        std::fs::write(path, contents).expect("write");
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pi-artifacts-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        root
    }

    #[tokio::test]
    async fn copies_a_nested_tree_preserving_relative_paths() {
        let root = temp_root("nested");
        let source = root.join("src");
        let target = root.join("dst");
        write(&source.join("notes.md"), "top");
        write(&source.join("deep/inner/data.txt"), "inner");

        let copied = copy_artifact_tree(&source, &target)
            .await
            .expect("copy")
            .expect("present");

        assert_eq!(
            copied
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["deep/inner/data.txt", "notes.md"]
        );
        assert!(!copied.truncated);
        assert_eq!(
            std::fs::read_to_string(target.join("deep/inner/data.txt")).expect("read"),
            "inner"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn skips_symlinks_and_never_traverses_a_symlinked_directory() {
        let root = temp_root("symlink");
        let source = root.join("src");
        let target = root.join("dst");
        let outside = root.join("outside");
        write(&outside.join("secret.txt"), "secret");
        write(&source.join("keep.txt"), "keep");
        std::os::unix::fs::symlink(outside.join("secret.txt"), source.join("link.txt"))
            .expect("file symlink");
        std::os::unix::fs::symlink(&outside, source.join("dir")).expect("dir symlink");

        let copied = copy_artifact_tree(&source, &target)
            .await
            .expect("copy")
            .expect("present");

        assert_eq!(
            copied
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["keep.txt"]
        );
        assert_eq!(
            copied.skipped,
            vec!["dir".to_string(), "link.txt".to_string()]
        );
        assert!(!target.join("link.txt").exists());
        assert!(!target.join("dir").exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("secret.txt")).expect("read"),
            "secret"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn truncates_deterministically_at_the_file_bound() {
        let root = temp_root("file-bound");
        let source = root.join("src");
        let target = root.join("dst");
        for index in 0..(MAX_ARTIFACT_FILES + 10) {
            write(&source.join(format!("f{index:04}.txt")), "x");
        }

        let copied = copy_artifact_tree(&source, &target)
            .await
            .expect("copy")
            .expect("present");

        assert!(copied.truncated);
        assert_eq!(copied.files.len(), MAX_ARTIFACT_FILES);
        assert_eq!(copied.files[0].path, "f0000.txt");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn truncates_at_the_byte_bound() {
        let root = temp_root("byte-bound");
        let source = root.join("src");
        let target = root.join("dst");
        let big = "x".repeat((MAX_ARTIFACT_BYTES / 2 + 1) as usize);
        write(&source.join("a.bin"), &big);
        write(&source.join("b.bin"), &big);
        write(&source.join("c.txt"), "tail");

        let copied = copy_artifact_tree(&source, &target)
            .await
            .expect("copy")
            .expect("present");

        assert!(copied.truncated);
        assert_eq!(
            copied
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["a.bin"]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn stops_at_the_depth_bound_instead_of_overflowing_the_stack() {
        let root = temp_root("depth");
        let source = root.join("src");
        let target = root.join("dst");
        let mut deep = source.clone();
        for _ in 0..2000 {
            deep = deep.join("d");
        }
        write(&deep.join("buried.txt"), "buried");
        write(&source.join("top.txt"), "top");

        let copied = copy_artifact_tree(&source, &target)
            .await
            .expect("copy")
            .expect("present");

        assert_eq!(
            copied
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["top.txt"]
        );
        assert_eq!(
            copied.skipped,
            vec!["d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d".to_string()]
        );
        assert!(!copied.truncated);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn skipped_entries_count_against_the_file_bound() {
        let root = temp_root("skip-bound");
        let source = root.join("src");
        let target = root.join("dst");
        std::fs::create_dir_all(&source).expect("source");
        for index in 0..(MAX_ARTIFACT_FILES + 10) {
            std::os::unix::fs::symlink(root.join("nowhere"), source.join(format!("l{index:04}")))
                .expect("symlink");
        }

        let copied = copy_artifact_tree(&source, &target)
            .await
            .expect("copy")
            .expect("present");

        assert!(copied.truncated);
        assert_eq!(copied.skipped.len(), MAX_ARTIFACT_FILES);
        assert!(copied.files.is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn skips_names_the_reader_could_not_resolve() {
        let root = temp_root("names");
        let source = root.join("src");
        let target = root.join("dst");
        // `safe_handoff_path_segment` trims, so these would be written under one
        // name and read under another; a non-UTF-8 name has no readable form.
        write(&source.join(" "), "space");
        write(&source.join(" lead.txt"), "lead");
        write(&source.join("trail.txt "), "trail");
        write(
            &source.join(std::ffi::OsStr::from_bytes(b"bad\xff.txt")),
            "bad",
        );
        write(&source.join("good.txt"), "good");

        let copied = copy_artifact_tree(&source, &target)
            .await
            .expect("copy")
            .expect("present");

        assert_eq!(
            copied
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["good.txt"]
        );
        assert_eq!(copied.skipped.len(), 4);
        for file in &copied.files {
            assert!(target.join(&file.path).exists(), "{} missing", file.path);
        }
        std::fs::remove_dir_all(root).ok();
    }

    /// Renaming a symlink over a staged entry after the walk has listed the
    /// directory but before it reaches that entry used to redirect the copy at a
    /// file outside the tree — and to defeat the byte bound, since the size came
    /// from the link and the bytes from its target.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rename_racing_the_walk_never_copies_from_outside_the_tree() {
        let root = temp_root("race");
        let outside = root.join("outside");
        write(&outside.join("secret.txt"), "SECRET");
        write(&outside.join("secretdir/inner.txt"), "SECRETDIR");

        for round in 0..20 {
            let source = root.join(format!("src{round}"));
            let target = root.join(format!("dst{round}"));
            // Decoys, so the copy phase is long enough to be raced.
            for index in 0..100 {
                write(&source.join(format!("a{index:04}.txt")), &"x".repeat(4096));
            }
            write(&source.join("z_file.txt"), "harmless");
            write(&source.join("z_dir/inner.txt"), "harmless");
            let file_link = root.join(format!("flink{round}"));
            let dir_link = root.join(format!("dlink{round}"));
            std::os::unix::fs::symlink(outside.join("secret.txt"), &file_link).expect("link");
            std::os::unix::fs::symlink(outside.join("secretdir"), &dir_link).expect("link");
            let stash = root.join(format!("stash{round}"));
            let (file_path, dir_path) = (source.join("z_file.txt"), source.join("z_dir"));

            let swapper = std::thread::spawn(move || {
                // Long enough for the walk to have listed the real entries.
                std::thread::sleep(std::time::Duration::from_micros(300));
                std::fs::rename(&dir_path, &stash).ok();
                std::fs::rename(&dir_link, &dir_path).ok();
                std::fs::rename(&file_link, &file_path).ok();
            });
            copy_artifact_tree(&source, &target).await.expect("copy");
            swapper.join().expect("join");

            assert_ne!(
                std::fs::read_to_string(target.join("z_file.txt")).unwrap_or_default(),
                "SECRET"
            );
            assert!(!target.join("z_dir/inner.txt").exists());
        }
        std::fs::remove_dir_all(root).ok();
    }

    /// A fifo with no writer would block `open` forever without `O_NONBLOCK`,
    /// stalling the handback of every other artifact.
    #[tokio::test]
    async fn a_writerless_fifo_neither_blocks_nor_is_copied() {
        let root = temp_root("fifo");
        let source = root.join("src");
        let target = root.join("dst");
        write(&source.join("keep.txt"), "keep");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            source.join("pipe"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR,
            0,
        )
        .expect("fifo");

        let copied = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            copy_artifact_tree(&source, &target),
        )
        .await
        .expect("copy did not block")
        .expect("copy")
        .expect("present");

        assert_eq!(
            copied
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["keep.txt"]
        );
        assert_eq!(copied.skipped, vec!["pipe".to_string()]);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn absent_source_copies_nothing() {
        let root = temp_root("absent");

        let copied = copy_artifact_tree(&root.join("missing"), &root.join("dst"))
            .await
            .expect("copy");

        assert!(copied.is_none());
        assert!(!root.join("dst").exists());
        std::fs::remove_dir_all(root).ok();
    }
}
