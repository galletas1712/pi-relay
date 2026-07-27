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
//! a copy at a file outside the tree. The target side keeps the same discipline:
//! every destination directory is created with `mkdirat` and opened with
//! `openat(O_NOFOLLOW)` against the directory fd above it, and files with
//! `openat(O_CREAT | O_EXCL | O_NOFOLLOW)`, so a symlink planted in the target
//! tree is unlinked and replaced rather than written through. Order is the
//! sorted relative path, so a truncated copy is reproducible.

use std::collections::BinaryHeap;
use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

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

/// Target directories are only ever opened to `openat` beneath them, and
/// `O_DIRECTORY` plus `O_NOFOLLOW` means a symlink planted at that name fails
/// instead of moving the rest of the copy outside the target tree.
const TARGET_DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The modes `std::fs` would have used, minus the process umask that `mkdirat`
/// and `openat` apply for us.
const DIR_MODE: Mode = Mode::RWXU.union(Mode::RWXG).union(Mode::RWXO);
const FILE_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::RGRP)
    .union(Mode::WGRP)
    .union(Mode::ROTH)
    .union(Mode::WOTH);

/// `O_EXCL` refuses an existing name — including a symlink, dangling or not — so
/// the copy can never write through one.
const TARGET_FILE_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Copy every regular file under `source` into `target_root/target_rel`,
/// preserving relative paths. Every target directory is created lazily, so an
/// empty source tree writes nothing at all. Returns `None` when `source` does
/// not exist (the agent removed the staging dir).
///
/// The target is given as a root plus a relative path rather than one path
/// because the whole relative chain is created and opened one component at a
/// time from the root's fd, the same way the source is walked.
pub(super) async fn copy_artifact_tree(
    source: &Path,
    target_root: &Path,
    target_rel: &Path,
) -> Result<Option<CopiedArtifacts>> {
    let source = source.to_path_buf();
    let target_root = target_root.to_path_buf();
    let target_rel = target_rel.to_path_buf();
    tokio::task::spawn_blocking(move || {
        copy_artifact_tree_blocking(&source, &target_root, &target_rel)
    })
    .await
    .context("copy artifact tree task failed")?
}

fn copy_artifact_tree_blocking(
    source: &Path,
    target_root: &Path,
    target_rel: &Path,
) -> Result<Option<CopiedArtifacts>> {
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
    let mut walk = Walk {
        target_root: target_root.to_path_buf(),
        target_rel: target_rel.to_path_buf(),
        target_base_fd: None,
        copied: CopiedArtifacts::default(),
        bytes: 0,
        examined: 0,
    };
    copy_dir(source_fd, Path::new(""), 0, &mut walk)?;
    Ok(Some(walk.copied))
}

/// The mutable state of one copy: what has been recorded so far, and the target
/// the recorded paths land under.
struct Walk {
    /// The parent's session cwd, which the daemon owns; everything below it is
    /// reached component by component so nothing the parent staged there can
    /// redirect a write outside it.
    target_root: PathBuf,
    target_rel: PathBuf,
    /// `target_root/target_rel`, opened on the first file actually copied so an
    /// empty staging tree leaves no empty `artifacts/` directory behind.
    target_base_fd: Option<OwnedFd>,
    copied: CopiedArtifacts,
    bytes: u64,
    /// Every entry the walk has looked at, copied or not. Directories cost
    /// nothing against `files`/`skipped`, so this is what keeps an agent-authored
    /// tree of a million empty directories from stalling the walk — and with it
    /// the parent's cwd mutation guard — for minutes.
    examined: usize,
}

impl Walk {
    fn budget(&self) -> usize {
        MAX_ARTIFACT_FILES.saturating_sub(self.examined)
    }

