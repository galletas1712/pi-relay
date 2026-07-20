use std::ffi::OsStr;
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

/// Fetch a per-session branch override into an already-instantiated git workspace
/// and return its commit sha.
///
/// The session workspace inherits `origin` from the managed project base, so this
/// only needs to fetch the override branch (the project base itself stays on the
/// project's configured branch and is shared across sessions). Errors when the
/// branch name is invalid or absent on the remote; these are client-input errors.
pub(super) async fn fetch_session_branch_head(workspace: &Path, branch: &str) -> Result<String> {
    let branch = branch.trim();
    if branch.is_empty() {
        bail!("session branch override is required");
    }
    let branch_check = git_command()
        .args(["check-ref-format", "--branch", branch])
        .output()
        .await
        .context("validate session branch override name")?;
    if !branch_check.status.success() {
        bail!("session branch override is not a valid git branch name: {branch}");
    }
    let branch_refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    let fetch = git_command()
        .args(["fetch", "--prune", "origin", &branch_refspec])
        .current_dir(workspace)
        .output()
        .await
        .with_context(|| format!("fetch session branch {branch} in {}", workspace.display()))?;
    if !fetch.status.success() {
        bail!(
            "session branch override not found on remote: {branch}: {}",
            String::from_utf8_lossy(&fetch.stderr).trim()
        );
    }
    let origin_ref = format!("refs/remotes/origin/{branch}");
    git_output(workspace, ["rev-parse", &origin_ref]).await
}

pub async fn validate_remote_branch(remote_url: &str, remote_branch: &str) -> Result<()> {
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

/// Run `git` in `cwd` (optionally against an alternate index) and return trimmed
/// stdout. The typed wrappers below choose the arg shape and whether stdout is
/// used; this is the single place that spawns git and maps failure to an error.
async fn git(cwd: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<String> {
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

pub(super) async fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<()> {
    git(cwd, args).await.map(|_| ())
}

pub(super) async fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
    git(cwd, args).await
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
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "pi-relay")
        .env("GIT_AUTHOR_EMAIL", "pi-relay@example.invalid")
        .env("GIT_COMMITTER_NAME", "pi-relay")
        .env("GIT_COMMITTER_EMAIL", "pi-relay@example.invalid");
    command
}
