use std::path::Path;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

pub(super) async fn create_session_subvolume(target: &Path) -> Result<()> {
    run_btrfs(["subvolume", "create"], target, None).await
}

pub(super) async fn snapshot_session(source: &Path, target: &Path) -> Result<()> {
    run_btrfs(["subvolume", "snapshot"], source, Some(target)).await
}

pub(super) async fn populate_workspace(source: &Path, target: &Path) -> Result<()> {
    tokio::fs::create_dir(target)
        .await
        .with_context(|| format!("create workspace {}", target.display()))?;
    let output = Command::new("cp")
        .args(["-a", "--reflink=always"])
        .arg(source.join("."))
        .arg(target)
        .output()
        .await
        .with_context(|| {
            format!(
                "populate btrfs workspace {} from {}",
                target.display(),
                source.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "required btrfs reflink failed from {} to {}: {}",
            source.display(),
            target.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    super::sanitize::sanitize_copied_tree(target).await
}

pub(super) async fn destroy_session_subvolume(target: &Path) -> Result<()> {
    if !tokio::fs::try_exists(target).await? {
        return Ok(());
    }
    run_btrfs(["subvolume", "delete"], target, None).await
}

async fn run_btrfs<const N: usize>(
    args: [&str; N],
    source: &Path,
    target: Option<&Path>,
) -> Result<()> {
    let mut command = Command::new("btrfs");
    command.args(args).arg(source);
    if let Some(target) = target {
        command.arg(target);
    }
    let output = command
        .output()
        .await
        .with_context(|| format!("run required btrfs operation for {}", source.display()))?;
    if !output.status.success() {
        bail!(
            "btrfs operation failed for {}: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