    /// The target directory for `relative`, creating and opening each component
    /// below the root with `mkdirat`/`openat(O_NOFOLLOW)`. The chain is rebuilt
    /// per source directory rather than cached, which costs at most a handful of
    /// opens per directory that actually receives a file.
    fn target_dir(&mut self, relative: &Path) -> Result<OwnedFd> {
        if self.target_base_fd.is_none() {
            let root = open_target_dir(rustix::fs::CWD, &path_cstring(&self.target_root)?)
                .with_context(|| format!("open {}", self.target_root.display()))?;
            self.target_base_fd = Some(self.descend(root, &self.target_rel.clone())?);
        }
        let base = self
            .target_base_fd
            .as_ref()
            .expect("target base fd")
            .try_clone()
            .context("clone artifact target dir")?;
        self.descend(base, relative)
    }

    fn descend(&self, mut dir: OwnedFd, relative: &Path) -> Result<OwnedFd> {
        for component in relative.iter() {
            dir = create_dir_at(&dir, component).with_context(|| {
                format!(
                    "create artifact dir {}",
                    self.target_root
                        .join(&self.target_rel)
                        .join(relative)
                        .display()
                )
            })?;
        }
        Ok(dir)
    }
}

/// Copy the contents of the already-opened directory `source` into the target
/// directory for `relative`.
///
/// `depth` is the number of path segments already walked, so an entry of this
/// directory sits at `depth + 1`. Taking the source as an owned fd rather than a
/// path is what makes the walk race-proof: children are reached with `openat`
/// relative to it, never by re-resolving a path the child can rename underneath
/// us.
fn copy_dir(source: OwnedFd, relative: &Path, depth: usize, walk: &mut Walk) -> Result<()> {
    let DirNames { names, overflowed } = read_dir_names(&source, walk.budget())?;
    // Opened on the first file actually copied, so an empty source directory
    // creates nothing in the target.
    let mut target_dir = None;

    for name in names {
        if walk.copied.truncated {
            return Ok(());
        }
        // Every entry costs the same against the bound whether it is copied,
        // skipped, or merely descended into: traversal is work too, and
        // `skipped` is written into the parent's durable workspace just like
        // `files`.
        if walk.examined >= MAX_ARTIFACT_FILES {
            walk.copied.truncated = true;
            return Ok(());
        }
        walk.examined += 1;
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
            walk.copied.skipped.push(display);
            continue;
        }
        // A rename racing this open can only make it fail (a symlink now:
        // ELOOP) or hand back the new inode — never redirect a later by-name
        // lookup, because there is no later lookup.
        let Ok(child) = openat_nofollow(&source, &name) else {
            walk.copied.skipped.push(display);
            continue;
        };
        let Ok(metadata) = rustix::fs::fstat(&child) else {
            walk.copied.skipped.push(display);
            continue;
        };
        if file_type(&metadata) == FileType::Directory {
            copy_dir(child, &child_relative, depth + 1, walk)?;
        } else if file_type(&metadata) == FileType::RegularFile {
            let size = metadata.st_size as u64;
            if walk.bytes + size > MAX_ARTIFACT_BYTES {
                walk.copied.truncated = true;
                return Ok(());
            }
            let dir = match &target_dir {
                Some(dir) => dir,
                None => target_dir.insert(walk.target_dir(relative)?),
            };
            let target_file = create_file_at(dir, &name)
                .with_context(|| format!("create artifact {display} in the parent workspace"))?;
            let written = copy_file(child, size, target_file)
                .with_context(|| format!("copy artifact {display} to the parent workspace"))?;
            walk.bytes += written;
            walk.copied.files.push(CopiedArtifactFile {
                path: display,
                bytes: written,
            });
        } else {
            walk.copied.skipped.push(display);
        }
    }
    // A directory holding more entries than the walk could still afford to
    // examine is itself a truncation, even if nothing in it was copyable.
    walk.copied.truncated |= overflowed;
    Ok(())
}

/// The `budget` alphabetically first names in `source`, and whether there were
/// more.
///
/// Listing is the one step whose cost the child sets rather than our bounds — a
/// directory can hold millions of entries — so only the names the walk can still
/// afford to examine are kept. Sorting is what makes a truncated copy
/// reproducible.
struct DirNames {
    names: Vec<OsString>,
    overflowed: bool,
}

