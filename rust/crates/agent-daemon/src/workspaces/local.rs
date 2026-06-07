use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use super::config::{required_local_field, WorkspaceBaseConfig};
use super::sanitize::sanitize_copied_tree;

pub(super) async fn refresh_local_workspace_base(
    base: &Path,
    config: &WorkspaceBaseConfig,
) -> Result<()> {
    let source_path = required_local_field(config.source_path.as_deref(), "source_path")?;
    let source = PathBuf::from(source_path);
    if !source.is_dir() {
        bail!(
            "local workspace source_path is not a directory: {}",
            source.display()
        );
    }
    rsync_dir_delete(&source, base).await?;
    sanitize_copied_tree(base).await
}

async fn rsync_dir_delete(source: &Path, target: &Path) -> Result<()> {
    let source_contents = source.join(".");
    let output = Command::new("rsync")
        .arg("-a")
        .arg("--delete")
        .arg("--delete-excluded")
        .arg("--numeric-ids")
        .arg("--no-owner")
        .arg("--no-group")
        .arg(source_contents)
        .arg(target)
        .output()
        .await
        .with_context(|| {
            format!(
                "rsync local workspace base {} to {}",
                source.display(),
                target.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "rsync failed from {} to {}: {}",
            source.display(),
            target.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
