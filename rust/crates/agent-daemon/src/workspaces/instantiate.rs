use std::path::Path;

use anyhow::{Context, Result};
use tokio::process::Command;

use super::sanitize::{copy_symlink_target, sanitize_copied_tree};

pub(super) async fn create_workspace_dir(target: &Path) -> Result<()> {
    if try_btrfs_subvolume_create(target).await? {
        return Ok(());
    }
    tokio::fs::create_dir_all(target)
        .await
        .with_context(|| format!("create workspace directory {}", target.display()))?;
    Ok(())
}

pub(super) async fn instantiate_workspace_from_base(base: &Path, target: &Path) -> Result<()> {
    if try_btrfs_subvolume_snapshot(base, target).await? {
        return Ok(());
    }
    materialize_tree_from_source(base, target).await
}

pub(super) async fn materialize_tree_from_source(source: &Path, target: &Path) -> Result<()> {
    materialize_tree_from_source_with_mode(source, target, SymlinkMode::Sanitize).await
}

pub(super) async fn materialize_tree_from_source_exact(source: &Path, target: &Path) -> Result<()> {
    materialize_tree_from_source_with_mode(source, target, SymlinkMode::Preserve).await
}

async fn materialize_tree_from_source_with_mode(
    source: &Path,
    target: &Path,
    symlink_mode: SymlinkMode,
) -> Result<()> {
    if try_snapshot_tree(source, target, symlink_mode).await? {
        return Ok(());
    }
    if try_btrfs_subvolume_create(target).await? {
        match reflink_dir_all(source, target, symlink_mode).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(target).await;
                eprintln!(
                    "failed to populate btrfs workspace subvolume {} from {}; falling back to copy: {error:#}",
                    target.display(),
                    source.display()
                );
            }
        }
    }

    copy_dir_all(source, target, symlink_mode).await
}

async fn try_snapshot_tree(
    source: &Path,
    target: &Path,
    symlink_mode: SymlinkMode,
) -> Result<bool> {
    if !try_btrfs_subvolume_snapshot(source, target).await? {
        return Ok(false);
    }
    if matches!(symlink_mode, SymlinkMode::Preserve) {
        return Ok(true);
    }
    match sanitize_copied_tree(target).await {
        Ok(()) => Ok(true),
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(target).await;
            eprintln!(
                "failed to sanitize btrfs snapshot {} from {}; falling back to copy: {error:#}",
                target.display(),
                source.display()
            );
            Ok(false)
        }
    }
}

async fn try_btrfs_subvolume_snapshot(source: &Path, target: &Path) -> Result<bool> {
    let output = match Command::new("btrfs")
        .arg("subvolume")
        .arg("snapshot")
        .arg(source)
        .arg(target)
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "try btrfs subvolume snapshot {} to {}",
                    source.display(),
                    target.display()
                )
            })
        }
    };
    Ok(output.status.success())
}

async fn try_btrfs_subvolume_create(target: &Path) -> Result<bool> {
    let output = match Command::new("btrfs")
        .arg("subvolume")
        .arg("create")
        .arg(target)
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("try btrfs subvolume create {}", target.display()))
        }
    };
    Ok(output.status.success())
}

/// Reclaim a session workspace tree, including any nested btrfs subvolumes.
///
/// `instantiate.rs` may turn the cwd root and each workspace dir into its own
/// btrfs subvolume. A plain `remove_dir_all` leaks that subvolume metadata, and
/// `btrfs subvolume delete` refuses a subvolume that still contains child
/// subvolumes. So we walk depth-first (children before parents), attempt a
/// subvolume delete at every directory, then `remove_dir_all` the reflink/copy
/// remainder. When btrfs is absent the delete is a no-op (mirroring
/// `try_btrfs_subvolume_create`) and only the `remove_dir_all` runs.
pub(super) async fn destroy_workspace_tree(root: &Path) -> Result<()> {
    delete_btrfs_subvolumes_depth_first(root).await?;
    if root.exists() {
        tokio::fs::remove_dir_all(root)
            .await
            .with_context(|| format!("remove workspace tree {}", root.display()))?;
    }
    Ok(())
}

async fn delete_btrfs_subvolumes_depth_first(root: &Path) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    let mut post_order = Vec::new();
    while let Some(dir) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read workspace tree {}", dir.display()))
            }
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .with_context(|| format!("read workspace tree {}", dir.display()))?
        {
            if entry
                .file_type()
                .await
                .with_context(|| format!("stat {}", entry.path().display()))?
                .is_dir()
            {
                stack.push(entry.path());
            }
        }
        post_order.push(dir);
    }
    // Deepest paths last in, first out: deleting in reverse pushes children
    // before their parents so a parent subvolume is never deleted while it
    // still contains child subvolumes.
    for dir in post_order.into_iter().rev() {
        try_btrfs_subvolume_delete(&dir).await?;
    }
    Ok(())
}

