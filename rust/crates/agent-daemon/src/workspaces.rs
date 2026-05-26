use std::path::{Component, Path, PathBuf};

use agent_store::{ProjectWorkspace, SessionWorkspace, WorkspaceKind};
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
            if !target.is_dir() {
                bail!("session workspace is missing: {}", target.display());
            }
            if workspace.kind == WorkspaceKind::Git && !target.join(".git").exists() {
                bail!("session git workspace is missing .git: {}", target.display());
            }
        }
        Ok(())
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
        match workspace.kind {
            WorkspaceKind::Git => {
                self.materialize_git_workspace(session_id, cwd, workspace)
                    .await
            }
            WorkspaceKind::Local => self.materialize_local_workspace(cwd, workspace).await,
        }
    }

    async fn materialize_git_workspace(
        &self,
        session_id: &str,
        cwd: &Path,
        workspace: &ProjectWorkspace,
    ) -> Result<SessionWorkspace> {
        validate_workspace_dir(&workspace.workspace_dir)?;
        let remote_url = required_git_field(workspace.remote_url.as_deref(), "remote_url")?;
        let branch = required_git_field(workspace.remote_branch.as_deref(), "remote_branch")?;
        let workspace_dir = workspace.workspace_dir.trim();
        let target = cwd.join(workspace_dir);
        if target.exists() {
            bail!("session workspace already exists: {}", target.display());
        }
        let local_branch = local_branch(session_id, workspace_dir);
        let branch_refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");

        tokio::fs::create_dir_all(&target).await?;
        run_git(&target, ["init"]).await?;
        run_git(&target, ["remote", "add", "origin", remote_url]).await?;
        run_git(&target, ["fetch", "origin", &branch_refspec]).await?;
        let origin_ref = format!("refs/remotes/origin/{branch}");
        let base_sha = git_output(&target, ["rev-parse", &origin_ref]).await?;
        run_git(&target, ["switch", "-c", &local_branch, &base_sha]).await?;

        Ok(SessionWorkspace::git(
            workspace_dir,
            remote_url,
            branch,
            base_sha,
            local_branch,
        ))
    }

    async fn materialize_local_workspace(
        &self,
        cwd: &Path,
        workspace: &ProjectWorkspace,
    ) -> Result<SessionWorkspace> {
        validate_workspace_dir(&workspace.workspace_dir)?;
        let source_path = required_local_field(workspace.source_path.as_deref(), "source_path")?;
        let source = PathBuf::from(source_path);
        if !source.is_dir() {
            bail!(
                "local workspace source_path is not a directory: {}",
                source.display()
            );
        }
        let workspace_dir = workspace.workspace_dir.trim();
        let target = cwd.join(workspace_dir);
        if target.exists() {
            bail!("session workspace already exists: {}", target.display());
        }
        copy_dir_all(&source, &target).await?;
        Ok(SessionWorkspace::local(
            workspace_dir,
            source.to_string_lossy().into_owned(),
        ))
    }
}

async fn copy_dir_all(source: &Path, target: &Path) -> Result<()> {
    let source = source.to_path_buf();
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || copy_dir_all_blocking(&source, &target))
        .await
        .context("copy local workspace task failed")?
}

fn copy_dir_all_blocking(source: &Path, target: &Path) -> Result<()> {
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
            copy_dir_all_blocking(&source_path, &target_path)?;
        } else if file_type.is_file() {
            std::fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "copy local workspace file {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        } else if file_type.is_symlink() {
            copy_symlink_target(&source_path, &target_path)?;
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

fn copy_symlink_target(source_path: &Path, target_path: &Path) -> Result<()> {
    let target = std::fs::read_link(source_path)
        .with_context(|| format!("read symlink {}", source_path.display()))?;
    if !is_safe_relative_symlink(&target) {
        std::fs::write(
            target_path,
            format!(
                "pi-relay local workspace copy skipped external symlink target: {}\n",
                target.display()
            ),
        )
        .with_context(|| format!("write skipped symlink marker {}", target_path.display()))?;
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

fn required_git_field<'a>(value: Option<&'a str>, field: &str) -> Result<&'a str> {
    let value = value.unwrap_or("").trim();
    if value.is_empty() {
        bail!("git workspace {field} is required");
    }
    Ok(value)
}

fn required_local_field<'a>(value: Option<&'a str>, field: &str) -> Result<&'a str> {
    let value = value.unwrap_or("").trim();
    if value.is_empty() {
        bail!("local workspace {field} is required");
    }
    Ok(value)
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_store::ProjectWorkspace;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn materialize_session_workspaces_from_local_remote() {
        let temp = TempDir::new("workspace-manager");
        let remote = temp.path().join("remote.git");
        let seed = temp.path().join("seed");
        std::fs::create_dir_all(&seed).expect("seed dir");

        git(
            temp.path(),
            ["init", "--bare", remote.to_str().expect("remote path")],
        );
        git(&seed, ["init"]);
        git(&seed, ["config", "user.email", "pi-relay@example.test"]);
        git(&seed, ["config", "user.name", "pi relay"]);
        std::fs::write(seed.join("README.md"), "hello\n").expect("seed file");
        git(&seed, ["add", "README.md"]);
        git(&seed, ["commit", "-m", "initial"]);
        git(&seed, ["branch", "-M", "main"]);
        git(
            &seed,
            [
                "remote",
                "add",
                "origin",
                remote.to_str().expect("remote path"),
            ],
        );
        git(&seed, ["push", "origin", "main"]);

        let manager = WorkspaceManager {
            state_root: temp.path().join("state"),
        };
        let project_workspaces = vec![ProjectWorkspace::git(
            "repo",
            remote.to_string_lossy(),
            "main",
        )];

        let (cwd, workspaces) = manager
            .materialize_session("session-1", &project_workspaces)
            .await
            .expect("materialize session");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].workspace_dir, "repo");
        assert_eq!(workspaces[0].kind, WorkspaceKind::Git);
        assert_eq!(workspaces[0].remote_branch.as_deref(), Some("main"));
        assert_eq!(
            workspaces[0].local_branch.as_deref(),
            Some("pi/session/session-1/repo")
        );
        assert_eq!(
            git_stdout(
                Path::new(&cwd).join("repo").as_path(),
                ["branch", "--show-current"]
            ),
            "pi/session/session-1/repo"
        );

        assert_eq!(
            std::fs::read_to_string(Path::new(&cwd).join("repo/README.md"))
                .expect("workspace file"),
            "hello\n"
        );
    }

    #[test]
    fn workspace_dir_validation_rejects_paths_and_hidden_dirs() {
        assert!(validate_workspace_dir("repo").is_ok());
        assert!(validate_workspace_dir("repo_1").is_ok());
        assert!(validate_workspace_dir(".repo").is_err());
        assert!(validate_workspace_dir("nested/repo").is_err());
        assert!(validate_workspace_dir("../repo").is_err());
        assert!(validate_workspace_dir("repo.name").is_err());
    }

    fn git<const N: usize>(cwd: &Path, args: [&str; N]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout<const N: usize>(cwd: &Path, args: [&str; N]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("pi-relay-{prefix}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
