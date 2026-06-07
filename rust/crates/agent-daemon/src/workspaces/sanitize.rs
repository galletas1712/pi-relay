use std::path::{Component, Path};

use anyhow::{Context, Result};

pub(super) fn copy_symlink_target(source_path: &Path, target_path: &Path) -> Result<()> {
    let target = std::fs::read_link(source_path)
        .with_context(|| format!("read symlink {}", source_path.display()))?;
    if !is_safe_relative_symlink(&target) {
        write_skipped_symlink_marker(target_path, &target)?;
        return Ok(());
    }
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

pub(super) async fn sanitize_copied_tree(target: &Path) -> Result<()> {
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || sanitize_copied_tree_blocking(&target))
        .await
        .context("sanitize local workspace task failed")?
}

fn sanitize_copied_tree_blocking(path: &Path) -> Result<()> {
    for entry in
        std::fs::read_dir(path).with_context(|| format!("read copied tree {}", path.display()))?
    {
        let entry = entry?;
        let child = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            sanitize_copied_tree_blocking(&child)?;
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&child)
                .with_context(|| format!("read symlink {}", child.display()))?;
            if !is_safe_relative_symlink(&target) {
                std::fs::remove_file(&child)
                    .with_context(|| format!("remove unsafe symlink {}", child.display()))?;
                write_skipped_symlink_marker(&child, &target)?;
            }
        } else if !file_type.is_file() {
            std::fs::remove_file(&child)
                .with_context(|| format!("remove unsupported copied file {}", child.display()))?;
        }
    }
    Ok(())
}

fn is_safe_relative_symlink(target: &Path) -> bool {
    !target.is_absolute()
        && target
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn write_skipped_symlink_marker(target_path: &Path, target: &Path) -> Result<()> {
    std::fs::write(
        target_path,
        format!(
            "pi-relay local workspace copy skipped external symlink target: {}\n",
            target.display()
        ),
    )
    .with_context(|| format!("write skipped symlink marker {}", target_path.display()))?;
    Ok(())
}
