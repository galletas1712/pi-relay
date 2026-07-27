//! The bounded, symlink-free artifact copy used to hand a read-only subagent's
//! staging tree to its parent before the child subvolume is destroyed.
//!
//! The walk is curated rather than a `cp -a` plus a repair pass: only regular
//! files are copied and only real directories are traversed, so no copied entry
//! can ever point outside the target tree. Order is the sorted relative path, so
//! a truncated copy is reproducible.

use std::path::Path;

use agent_runtime_protocol::{CopiedArtifactFile, CopiedArtifacts};
use anyhow::{Context, Result};

const MAX_ARTIFACT_FILES: usize = 200;
const MAX_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
/// The walk is recursive, so an agent-authored `mkdir -p a/a/a/...` chain would
/// otherwise overflow the runtime's stack — a hard `abort()`, not a catchable
/// panic. Both bounds match the read vocabulary (`MAX_ARTIFACT_PATH_SEGMENTS`
/// and `MAX_ARTIFACT_PATH_LEN` in the daemon), so nothing is ever copied that a
/// later `read_handoff_file` would refuse to name.
const MAX_ARTIFACT_DEPTH: usize = 16;
const MAX_ARTIFACT_PATH_LEN: usize = 512;

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
    match std::fs::symlink_metadata(source) {
        Ok(metadata) if metadata.is_dir() => {}
        // A symlinked staging dir is never followed; treat it as absent.
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", source.display()));
        }
    }
    let mut copied = CopiedArtifacts::default();
    let mut bytes = 0u64;
    copy_dir(source, target, Path::new(""), 0, &mut copied, &mut bytes)?;
    Ok(Some(copied))
}

/// `depth` is the number of path segments already walked, so an entry of this
/// directory sits at `depth + 1`.
fn copy_dir(
    source: &Path,
    target: &Path,
    relative: &Path,
    depth: usize,
    copied: &mut CopiedArtifacts,
    bytes: &mut u64,
) -> Result<()> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("read artifact dir {}", source.display()))?
    {
        let entry = entry?;
        entries.push((entry.file_name(), entry.file_type()?));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (name, file_type) in entries {
        if copied.truncated {
            return Ok(());
        }
        // `skipped` is written into the parent's durable workspace just like
        // `files`, so the two share one bound.
        if copied.files.len() + copied.skipped.len() >= MAX_ARTIFACT_FILES {
            copied.truncated = true;
            return Ok(());
        }
        let child_source = source.join(&name);
        let child_target = target.join(&name);
        let child_relative = relative.join(&name);
        let display = child_relative.to_string_lossy().to_string();
        // `file_type` comes from `read_dir` and never follows symlinks, so a
        // symlinked directory is recorded as skipped rather than traversed.
        // Entries past the depth/length bounds are skipped for the same reason:
        // they are recorded, never followed.
        if file_type.is_symlink()
            || !(file_type.is_dir() || file_type.is_file())
            || depth + 1 > MAX_ARTIFACT_DEPTH
            || display.len() > MAX_ARTIFACT_PATH_LEN
        {
            copied.skipped.push(display);
        } else if file_type.is_dir() {
            copy_dir(
                &child_source,
                &child_target,
                &child_relative,
                depth + 1,
                copied,
                bytes,
            )?;
        } else {
            let size = std::fs::symlink_metadata(&child_source)
                .with_context(|| format!("inspect artifact {}", child_source.display()))?
                .len();
            if copied.files.len() >= MAX_ARTIFACT_FILES || *bytes + size > MAX_ARTIFACT_BYTES {
                copied.truncated = true;
                return Ok(());
            }
            // Target directories are created lazily, so an empty staging tree
            // leaves no empty `artifacts/` dir behind in the parent.
            std::fs::create_dir_all(target)
                .with_context(|| format!("create artifact dir {}", target.display()))?;
            std::fs::copy(&child_source, &child_target).with_context(|| {
                format!(
                    "copy artifact {} to {}",
                    child_source.display(),
                    child_target.display()
                )
            })?;
            *bytes += size;
            copied.files.push(CopiedArtifactFile {
                path: display,
                bytes: size,
            });
        }
    }
    Ok(())
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