async fn try_btrfs_subvolume_delete(target: &Path) -> Result<bool> {
    let output = match Command::new("btrfs")
        .arg("subvolume")
        .arg("delete")
        .arg(target)
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("try btrfs subvolume delete {}", target.display()))
        }
    };
    Ok(output.status.success())
}

#[derive(Clone, Copy)]
enum SymlinkMode {
    Sanitize,
    Preserve,
}

async fn reflink_dir_all(source: &Path, target: &Path, symlink_mode: SymlinkMode) -> Result<()> {
    let source = source.to_path_buf();
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || reflink_dir_all_blocking(&source, &target, symlink_mode))
        .await
        .context("reflink local workspace task failed")?
}

fn reflink_dir_all_blocking(source: &Path, target: &Path, symlink_mode: SymlinkMode) -> Result<()> {
    std::fs::create_dir_all(target)
        .with_context(|| format!("create reflink workspace copy {}", target.display()))?;
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("read reflink workspace source {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            reflink_dir_all_blocking(&source_path, &target_path, symlink_mode)?;
        } else if file_type.is_file() {
            reflink_copy::reflink(&source_path, &target_path).with_context(|| {
                format!(
                    "reflink local workspace file {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
            copy_file_permissions(&source_path, &target_path)?;
        } else if file_type.is_symlink() {
            copy_symlink(&source_path, &target_path, symlink_mode)?;
        }
    }
    Ok(())
}

async fn copy_dir_all(source: &Path, target: &Path, symlink_mode: SymlinkMode) -> Result<()> {
    let source = source.to_path_buf();
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || copy_dir_all_blocking(&source, &target, symlink_mode))
        .await
        .context("copy local workspace task failed")?
}

fn copy_dir_all_blocking(source: &Path, target: &Path, symlink_mode: SymlinkMode) -> Result<()> {
    std::fs::create_dir_all(target)
        .with_context(|| format!("create local workspace copy {}", target.display()))?;
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("read local workspace source {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_all_blocking(&source_path, &target_path, symlink_mode)?;
        } else if file_type.is_file() {
            std::fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "copy local workspace file {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
            copy_file_permissions(&source_path, &target_path)?;
        } else if file_type.is_symlink() {
            copy_symlink(&source_path, &target_path, symlink_mode)?;
        }
    }
    Ok(())
}

fn copy_symlink(source_path: &Path, target_path: &Path, symlink_mode: SymlinkMode) -> Result<()> {
    match symlink_mode {
        SymlinkMode::Sanitize => copy_symlink_target(source_path, target_path),
        SymlinkMode::Preserve => copy_symlink_target_exact(source_path, target_path),
    }
}

fn copy_symlink_target_exact(source_path: &Path, target_path: &Path) -> Result<()> {
    let target = std::fs::read_link(source_path)
        .with_context(|| format!("read symlink {}", source_path.display()))?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, target_path).with_context(|| {
            format!(
                "copy symlink {} to {}",
                source_path.display(),
                target_path.display()
            )
        })?;
    }
    #[cfg(windows)]
    {
        let resolved = if target.is_absolute() {
            target
        } else {
            source_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(target)
        };
        if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(&resolved, target_path).with_context(|| {
                format!(
                    "copy directory symlink {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        } else {
            std::os::windows::fs::symlink_file(&resolved, target_path).with_context(|| {
                format!(
                    "copy file symlink {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn copy_file_permissions(source_path: &Path, target_path: &Path) -> Result<()> {
    let permissions = std::fs::metadata(source_path)
        .with_context(|| format!("read file metadata {}", source_path.display()))?
        .permissions();
    std::fs::set_permissions(target_path, permissions)
        .with_context(|| format!("set file permissions {}", target_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[tokio::test]
    async fn destroy_workspace_tree_removes_nested_dirs() {
        let root = std::env::temp_dir().join(format!("pi-destroy-{}", Uuid::new_v4()));
        let nested = root.join("cwd").join("repo").join("src");
        tokio::fs::create_dir_all(&nested).await.unwrap();
        tokio::fs::write(nested.join("main.rs"), b"fn main() {}")
            .await
            .unwrap();
        tokio::fs::write(root.join("cwd").join("README.md"), b"hi")
            .await
            .unwrap();

        destroy_workspace_tree(&root).await.unwrap();
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn destroy_workspace_tree_is_ok_when_missing() {
        let root = std::env::temp_dir().join(format!("pi-destroy-missing-{}", Uuid::new_v4()));
        destroy_workspace_tree(&root).await.unwrap();
        assert!(!root.exists());
    }
}
