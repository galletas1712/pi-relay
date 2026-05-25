use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use agent_store::{ProjectWorkspace, SessionWorkspace};
use anyhow::{anyhow, bail, Context, Result};
use tokio::process::Command;

#[derive(Clone)]
pub(crate) struct WorkspaceManager {
    state_root: PathBuf,
}

impl WorkspaceManager {
    pub(crate) fn from_default_state_dir() -> Result<Self> {
        let state_home = match std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty())
        {
            Some(value) => PathBuf::from(value),
            None => {
                let home = std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow!("HOME is required when XDG_STATE_HOME is unset"))?;
                PathBuf::from(home).join(".local/state")
            }
        };
        Ok(Self {
            state_root: state_home.join("pi-relay"),
        })
    }

    pub(crate) fn session_cwd(&self, session_id: &str) -> String {
        self.session_root(session_id)
            .join("cwd")
            .to_string_lossy()
            .into_owned()
    }

    pub(crate) async fn materialize_session(
        &self,
        session_id: &str,
        project_workspaces: &[ProjectWorkspace],
    ) -> Result<(String, Vec<SessionWorkspace>)> {
        let root = self.session_root(session_id);
        if root.exists() {
            tokio::fs::remove_dir_all(&root).await?;
        }
        let cwd = root.join("cwd");
        tokio::fs::create_dir_all(&cwd).await?;
        let mut workspaces = Vec::with_capacity(project_workspaces.len());
        for workspace in project_workspaces {
            workspaces.push(
                self.materialize_workspace(session_id, &cwd, workspace)
                    .await?,
            );
        }
        Ok((cwd.to_string_lossy().into_owned(), workspaces))
    }

    pub(crate) async fn ensure_session(
        &self,
        session_id: &str,
        outer_cwd: &str,
        workspaces: &[SessionWorkspace],
    ) -> Result<()> {
        if workspaces.is_empty() {
            return Ok(());
        }
        let cwd = self.session_root(session_id).join("cwd");
        let configured = PathBuf::from(outer_cwd);
        if configured != cwd {
            bail!(
                "session outer_cwd does not match workspace state root: expected {}, got {}",
                cwd.display(),
                configured.display()
            );
        }
        tokio::fs::create_dir_all(&cwd).await?;
        for workspace in workspaces {
            validate_workspace_dir(&workspace.workspace_dir)?;
            let target = cwd.join(&workspace.workspace_dir);
            if !target.join(".git").exists() {
                bail!(
                    "session workspace is missing or is not a git checkout: {}",
                    target.display()
                );
            }
        }
        Ok(())
    }

    pub(crate) async fn fork_session(
        &self,
        source_session_id: &str,
        new_session_id: &str,
        workspaces: &[SessionWorkspace],
    ) -> Result<(String, Vec<SessionWorkspace>)> {
        let source_cwd = self.session_root(source_session_id).join("cwd");
        let new_root = self.session_root(new_session_id);
        if new_root.exists() {
            bail!(
                "target session workspace state already exists: {}",
                new_root.display()
            );
        }
        let new_cwd = new_root.join("cwd");
        let forked: Result<Vec<SessionWorkspace>> = async {
            copy_dir(&source_cwd, &new_cwd).await?;
            let mut forked = Vec::with_capacity(workspaces.len());
            for workspace in workspaces {
                let mut workspace = workspace.clone();
                workspace.local_branch = local_branch(new_session_id, &workspace.workspace_dir);
                let workspace_path = new_cwd.join(&workspace.workspace_dir);
                run_git(&workspace_path, ["branch", "-M", &workspace.local_branch]).await?;
                forked.push(workspace);
            }
            Ok(forked)
        }
        .await;
        let forked = match forked {
            Ok(forked) => forked,
            Err(error) => {
                match tokio::fs::remove_dir_all(&new_root).await {
                    Ok(()) => {}
                    Err(cleanup_error) if cleanup_error.kind() == ErrorKind::NotFound => {}
                    Err(cleanup_error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "failed to remove partial fork workspace state at {}: {cleanup_error:#}",
                                new_root.display()
                            )
                        });
                    }
                }
                return Err(error);
            }
        };
        Ok((new_cwd.to_string_lossy().into_owned(), forked))
    }

    pub(crate) async fn remove_session_dir(&self, session_id: &str) -> Result<()> {
        let root = self.session_root(session_id);
        if root.exists() {
            tokio::fs::remove_dir_all(root).await?;
        }
        Ok(())
    }

    fn session_root(&self, session_id: &str) -> PathBuf {
        self.state_root
            .join("sessions")
            .join(path_component(session_id))
    }

    async fn materialize_workspace(
        &self,
        session_id: &str,
        cwd: &Path,
        workspace: &ProjectWorkspace,
    ) -> Result<SessionWorkspace> {
        validate_workspace_dir(&workspace.workspace_dir)?;
        if workspace.remote_url.trim().is_empty() {
            bail!(
                "workspace remote_url is required: {}",
                workspace.workspace_dir
            );
        }
        if workspace.remote_branch.trim().is_empty() {
            bail!(
                "workspace remote_branch is required: {}",
                workspace.workspace_dir
            );
        }
        let workspace_dir = workspace.workspace_dir.trim();
        let target = cwd.join(workspace_dir);
        if target.exists() {
            bail!("session workspace already exists: {}", target.display());
        }
        let branch = workspace.remote_branch.trim();
        let remote_url = workspace.remote_url.trim();
        let local_branch = local_branch(session_id, workspace_dir);
        let branch_refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");

        tokio::fs::create_dir_all(&target).await?;
        run_git(&target, ["init"]).await?;
        run_git(&target, ["remote", "add", "origin", remote_url]).await?;
        run_git(&target, ["fetch", "origin", &branch_refspec]).await?;
        let origin_ref = format!("refs/remotes/origin/{branch}");
        let base_sha = git_output(&target, ["rev-parse", &origin_ref]).await?;
        run_git(&target, ["switch", "-c", &local_branch, &base_sha]).await?;
        let upstream = format!("origin/{branch}");
        run_git(
            &target,
            ["branch", "--set-upstream-to", &upstream, &local_branch],
        )
        .await?;

        Ok(SessionWorkspace {
            workspace_dir: workspace_dir.to_string(),
            remote_url: remote_url.to_string(),
            remote_branch: branch.to_string(),
            base_sha,
            local_branch,
        })
    }
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

