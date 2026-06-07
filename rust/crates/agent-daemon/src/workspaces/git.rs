use std::path::Path;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use super::config::{required_git_field, WorkspaceBaseConfig};

pub(super) async fn refresh_git_workspace_base(
    base: &Path,
    config: &WorkspaceBaseConfig,
) -> Result<()> {
    let remote_url = required_git_field(config.remote_url.as_deref(), "remote_url")?;
    let branch = required_git_field(config.remote_branch.as_deref(), "remote_branch")?;
    let branch_refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");

    if !base.join(".git").is_dir() {
        run_git(base, ["init"]).await?;
    }

    if git_remote_exists(base, "origin").await? {
        run_git(base, ["remote", "set-url", "origin", remote_url]).await?;
    } else {
        run_git(base, ["remote", "add", "origin", remote_url]).await?;
    }
    run_git(base, ["fetch", "--prune", "origin", &branch_refspec]).await?;
    let origin_ref = format!("refs/remotes/origin/{branch}");
    let base_sha = git_output(base, ["rev-parse", &origin_ref]).await?;
    run_git(base, ["checkout", "--detach", &base_sha]).await?;
    run_git(base, ["reset", "--hard", &base_sha]).await?;
    run_git(base, ["clean", "-ffdx"]).await?;
    Ok(())
}

pub(crate) async fn validate_remote_branch(remote_url: &str, remote_branch: &str) -> Result<()> {
    let remote_url = remote_url.trim();
    let remote_branch = remote_branch.trim();
    if remote_url.is_empty() {
        bail!("workspace remote_url is required");
    }
    if remote_branch.is_empty() {
        bail!("workspace remote_branch is required");
    }
    let branch_check = git_command()
        .arg("check-ref-format")
        .arg("--branch")
        .arg(remote_branch)
        .output()
        .await
        .context("validate remote branch name")?;
    if !branch_check.status.success() {
        bail!("workspace remote_branch is not a valid git branch name: {remote_branch}");
    }
    let output = git_command()
        .arg("ls-remote")
        .arg("--heads")
        .arg(remote_url)
        .arg(remote_branch)
        .output()
        .await
        .with_context(|| format!("check remote branch {remote_url} {remote_branch}"))?;
    if !output.status.success() {
        bail!(
            "git ls-remote failed for {remote_url} {remote_branch}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if String::from_utf8_lossy(&output.stdout).trim().is_empty() {
        bail!("remote branch not found: {remote_url} {remote_branch}");
    }
    Ok(())
}

pub(super) async fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<()> {
    let output = git_command()
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("run git in {}", cwd.display()))?;
    if !output.status.success() {
        bail!(
            "git failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub(super) async fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
    let output = git_command()
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("run git in {}", cwd.display()))?;
    if !output.status.success() {
        bail!(
            "git failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn git_remote_exists(cwd: &Path, name: &str) -> Result<bool> {
    let output = git_command()
        .args(["remote", "get-url", name])
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("check git remote {name} in {}", cwd.display()))?;
    Ok(output.status.success())
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}
