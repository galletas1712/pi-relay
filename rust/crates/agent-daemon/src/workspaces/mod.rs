mod config;
mod git;
mod instantiate;
mod local;
mod sanitize;
mod selection;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use agent_store::{ProjectWorkspace, SessionConfig, SessionWorkspace, WorkspaceKind};
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
use self::git::{
    fetch_local_commit_ref, fetch_session_branch_head, git_output, refresh_git_workspace_base,
    run_git, snapshot_worktree_commit,
};
use self::instantiate::{
    create_workspace_dir, destroy_workspace_tree, instantiate_workspace_from_base,
    materialize_tree_from_source_exact,
};
use self::local::refresh_local_workspace_base;
use self::selection::SelectedWorkspace;
pub(crate) use self::selection::{RequestedWorkspace, WorkspaceSelection};

/// Sibling of the workspace dirs under the cwd root. Owned by the daemon for
/// stage handoff files; it is never a workspace, never snapshotted into an RO
/// fork.
const HANDOFF_DIR: &str = ".pi-handoff";

#[derive(Clone)]
pub(crate) struct WorkspaceManager {
    state_root: PathBuf,
    workspace_base_lock: Arc<Mutex<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceRefSpec {
    pub(crate) source_id: String,
    pub(crate) session_id: String,
    pub(crate) workspace_dir: String,
    pub(crate) git_ref: String,
    pub(crate) commit: String,
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

    /// Materialize a new session's workspaces under a private `outer_cwd`.
    ///
    /// `project_workspaces` is the project's full declared set and is used to
    /// reconcile the managed per-project workspace bases (so a session scoped to a
    /// subset never destroys the bases of workspaces it skipped).
    /// `selected_workspaces` is the subset to actually instantiate into the session,
    /// each paired with an optional git branch override; the workspaces must be a
    /// subset of `project_workspaces` (callers resolve this via
    /// [`WorkspaceSelection::resolve`]).
    pub(crate) async fn materialize_session(
        &self,
        project_id: Uuid,
        session_id: &str,
        project_workspaces: &[ProjectWorkspace],
        selected_workspaces: &[SelectedWorkspace],
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
        let mut workspaces = Vec::with_capacity(selected_workspaces.len());
        for selected in selected_workspaces {
            workspaces.push(
                self.materialize_workspace(
                    project_id,
                    session_id,
                    &cwd,
                    &selected.workspace,
                    selected.branch_override.as_deref(),
                )
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
        let own_cwd = self.session_root(session_id).join("cwd");
        let cwd = PathBuf::from(outer_cwd);
        // A `full` subagent runs against its parent's workspace dirs in place, so
        // its configured outer_cwd lives under the parent's session root, not its
        // own. Those dirs already exist; only materialize the tree for a session
        // that owns its cwd. The workspaces below are validated either way.
        if cwd == own_cwd {
            tokio::fs::create_dir_all(&cwd).await?;
        }
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

    pub(crate) async fn fork_session_from_parent(
        &self,
        parent_session_id: &str,
        parent_outer_cwd: &str,
        parent_workspaces: &[SessionWorkspace],
        child_session_id: &str,
    ) -> Result<(String, Vec<SessionWorkspace>)> {
        if parent_session_id == child_session_id {
            bail!("child session id must differ from parent session id");
        }
        self.ensure_session(parent_session_id, parent_outer_cwd, parent_workspaces)
            .await?;

        let child_root = self.session_root(child_session_id);
        let parent_cwd = PathBuf::from(parent_outer_cwd);
        if child_root.starts_with(&parent_cwd) {
            bail!(
                "child session root {} must not be inside parent cwd {}",
                child_root.display(),
                parent_cwd.display()
            );
        }
        if child_root.exists() {
            tokio::fs::remove_dir_all(&child_root)
                .await
                .with_context(|| {
                    format!(
                        "remove existing child session root {}",
                        child_root.display()
                    )
                })?;
        }
        tokio::fs::create_dir_all(&child_root)
            .await
            .with_context(|| format!("create child session root {}", child_root.display()))?;

        let child_cwd = child_root.join("cwd");
        materialize_tree_from_source_exact(&parent_cwd, &child_cwd)
            .await
            .with_context(|| {
                format!(
                    "fork parent session cwd {} to child cwd {}",
                    parent_cwd.display(),
                    child_cwd.display()
                )
            })?;

        // The durable handoff directory lives under the parent cwd and must
        // never be carried into a disposable RO fork (the durable copy stays
        // under the parent). The fork copies the whole cwd, so drop it here.
        let child_handoff = child_cwd.join(HANDOFF_DIR);
        if child_handoff.exists() {
            destroy_workspace_tree(&child_handoff).await.with_context(|| {
                format!("exclude handoff dir from fork {}", child_handoff.display())
            })?;
        }

        let mut child_workspaces = Vec::with_capacity(parent_workspaces.len());
        for workspace in parent_workspaces {
            validate_workspace_dir(&workspace.workspace_dir)?;
            let child_workspace_root = child_cwd.join(&workspace.workspace_dir);
            let mut child_workspace = workspace.clone();
            if workspace.kind == WorkspaceKind::Git {
                validate_git_workspace_isolated(&child_workspace_root).await?;
                let local_branch = local_branch(child_session_id, &workspace.workspace_dir);
                let head = git_output(&child_workspace_root, ["rev-parse", "HEAD"]).await?;
                run_git(
                    &child_workspace_root,
                    ["switch", "-C", &local_branch, &head],
                )
                .await?;
                child_workspace.local_branch = Some(local_branch);
            }
            child_workspaces.push(child_workspace);
        }

        Ok((child_cwd.to_string_lossy().into_owned(), child_workspaces))
    }

    pub(crate) async fn import_source_refs(
        &self,
        target_outer_cwd: &str,
        target_workspaces: &[SessionWorkspace],
        sources: &[(String, SessionConfig)],
    ) -> Result<Vec<SourceRefSpec>> {
        let mut refs = Vec::new();
        let target_cwd = PathBuf::from(target_outer_cwd);
        for (source_index, (source_session_id, source_config)) in sources.iter().enumerate() {
            let source_id = source_ref_id(source_index, source_session_id, source_config);
            for target_workspace in target_workspaces {
                if target_workspace.kind != WorkspaceKind::Git {
                    continue;
                }
                let workspace_dir = &target_workspace.workspace_dir;
                let Some(source_workspace) = source_config.workspaces.iter().find(|workspace| {
                    workspace.kind == WorkspaceKind::Git
                        && workspace.workspace_dir == *workspace_dir
                }) else {
                    continue;
                };
                let source_repo =
                    PathBuf::from(&source_config.outer_cwd).join(&source_workspace.workspace_dir);
                let target_repo = target_cwd.join(&target_workspace.workspace_dir);
                let message =
                    format!("pi-relay source {source_id} from child session {source_session_id}");
                let commit = snapshot_worktree_commit(&source_repo, &message).await?;
                let git_ref = format!("refs/pi-relay/sources/{source_id}");
                fetch_local_commit_ref(&target_repo, &source_repo, &commit, &git_ref).await?;
                refs.push(SourceRefSpec {
                    source_id: source_id.clone(),
                    session_id: source_session_id.clone(),
                    workspace_dir: workspace_dir.clone(),
                    git_ref,
                    commit,
                });
            }
        }
        Ok(refs)
    }

    /// Reclaim a session's entire workspace tree, including any btrfs
    /// subvolumes created while instantiating or forking it. This is the single
    /// teardown primitive; callers that only want directory removal still route
    /// through it so subvolume metadata is never leaked.
    pub(crate) async fn destroy_session_workspaces(&self, session_id: &str) -> Result<()> {
        let root = self.session_root(session_id);
        if root.exists() {
            destroy_workspace_tree(&root).await?;
        }
        Ok(())
    }

    pub(crate) async fn remove_session_dir(&self, session_id: &str) -> Result<()> {
        self.destroy_session_workspaces(session_id).await
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
        branch_override: Option<&str>,
    ) -> Result<SessionWorkspace> {
        match workspace.kind {
            WorkspaceKind::Git => {
                self.materialize_git_workspace(
                    project_id,
                    session_id,
                    cwd,
                    workspace,
                    branch_override,
                )
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
        branch_override: Option<&str>,
    ) -> Result<SessionWorkspace> {
        let base = self.refresh_workspace_base(project_id, workspace).await?;
        let remote_url = required_git_field(base.config.remote_url.as_deref(), "remote_url")?;
        let default_branch =
            required_git_field(base.config.remote_branch.as_deref(), "remote_branch")?;
        let workspace_dir = base.config.workspace_dir.as_str();
        let target = cwd.join(workspace_dir);
        if target.exists() {
            bail!("session workspace already exists: {}", target.display());
        }
        let local_branch = local_branch(session_id, workspace_dir);

        instantiate_workspace_from_base(&base.path, &target).await?;
        // The session copy inherits the base's branch by default; an override fetches
        // the requested branch into this session's copy only, leaving the shared base
        // on the project's configured branch.
        let (session_branch, base_sha) = match branch_override {
            Some(branch) if branch != default_branch => {
                let sha = fetch_session_branch_head(&target, branch).await?;
                (branch, sha)
            }
            _ => (
                default_branch,
                git_output(&target, ["rev-parse", "HEAD"]).await?,
            ),
        };
        run_git(&target, ["switch", "-C", &local_branch, &base_sha]).await?;

        Ok(SessionWorkspace::git(
            workspace_dir,
            remote_url,
            session_branch,
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

fn source_ref_id(
    source_index: usize,
    source_session_id: &str,
    source_config: &SessionConfig,
) -> String {
    let role = source_config
        .metadata
        .get("role_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("child");
    let suffix = source_session_id
        .rsplit_once('-')
        .map(|(_, suffix)| suffix)
        .unwrap_or(source_session_id);
    format!(
        "source-{}-{}-{}",
        source_index + 1,
        branch_component(role),
        branch_component(suffix)
    )
}

async fn validate_git_workspace_isolated(workspace_root: &Path) -> Result<()> {
    if !workspace_root.is_dir() {
        bail!(
            "child git workspace is missing: {}",
            workspace_root.display()
        );
    }
    let git_dir = git_output(workspace_root, ["rev-parse", "--git-dir"]).await?;
    let common_dir = git_output(workspace_root, ["rev-parse", "--git-common-dir"]).await?;
    let workspace_root = tokio::fs::canonicalize(workspace_root)
        .await
        .with_context(|| format!("canonicalize child workspace {}", workspace_root.display()))?;
    let git_dir = canonicalize_git_path(&workspace_root, &git_dir).await?;
    let common_dir = canonicalize_git_path(&workspace_root, &common_dir).await?;
    if !git_dir.starts_with(&workspace_root) {
        bail!(
            "child git dir {} escapes workspace {}",
            git_dir.display(),
            workspace_root.display()
        );
    }
    if !common_dir.starts_with(&workspace_root) {
        bail!(
            "child git common dir {} escapes workspace {}",
            common_dir.display(),
            workspace_root.display()
        );
    }
    Ok(())
}

async fn canonicalize_git_path(workspace_root: &Path, git_path: &str) -> Result<PathBuf> {
    let path = Path::new(git_path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    };
    tokio::fs::canonicalize(&path)
        .await
        .with_context(|| format!("canonicalize git path {}", path.display()))
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