pub(crate) fn validate_workspace_dir(workspace_dir: &str) -> Result<()> {
    let workspace_dir = workspace_dir.trim();
    if workspace_dir.is_empty() {
        bail!("workspace_dir is required");
    }
    if workspace_dir.starts_with('.') {
        bail!("workspace_dir must not start with '.': {workspace_dir}");
    }
    let path = Path::new(workspace_dir);
    let mut components = path.components();
    let valid_single_component =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if path.is_absolute() || !valid_single_component {
        bail!("workspace_dir must be a direct child name: {workspace_dir}");
    }
    if !workspace_dir
        .bytes()
        .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
    {
        bail!("workspace_dir may only contain ASCII letters, digits, '_' and '-': {workspace_dir}");
    }
    Ok(())
}

fn local_branch(session_id: &str, workspace_dir: &str) -> String {
    format!(
        "pi/session/{}/{}",
        branch_component(session_id),
        branch_component(workspace_dir)
    )
}

async fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<()> {
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

async fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
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

fn git_command() -> Command {
    let mut command = Command::new("git");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

async fn copy_dir(source: &Path, dest: &Path) -> Result<()> {
    if !source.is_dir() {
        bail!(
            "source session cwd is not a directory: {}",
            source.display()
        );
    }
    let mut pending = vec![(source.to_path_buf(), dest.to_path_buf())];
    let mut copied_dirs = Vec::new();
    while let Some((source_dir, dest_dir)) = pending.pop() {
        tokio::fs::create_dir_all(&dest_dir)
            .await
            .with_context(|| format!("create copy destination {}", dest_dir.display()))?;
        copied_dirs.push((source_dir.clone(), dest_dir.clone()));

        let mut entries = tokio::fs::read_dir(&source_dir)
            .await
            .with_context(|| format!("read directory {}", source_dir.display()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .with_context(|| format!("read entry in {}", source_dir.display()))?
        {
            let source_path = entry.path();
            let dest_path = dest_dir.join(PathBuf::from(entry.file_name()));
            let file_type = entry
                .file_type()
                .await
                .with_context(|| format!("read file type for {}", source_path.display()))?;
            if file_type.is_dir() {
                pending.push((source_path, dest_path));
            } else if file_type.is_symlink() {
                copy_symlink(&source_path, &dest_path).await?;
            } else if file_type.is_file() {
                tokio::fs::copy(&source_path, &dest_path)
                    .await
                    .with_context(|| {
                        format!(
                            "copy file {} to {}",
                            source_path.display(),
                            dest_path.display()
                        )
                    })?;
                let permissions = tokio::fs::metadata(&source_path)
                    .await
                    .with_context(|| format!("read permissions for {}", source_path.display()))?
                    .permissions();
                tokio::fs::set_permissions(&dest_path, permissions)
                    .await
                    .with_context(|| format!("set permissions on {}", dest_path.display()))?;
            } else {
                bail!(
                    "unsupported filesystem entry in session workspace copy: {}",
                    source_path.display()
                );
            }
        }
    }
    for (source_dir, dest_dir) in copied_dirs.into_iter().rev() {
        let permissions = tokio::fs::metadata(&source_dir)
            .await
            .with_context(|| format!("read permissions for {}", source_dir.display()))?
            .permissions();
        tokio::fs::set_permissions(&dest_dir, permissions)
            .await
            .with_context(|| format!("set permissions on {}", dest_dir.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
async fn copy_symlink(source: &Path, dest: &Path) -> Result<()> {
    let target = tokio::fs::read_link(source)
        .await
        .with_context(|| format!("read symlink {}", source.display()))?;
    std::os::unix::fs::symlink(&target, dest)
        .with_context(|| format!("copy symlink {} to {}", source.display(), dest.display()))?;
    Ok(())
}

#[cfg(windows)]
async fn copy_symlink(source: &Path, dest: &Path) -> Result<()> {
    let target = tokio::fs::read_link(source)
        .await
        .with_context(|| format!("read symlink {}", source.display()))?;
    let metadata = tokio::fs::metadata(source)
        .await
        .with_context(|| format!("read symlink target metadata {}", source.display()))?;
    if metadata.is_dir() {
        std::os::windows::fs::symlink_dir(&target, dest)
    } else {
        std::os::windows::fs::symlink_file(&target, dest)
    }
    .with_context(|| format!("copy symlink {} to {}", source.display(), dest.display()))?;
    Ok(())
}

fn path_component(value: &str) -> String {
    let encoded: String = value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02x}").chars().collect(),
        })
        .collect();
    if encoded.is_empty() {
        "%00".to_string()
    } else {
        encoded
    }
}

fn branch_component(value: &str) -> String {
    let component = value
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' => ch,
            _ => '-',
        })
        .collect::<String>()
        .trim_matches(['-', '/'])
        .to_string();
    if component.is_empty() {
        "id".to_string()
    } else {
        component
    }
}
