mod config;
mod git;
mod instantiate;
mod local;
mod sanitize;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use agent_store::{ProjectWorkspace, SessionWorkspace, WorkspaceKind};
use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::Mutex;
use uuid::Uuid;

pub(crate) use self::config::validate_workspace_dir;
use self::config::{
    branch_component, path_component, read_workspace_base_config, required_git_field,
    required_local_field, workspace_base_config, write_workspace_base_config, WorkspaceBaseConfig,
    WORKSPACE_BASE_DIR, WORKSPACE_BASE_METADATA,
};
pub(crate) use self::git::validate_remote_branch;
use self::git::{git_output, refresh_git_workspace_base, run_git};
use self::instantiate::{create_workspace_dir, instantiate_workspace_from_base};
use self::local::refresh_local_workspace_base;

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

fn local_branch(session_id: &str, workspace_dir: &str) -> String {
    format!(
        "pi/session/{}/{}",
        branch_component(session_id),
        branch_component(workspace_dir)
    )
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
