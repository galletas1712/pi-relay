use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use agent_store::{ProjectWorkspace, SessionWorkspace, WorkspaceKind};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::Mutex;
use uuid::Uuid;

const WORKSPACE_BASE_METADATA: &str = "metadata.json";
const WORKSPACE_BASE_DIR: &str = "base";

#[derive(Clone)]
pub(crate) struct WorkspaceManager {
    state_root: PathBuf,
    workspace_base_lock: Arc<Mutex<()>>,
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
        Ok(Self::new(state_home.join("pi-relay")))
    }

    fn new(state_root: PathBuf) -> Self {
        Self {
            state_root,
            workspace_base_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) async fn materialize_session(
        &self,
        project_id: Uuid,
        session_id: &str,
        project_workspaces: &[ProjectWorkspace],
    ) -> Result<(String, Vec<SessionWorkspace>)> {
        let root = self.session_root(session_id);
        if root.exists() {
            tokio::fs::remove_dir_all(&root).await?;
        }
        let cwd = root.join("cwd");
        tokio::fs::create_dir_all(&cwd).await?;
        let _workspace_base_guard = self.workspace_base_lock.lock().await;
        self.remove_stale_workspace_bases(project_id, project_workspaces)
            .await?;
        let mut workspaces = Vec::with_capacity(project_workspaces.len());
        for workspace in project_workspaces {
            workspaces.push(
                self.materialize_workspace(project_id, session_id, &cwd, workspace)
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
                bail!(
                    "session git workspace is missing .git: {}",
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

    pub(crate) async fn reconcile_project_bases(
        &self,
        project_id: Uuid,
        project_workspaces: &[ProjectWorkspace],
    ) -> Result<()> {
        let _workspace_base_guard = self.workspace_base_lock.lock().await;
        self.remove_stale_workspace_bases(project_id, project_workspaces)
            .await?;
        self.remove_changed_workspace_bases(project_id, project_workspaces)
            .await
    }

    pub(crate) async fn remove_project_bases(&self, project_id: Uuid) -> Result<()> {
        let _workspace_base_guard = self.workspace_base_lock.lock().await;
        let root = self.workspace_bases_root(project_id);
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

    fn workspace_bases_root(&self, project_id: Uuid) -> PathBuf {
        self.state_root
            .join("workspace-bases")
            .join(project_id.to_string())
    }

    fn workspace_base_slot(&self, project_id: Uuid, workspace_dir: &str) -> PathBuf {
        self.workspace_bases_root(project_id).join(workspace_dir)
    }

    async fn remove_stale_workspace_bases(
        &self,
        project_id: Uuid,
        project_workspaces: &[ProjectWorkspace],
    ) -> Result<()> {
        let root = self.workspace_bases_root(project_id);
        if !root.exists() {
            return Ok(());
        }
        let mut expected = BTreeSet::new();
        for workspace in project_workspaces {
            expected.insert(workspace_base_config(workspace)?.workspace_dir);
        }

        let mut entries = tokio::fs::read_dir(&root)
            .await
            .with_context(|| format!("read workspace bases {}", root.display()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .with_context(|| format!("read workspace bases {}", root.display()))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !expected.contains(&name) {
                tokio::fs::remove_dir_all(entry.path())
                    .await
                    .with_context(|| {
                        format!("remove stale workspace base {}", entry.path().display())
                    })?;
            }
        }
        Ok(())
    }

    async fn remove_changed_workspace_bases(
        &self,
        project_id: Uuid,
        project_workspaces: &[ProjectWorkspace],
    ) -> Result<()> {
        for workspace in project_workspaces {
            let config = workspace_base_config(workspace)?;
            let slot = self.workspace_base_slot(project_id, &config.workspace_dir);
            if !slot.exists() {
                continue;
            }
            let metadata_path = slot.join(WORKSPACE_BASE_METADATA);
            if read_workspace_base_config(&metadata_path).await?.as_ref() != Some(&config) {
                tokio::fs::remove_dir_all(&slot)
                    .await
                    .with_context(|| format!("remove changed workspace base {}", slot.display()))?;
            }
        }
        Ok(())
    }

    async fn refresh_workspace_base(
        &self,
        project_id: Uuid,
        workspace: &ProjectWorkspace,
    ) -> Result<WorkspaceBase> {
        let config = workspace_base_config(workspace)?;
        let slot = self.workspace_base_slot(project_id, &config.workspace_dir);
        let metadata_path = slot.join(WORKSPACE_BASE_METADATA);
        let base_path = slot.join(WORKSPACE_BASE_DIR);

        let existing_config = read_workspace_base_config(&metadata_path).await?;
        if slot.exists() && (existing_config.as_ref() != Some(&config) || !base_path.is_dir()) {
            tokio::fs::remove_dir_all(&slot)
                .await
                .with_context(|| format!("remove changed workspace base {}", slot.display()))?;
        }

        tokio::fs::create_dir_all(&slot)
            .await
            .with_context(|| format!("create workspace base slot {}", slot.display()))?;
        if !base_path.exists() {
            create_workspace_dir(&base_path).await?;
        }

        match config.kind {
            WorkspaceKind::Git => refresh_git_workspace_base(&base_path, &config).await?,
            WorkspaceKind::Local => refresh_local_workspace_base(&base_path, &config).await?,
        }
        write_workspace_base_config(&metadata_path, &config).await?;

        Ok(WorkspaceBase {
            path: base_path,
            config,
        })
    }

    async fn materialize_workspace(
        &self,
        project_id: Uuid,
        session_id: &str,
        cwd: &Path,
        workspace: &ProjectWorkspace,
    ) -> Result<SessionWorkspace> {
        match workspace.kind {
            WorkspaceKind::Git => {
                self.materialize_git_workspace(project_id, session_id, cwd, workspace)
                    .await
            }
            WorkspaceKind::Local => {
                self.materialize_local_workspace(project_id, cwd, workspace)
                    .await
            }
        }
    }

    async fn materialize_git_workspace(
        &self,
        project_id: Uuid,
        session_id: &str,
        cwd: &Path,
        workspace: &ProjectWorkspace,
    ) -> Result<SessionWorkspace> {
        let base = self.refresh_workspace_base(project_id, workspace).await?;
        let remote_url = required_git_field(base.config.remote_url.as_deref(), "remote_url")?;
        let branch = required_git_field(base.config.remote_branch.as_deref(), "remote_branch")?;
        let workspace_dir = base.config.workspace_dir.as_str();
        let target = cwd.join(workspace_dir);
        if target.exists() {
            bail!("session workspace already exists: {}", target.display());
        }
        let local_branch = local_branch(session_id, workspace_dir);

        instantiate_workspace_from_base(&base.path, &target).await?;
        let base_sha = git_output(&target, ["rev-parse", "HEAD"]).await?;
        run_git(&target, ["switch", "-C", &local_branch, &base_sha]).await?;

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
        project_id: Uuid,
        cwd: &Path,
        workspace: &ProjectWorkspace,
    ) -> Result<SessionWorkspace> {
        let base = self.refresh_workspace_base(project_id, workspace).await?;
        let source_path = required_local_field(base.config.source_path.as_deref(), "source_path")?;
        let workspace_dir = base.config.workspace_dir.as_str();
        let target = cwd.join(workspace_dir);
        if target.exists() {
            bail!("session workspace already exists: {}", target.display());
        }
        instantiate_workspace_from_base(&base.path, &target).await?;
        Ok(SessionWorkspace::local(workspace_dir, source_path))
    }
}

#[derive(Debug)]
struct WorkspaceBase {
    path: PathBuf,
    config: WorkspaceBaseConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkspaceBaseConfig {
    kind: WorkspaceKind,
    workspace_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_path: Option<String>,
}

fn workspace_base_config(workspace: &ProjectWorkspace) -> Result<WorkspaceBaseConfig> {
    validate_workspace_dir(&workspace.workspace_dir)?;
    let workspace_dir = workspace.workspace_dir.trim().to_string();
    match workspace.kind {
        WorkspaceKind::Git => {
            let remote_url =
                required_git_field(workspace.remote_url.as_deref(), "remote_url")?.to_string();
            let remote_branch =
                required_git_field(workspace.remote_branch.as_deref(), "remote_branch")?
                    .to_string();
            Ok(WorkspaceBaseConfig {
                kind: WorkspaceKind::Git,
                workspace_dir,
                remote_url: Some(remote_url),
                remote_branch: Some(remote_branch),
                source_path: None,
            })
        }
        WorkspaceKind::Local => {
            let source_path =
                required_local_field(workspace.source_path.as_deref(), "source_path")?;
            let source = PathBuf::from(source_path);
            if !source.is_dir() {
                bail!(
                    "local workspace source_path is not a directory: {}",
                    source.display()
                );
            }
            Ok(WorkspaceBaseConfig {
                kind: WorkspaceKind::Local,
                workspace_dir,
                remote_url: None,
                remote_branch: None,
                source_path: Some(source.to_string_lossy().into_owned()),
            })
        }
    }
}

async fn read_workspace_base_config(path: &Path) -> Result<Option<WorkspaceBaseConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read workspace base metadata {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("decode workspace base metadata {}", path.display()))
        .map(Some)
}

async fn write_workspace_base_config(path: &Path, config: &WorkspaceBaseConfig) -> Result<()> {
    let json = serde_json::to_vec_pretty(config).context("encode workspace base metadata")?;
    tokio::fs::write(path, json)
        .await
        .with_context(|| format!("write workspace base metadata {}", path.display()))?;
    Ok(())
}

async fn create_workspace_dir(target: &Path) -> Result<()> {
    if try_btrfs_subvolume_create(target).await? {
        return Ok(());
    }
    tokio::fs::create_dir_all(target)
        .await
        .with_context(|| format!("create workspace directory {}", target.display()))?;
    Ok(())
}

async fn refresh_git_workspace_base(base: &Path, config: &WorkspaceBaseConfig) -> Result<()> {
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

async fn refresh_local_workspace_base(base: &Path, config: &WorkspaceBaseConfig) -> Result<()> {
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

async fn instantiate_workspace_from_base(base: &Path, target: &Path) -> Result<()> {
    if try_btrfs_subvolume_snapshot(base, target).await? {
        return Ok(());
    }
    materialize_local_workspace_dir(base, target).await
}

async fn materialize_local_workspace_dir(source: &Path, target: &Path) -> Result<()> {
    if try_btrfs_subvolume_snapshot(source, target).await? {
        match sanitize_copied_tree(target).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(target).await;
                eprintln!(
                    "failed to sanitize btrfs snapshot {} from {}; falling back to copy: {error:#}",
                    target.display(),
                    source.display()
                );
            }
        }
    }

    if try_btrfs_subvolume_create(target).await? {
        match reflink_dir_all(source, target).await {
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

    copy_dir_all(source, target).await
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

async fn reflink_dir_all(source: &Path, target: &Path) -> Result<()> {
    let source = source.to_path_buf();
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || reflink_dir_all_blocking(&source, &target))
        .await
        .context("reflink local workspace task failed")?
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

fn reflink_dir_all_blocking(source: &Path, target: &Path) -> Result<()> {
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
            reflink_dir_all_blocking(&source_path, &target_path)?;
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
            copy_symlink_target(&source_path, &target_path)?;
        }
    }
    Ok(())
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
            copy_file_permissions(&source_path, &target_path)?;
        } else if file_type.is_symlink() {
            copy_symlink_target(&source_path, &target_path)?;
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

async fn sanitize_copied_tree(target: &Path) -> Result<()> {
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
    use uuid::Uuid;

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
        git(&seed, ["config", "commit.gpgsign", "false"]);
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

        let manager = WorkspaceManager::new(temp.path().join("state"));
        let project_id = Uuid::new_v4();
        let project_workspaces = vec![ProjectWorkspace::git(
            "repo",
            remote.to_string_lossy(),
            "main",
        )];

        let (cwd, workspaces) = manager
            .materialize_session(project_id, "session-1", &project_workspaces)
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

        std::fs::write(seed.join("README.md"), "updated\n").expect("update seed file");
        git(&seed, ["add", "README.md"]);
        git(&seed, ["commit", "-m", "update"]);
        git(&seed, ["push", "origin", "main"]);

        let (cwd, _) = manager
            .materialize_session(project_id, "session-2", &project_workspaces)
            .await
            .expect("materialize second session");
        assert_eq!(
            std::fs::read_to_string(Path::new(&cwd).join("repo/README.md"))
                .expect("updated workspace file"),
            "updated\n"
        );
    }

    #[tokio::test]
    async fn materialize_session_workspaces_from_local_folder() {
        let temp = TempDir::new("workspace-manager-local");
        let source = temp.path().join("source");
        std::fs::create_dir_all(source.join("nested")).expect("source dirs");
        std::fs::write(source.join("README.md"), "hello\n").expect("source file");
        std::fs::write(source.join("nested/data.txt"), "nested\n").expect("nested source file");
        make_symlink(Path::new("README.md"), &source.join("readme-link"));
        make_symlink(Path::new("/etc/passwd"), &source.join("external-link"));

        let manager = WorkspaceManager::new(temp.path().join("state"));
        let project_id = Uuid::new_v4();
        let project_workspaces = vec![ProjectWorkspace::local(
            "local-repo",
            source.to_string_lossy(),
        )];

        let (cwd, workspaces) = manager
            .materialize_session(project_id, "session-local", &project_workspaces)
            .await
            .expect("materialize local session");
        let target = Path::new(&cwd).join("local-repo");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].workspace_dir, "local-repo");
        assert_eq!(workspaces[0].kind, WorkspaceKind::Local);
        assert_eq!(
            std::fs::read_to_string(target.join("README.md")).expect("target file"),
            "hello\n"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("nested/data.txt")).expect("nested target file"),
            "nested\n"
        );
        assert_eq!(
            std::fs::read_link(target.join("readme-link")).expect("safe symlink"),
            PathBuf::from("README.md")
        );
        assert!(std::fs::read_to_string(target.join("external-link"))
            .expect("external symlink marker")
            .contains("skipped external symlink target: /etc/passwd"));

        std::fs::write(source.join("README.md"), "updated\n").expect("update source file");
        std::fs::remove_file(source.join("nested/data.txt")).expect("remove deleted source file");
        std::fs::write(source.join("new.txt"), "new\n").expect("new source file");

        let (cwd, _) = manager
            .materialize_session(project_id, "session-local-2", &project_workspaces)
            .await
            .expect("materialize refreshed local session");
        let refreshed = Path::new(&cwd).join("local-repo");
        assert_eq!(
            std::fs::read_to_string(refreshed.join("README.md")).expect("updated target file"),
            "updated\n"
        );
        assert!(
            !refreshed.join("nested/data.txt").exists(),
            "destructive base refresh should delete files removed from the source"
        );
        assert_eq!(
            std::fs::read_to_string(refreshed.join("new.txt")).expect("new target file"),
            "new\n"
        );
    }

    #[tokio::test]
    async fn materialize_session_snapshots_managed_btrfs_base_when_available() {
        let Some(temp) = TempDir::new_under_home("workspace-manager-btrfs") else {
            return;
        };
        let source = temp.path().join("source");
        std::fs::create_dir_all(&source).expect("source dir");
        std::fs::write(source.join("large.bin"), vec![b'x'; 1024 * 1024]).expect("source file");
        make_symlink(Path::new("/etc/passwd"), &source.join("external-link"));

        let manager = WorkspaceManager::new(temp.path().join("state"));
        let project_id = Uuid::new_v4();
        let project_workspaces = vec![ProjectWorkspace::local(
            "local-repo",
            source.to_string_lossy(),
        )];

        let (cwd, _) = manager
            .materialize_session(project_id, "session-btrfs", &project_workspaces)
            .await
            .expect("materialize btrfs session");
        let target = Path::new(&cwd).join("local-repo");
        let base = manager
            .workspace_base_slot(project_id, "local-repo")
            .join(WORKSPACE_BASE_DIR);

        if !is_btrfs_subvolume(&base) || !is_btrfs_subvolume(&target) {
            return;
        }
        assert!(
            btrfs_files_have_shared_extents(&base.join("large.bin"), &target.join("large.bin")),
            "expected the session workspace to be a Btrfs snapshot of the managed base"
        );
        assert!(std::fs::read_to_string(target.join("external-link"))
            .expect("external symlink marker")
            .contains("skipped external symlink target: /etc/passwd"));
    }

    #[tokio::test]
    async fn workspace_base_config_changes_recreate_base() {
        let temp = TempDir::new("workspace-manager-base-recreate");
        let manager = WorkspaceManager::new(temp.path().join("state"));
        let project_id = Uuid::new_v4();
        let source_a = temp.path().join("source-a");
        let source_b = temp.path().join("source-b");
        std::fs::create_dir_all(&source_a).expect("source a");
        std::fs::create_dir_all(&source_b).expect("source b");
        std::fs::write(source_a.join("only-a.txt"), "a\n").expect("source a file");
        std::fs::write(source_b.join("only-b.txt"), "b\n").expect("source b file");

        let workspace_a = vec![ProjectWorkspace::local(
            "local-repo",
            source_a.to_string_lossy(),
        )];
        manager
            .materialize_session(project_id, "session-a", &workspace_a)
            .await
            .expect("materialize source a");

        let workspace_b = vec![ProjectWorkspace::local(
            "local-repo",
            source_b.to_string_lossy(),
        )];
        let (cwd, _) = manager
            .materialize_session(project_id, "session-b", &workspace_b)
            .await
            .expect("materialize source b");
        let target = Path::new(&cwd).join("local-repo");
        assert!(!target.join("only-a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(target.join("only-b.txt")).expect("source b file"),
            "b\n"
        );
    }

    #[tokio::test]
    async fn workspace_name_change_removes_old_base() {
        let temp = TempDir::new("workspace-manager-base-rename");
        let manager = WorkspaceManager::new(temp.path().join("state"));
        let project_id = Uuid::new_v4();
        let source = temp.path().join("source");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::write(source.join("README.md"), "hello\n").expect("source file");

        let old_workspace = vec![ProjectWorkspace::local(
            "old-name",
            source.to_string_lossy(),
        )];
        manager
            .materialize_session(project_id, "session-old", &old_workspace)
            .await
            .expect("materialize old workspace");
        assert!(manager.workspace_base_slot(project_id, "old-name").exists());

        let new_workspace = vec![ProjectWorkspace::local(
            "new-name",
            source.to_string_lossy(),
        )];
        manager
            .materialize_session(project_id, "session-new", &new_workspace)
            .await
            .expect("materialize renamed workspace");
        assert!(!manager.workspace_base_slot(project_id, "old-name").exists());
        assert!(manager.workspace_base_slot(project_id, "new-name").exists());
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

    fn make_symlink(target: &Path, link: &Path) {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).expect("create symlink");
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).expect("create symlink");
        }
    }

    fn is_btrfs_subvolume(path: &Path) -> bool {
        std::process::Command::new("btrfs")
            .args(["subvolume", "show"])
            .arg(path)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn btrfs_files_have_shared_extents(left: &Path, right: &Path) -> bool {
        let output = std::process::Command::new("filefrag")
            .arg("-v")
            .arg(left)
            .arg(right)
            .output()
            .expect("run filefrag");
        assert!(
            output.status.success(),
            "filefrag failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).contains("shared")
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
            Self::new_in(std::env::temp_dir(), prefix)
        }

        fn new_under_home(prefix: &str) -> Option<Self> {
            let home = std::env::var_os("HOME").filter(|value| !value.is_empty())?;
            let base = PathBuf::from(home).join(".local/state/pi-relay/test-tmp");
            std::fs::create_dir_all(&base).ok()?;
            let temp = Self::new_in(base, prefix);
            if can_create_btrfs_subvolume(temp.path()) {
                Some(temp)
            } else {
                None
            }
        }

        fn new_in(base: PathBuf, prefix: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos();
            let path = base.join(format!("pi-relay-{prefix}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    fn can_create_btrfs_subvolume(parent: &Path) -> bool {
        let probe = parent.join("probe-subvol");
        let created = std::process::Command::new("btrfs")
            .args(["subvolume", "create"])
            .arg(&probe)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if created {
            let _ = std::fs::remove_dir_all(&probe);
        }
        created
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
