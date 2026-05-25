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
        let project_workspaces = vec![ProjectWorkspace {
            workspace_dir: "repo".to_string(),
            remote_url: remote.to_string_lossy().into_owned(),
            remote_branch: "main".to_string(),
        }];

        let (cwd, workspaces) = manager
            .materialize_session("session-1", &project_workspaces)
            .await
            .expect("materialize session");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].workspace_dir, "repo");
        assert_eq!(workspaces[0].remote_branch, "main");
        assert_eq!(workspaces[0].local_branch, "pi/session/session-1/repo");
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