fn read_dir_names(source: &OwnedFd, budget: usize) -> Result<DirNames> {
    let mut heap: BinaryHeap<OsString> = BinaryHeap::new();
    let mut overflowed = false;
    for entry in rustix::fs::Dir::read_from(source).context("read artifact dir")? {
        let entry = entry.context("read artifact dir entry")?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        heap.push(OsString::from_vec(name.to_vec()));
        if heap.len() > budget {
            heap.pop();
            overflowed = true;
        }
    }
    let mut names = heap.into_vec();
    names.sort();
    Ok(DirNames { names, overflowed })
}

/// Copy at most `size` bytes of the already-opened `source` to the already-opened
/// `target`, returning what was actually written. The child may still be
/// appending, so the cap is what keeps the manifest's byte accounting equal to
/// the bytes on disk and inside the bound that was checked against `size`.
fn copy_file(source: OwnedFd, size: u64, target: OwnedFd) -> Result<u64> {
    use std::io::Read;

    let mut source = std::fs::File::from(source).take(size);
    let mut target = std::fs::File::from(target);
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

fn open_target_dir(dir: BorrowedFd<'_>, name: &CString) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat(dir, name.as_c_str(), TARGET_DIR_FLAGS, Mode::empty())
}

/// Create `name` under `dir` and open it. An existing real directory is reused;
/// anything else at that name — a symlink planted in the target, a leftover file
/// — is unlinked and replaced, so the copy can never descend through it out of
/// the target tree.
fn create_dir_at(dir: &OwnedFd, name: &OsStr) -> Result<OwnedFd> {
    let name = target_name(name)?;
    match rustix::fs::mkdirat(dir.as_fd(), name.as_c_str(), DIR_MODE) {
        Ok(()) => return Ok(open_target_dir(dir.as_fd(), &name)?),
        Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(error.into()),
    }
    if let Ok(existing) = open_target_dir(dir.as_fd(), &name) {
        return Ok(existing);
    }
    rustix::fs::unlinkat(dir.as_fd(), name.as_c_str(), rustix::fs::AtFlags::empty())?;
    rustix::fs::mkdirat(dir.as_fd(), name.as_c_str(), DIR_MODE)?;
    Ok(open_target_dir(dir.as_fd(), &name)?)
}

/// Create `name` under `dir` for writing. Any existing name — a leftover from an
/// earlier handback, or a symlink planted in the target — is removed rather than
/// written through; `unlinkat` never follows, so this cannot reach outside the
/// target tree, and `O_EXCL` then refuses anything that reappears.
fn create_file_at(dir: &OwnedFd, name: &OsStr) -> Result<OwnedFd> {
    let name = target_name(name)?;
    match rustix::fs::unlinkat(dir.as_fd(), name.as_c_str(), rustix::fs::AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(rustix::fs::openat(
        dir.as_fd(),
        name.as_c_str(),
        TARGET_FILE_FLAGS,
        FILE_MODE,
    )?)
}

fn target_name(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).context("artifact name contains a NUL")
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

    /// Every test copies `<root>/src` into `<root>/dst`, the shape the daemon
    /// uses: a target relative path under a directory the daemon owns.
    async fn copy(root: &Path) -> Option<CopiedArtifacts> {
        copy_artifact_tree(&root.join("src"), root, Path::new("dst"))
            .await
            .expect("copy")
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

        let copied = copy(&root).await.expect("present");

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

        let copied = copy(&root).await.expect("present");

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
        for index in 0..(MAX_ARTIFACT_FILES + 10) {
            write(&source.join(format!("f{index:04}.txt")), "x");
        }

        let copied = copy(&root).await.expect("present");

        assert!(copied.truncated);
        assert_eq!(copied.files.len(), MAX_ARTIFACT_FILES);
        assert_eq!(copied.files[0].path, "f0000.txt");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn truncates_at_the_byte_bound() {
        let root = temp_root("byte-bound");
        let source = root.join("src");
        let big = "x".repeat((MAX_ARTIFACT_BYTES / 2 + 1) as usize);
        write(&source.join("a.bin"), &big);
        write(&source.join("b.bin"), &big);
        write(&source.join("c.txt"), "tail");

        let copied = copy(&root).await.expect("present");

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

    /// A symlink planted in the *target* used to be written through, landing
    /// child-authored content outside the parent's artifacts directory. Nothing
    /// in the target is followed: the link is removed and replaced by the file.
    #[tokio::test]
    async fn a_symlink_in_the_target_is_replaced_rather_than_written_through() {
        let root = temp_root("target-symlink");
        let source = root.join("src");
        let target = root.join("dst");
        let outside = root.join("outside");
        write(&outside.join("victim.txt"), "innocent");
        write(&outside.join("elsewhere/x.txt"), "innocent");
        write(&source.join("victim.txt"), "child");
        write(&source.join("sub/x.txt"), "child");
        std::fs::create_dir_all(&target).expect("target");
        std::os::unix::fs::symlink(outside.join("victim.txt"), target.join("victim.txt"))
            .expect("file symlink");
        std::os::unix::fs::symlink(outside.join("elsewhere"), target.join("sub"))
            .expect("dir symlink");

        let copied = copy(&root).await.expect("present");

        assert_eq!(copied.files.len(), 2);
        assert_eq!(
            std::fs::read_to_string(outside.join("victim.txt")).expect("read"),
            "innocent"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("elsewhere/x.txt")).expect("read"),
            "innocent"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("victim.txt")).expect("read"),
            "child"
        );
        assert!(!target.join("sub").is_symlink());
        assert_eq!(
            std::fs::read_to_string(target.join("sub/x.txt")).expect("read"),
            "child"
        );
        std::fs::remove_dir_all(root).ok();
    }

    /// Directories cost nothing against `files`/`skipped`, so a tree of nothing
    /// but empty directories used to be walked in full — minutes of work under
    /// the parent's cwd mutation guard for a manifest of zero entries.
    #[tokio::test]
    async fn a_directory_only_tree_terminates_under_the_bound() {
        let root = temp_root("dir-bomb");
        let source = root.join("src");
        std::fs::create_dir_all(&source).expect("source");
        for index in 0..20_000 {
            std::fs::create_dir(source.join(format!("d{index:05}"))).expect("dir");
        }

        let started = std::time::Instant::now();
        let copied = copy(&root).await.expect("present");

        assert!(copied.truncated);
        assert!(copied.files.is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "walk took {:?}",
            started.elapsed()
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn stops_at_the_depth_bound_instead_of_overflowing_the_stack() {
        let root = temp_root("depth");
        let source = root.join("src");
        let mut deep = source.clone();
        for _ in 0..2000 {
            deep = deep.join("d");
        }
        write(&deep.join("buried.txt"), "buried");
        write(&source.join("top.txt"), "top");

        let copied = copy(&root).await.expect("present");

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
        std::fs::create_dir_all(&source).expect("source");
        for index in 0..(MAX_ARTIFACT_FILES + 10) {
            std::os::unix::fs::symlink(root.join("nowhere"), source.join(format!("l{index:04}")))
                .expect("symlink");
        }

        let copied = copy(&root).await.expect("present");

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

        let copied = copy(&root).await.expect("present");

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
            let target_rel = PathBuf::from(format!("dst{round}"));
            let target = root.join(&target_rel);
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
            copy_artifact_tree(&source, &root, &target_rel)
                .await
                .expect("copy");
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
            copy_artifact_tree(&source, &root, Path::new("dst")),
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

        let copied = copy_artifact_tree(&root.join("missing"), &root, Path::new("dst"))
            .await
            .expect("copy");

        assert!(copied.is_none());
        assert!(!root.join("dst").exists());
        std::fs::remove_dir_all(root).ok();
    }
}
