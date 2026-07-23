mod config;
mod git;
mod instantiate;
mod local;
mod sanitize;
mod selection;

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use agent_runtime_protocol::{
    ProjectWorkspace, RawInstructionFile, RawSkillFile, RuntimeContext, SessionWorkspace,
    SkillKind, SkillOrigin, WorkspaceKind,
};
use anyhow::{bail, Context, Result};
use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;

pub use self::config::validate_workspace_dir;
use self::config::{
    branch_component, path_component, read_workspace_base_config, required_git_field,
    required_local_field, workspace_base_config, write_workspace_base_config, WorkspaceBaseConfig,
    WORKSPACE_BASE_DIR, WORKSPACE_BASE_METADATA,
};
pub use self::git::validate_remote_branch;
use self::git::{fetch_session_branch_head, git_output, refresh_git_workspace_base, run_git};
use self::instantiate::{
    create_session_subvolume, destroy_session_subvolume, populate_workspace, snapshot_session,
};
use self::local::refresh_local_workspace_base;
pub use self::selection::SelectedWorkspace;

// `.pi-handoff` is a sibling of the workspace dirs under the cwd root. It is
// owned by the daemon for delegation artifact files; it is never a workspace,
// never snapshotted into an RO fork.

#[derive(Clone)]
pub struct WorkspaceManager {
    state_root: PathBuf,
    runtime_config_root: PathBuf,
    home_dir: PathBuf,
    workspace_base_lock: Arc<Mutex<()>>,
    cwd_mutation_guards: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>,
}

impl WorkspaceManager {
    pub fn new(state_root: PathBuf, runtime_config_root: PathBuf, home_dir: PathBuf) -> Self {
        Self {
            state_root,
            runtime_config_root,
            home_dir,
            workspace_base_lock: Arc::new(Mutex::new(())),
            cwd_mutation_guards: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn resolve(&self, workspace_id: &str) -> PathBuf {
        self.session_root(workspace_id).join("cwd")
    }

    /// Resolve a cwd-relative path, rejecting anything that is absolute or
    /// escapes the session cwd.
    fn safe_workspace_path(&self, workspace_id: &str, rel_path: &str) -> Result<PathBuf> {
        let cwd = self.resolve(workspace_id);
        let mut path = cwd.clone();
        for component in Path::new(rel_path).components() {
            match component {
                std::path::Component::Normal(part) => path.push(part),
                _ => bail!("workspace file path must be relative and normal: {rel_path}"),
            }
        }
        Ok(path)
    }

    /// Write a control-generated file (e.g. a delegation handoff artifact) into
    /// the session cwd so runtime-side tools can read it.
    pub async fn write_workspace_file(
        &self,
        workspace_id: &str,
        rel_path: &str,
        contents: &str,
    ) -> Result<()> {
        let path = self.safe_workspace_path(workspace_id, rel_path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, contents.as_bytes()).await?;
        Ok(())
    }

    pub async fn read_workspace_file(
        &self,
        workspace_id: &str,
        rel_path: &str,
    ) -> Result<Option<String>> {
        let path = self.safe_workspace_path(workspace_id, rel_path)?;
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => Ok(Some(contents)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Return all runtime-owned instructions and skill packages visible to a
    /// session. Personal project skills (keyed by `project_key`) override
    /// same-named repository skills.
    pub async fn read_runtime_context(
        &self,
        workspace_id: &str,
        workspace_dirs: &[String],
        project_key: Option<&str>,
    ) -> Result<RuntimeContext> {
        let mut instructions = Vec::new();
        collect_instruction(
            &self.runtime_config_root.join("AGENTS.md"),
            None,
            &mut instructions,
        )
        .await?;

        let mut global_skills = std::collections::BTreeMap::new();
        collect_skill_dir(
            &self.runtime_config_root.join("skills"),
            None,
            SkillKind::Skill,
            SkillOrigin::RuntimeWorkflow,
            &mut global_skills,
        )
        .await?;
        collect_skill_dir(
            &self.home_dir.join(".agents/skills"),
            None,
            SkillKind::Skill,
            SkillOrigin::HomeGlobal,
            &mut global_skills,
        )
        .await?;

        let mut roles = std::collections::BTreeMap::new();
        collect_skill_dir(
            &self.runtime_config_root.join("subagent-roles"),
            None,
            SkillKind::SubagentRole,
            SkillOrigin::RuntimeRole,
            &mut roles,
        )
        .await?;

        let mut project_skills = std::collections::BTreeMap::new();
        let cwd = self.resolve(workspace_id);
        for workspace_dir in workspace_dirs {
            let workspace = Some(workspace_dir.as_str());
            collect_instruction(
                &cwd.join(workspace_dir).join("AGENTS.md"),
                workspace,
                &mut instructions,
            )
            .await?;
            collect_skill_dir(
                &cwd.join(workspace_dir).join(".agents/skills"),
                workspace,
                SkillKind::Skill,
                SkillOrigin::WorkspaceProject,
                &mut project_skills,
            )
            .await?;
        }
        if let Some(project_key) = project_key.filter(|key| !key.is_empty()) {
            collect_skill_dir(
                &self
                    .home_dir
                    .join(".agents")
                    .join("projects")
                    .join(project_key)
                    .join("skills"),
                Some(project_key),
                SkillKind::Skill,
                SkillOrigin::HomeProject,
                &mut project_skills,
            )
            .await?;
        }

        let skills = global_skills
            .into_values()
            .chain(roles.into_values())
            .chain(project_skills.into_values())
            .collect();
        Ok(RuntimeContext {
            instructions,
            skills,
        })
    }

    pub async fn validate_root(&self) -> Result<()> {
        tokio::fs::create_dir_all(self.state_root.join("sessions")).await?;
        let probe = self
            .state_root
            .join(format!(".btrfs-probe-{}", Uuid::new_v4()));
        create_session_subvolume(&probe).await?;
        destroy_session_subvolume(&probe).await
    }

    /// Serialize daemon-managed workspace-mutating tool futures and snapshots
    /// for sessions that share the exact same cwd.
    ///
    /// This is an in-process future-lifetime guard. It intentionally does not
    /// claim to track independently running background processes after their
    /// daemon-managed tool future has returned or been dropped.
    pub async fn acquire_cwd_mutation_guard(&self, workspace_id: &str) -> OwnedMutexGuard<()> {
        let guard = {
            let mut guards = self.cwd_mutation_guards.lock().await;
            guards.retain(|_, guard| Arc::strong_count(guard) > 1);
            guards
                .entry(PathBuf::from(workspace_id))
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        guard.lock_owned().await
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
    pub async fn materialize_session(
        &self,
        project_id: Uuid,
        workspace_id: &str,
        project_workspaces: &[ProjectWorkspace],
        selected_workspaces: &[SelectedWorkspace],
    ) -> Result<(String, Vec<SessionWorkspace>)> {
        let root = self.session_root(workspace_id);
        if root.exists() {
            bail!("session workspace already exists: {}", root.display());
        }
        tokio::fs::create_dir_all(&root).await?;
        let cwd = root.join("cwd");
        create_session_subvolume(&cwd).await?;
        // Any failure after the cwd subvolume exists must tear the whole session
        // tree down; otherwise a partial materialize leaks a btrfs subvolume that
        // no later call reclaims (every session uses a fresh workspace_id).
        let materialized = async {
            let _workspace_base_guard = self.workspace_base_lock.lock().await;
            self.remove_stale_workspace_bases(project_id, project_workspaces)
                .await?;
            let mut workspaces = Vec::with_capacity(selected_workspaces.len());
            for selected in selected_workspaces {
                workspaces.push(
                    self.materialize_workspace(
                        project_id,
                        workspace_id,
                        &cwd,
                        &selected.workspace,
                        selected.branch_override.as_deref(),
                    )
                    .await?,
                );
            }
            Ok::<_, anyhow::Error>(workspaces)
        }
        .await;
        match materialized {
            Ok(workspaces) => Ok((workspace_id.to_string(), workspaces)),
            Err(error) => {
                let _ = self.destroy_session_workspaces(workspace_id).await;
                Err(error)
            }
        }
    }

    pub async fn ensure_session(
        &self,
        workspace_id: &str,
        workspaces: &[SessionWorkspace],
    ) -> Result<()> {
        if workspaces.is_empty() {
            return Ok(());
        }
        let cwd = self.resolve(workspace_id);
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

    pub async fn ensure_session_owns_cwd(&self, workspace_id: &str) -> Result<()> {
        let session_root = self.session_root(workspace_id);
        let expected = session_root.join("cwd");
        let root_metadata = tokio::fs::symlink_metadata(&session_root)
            .await
            .with_context(|| format!("inspect managed session root {}", session_root.display()))?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            bail!(
                "managed session root is not a directory: {}",
                session_root.display()
            );
        }
        let cwd_metadata = tokio::fs::symlink_metadata(&expected)
            .await
            .with_context(|| format!("inspect managed session cwd {}", expected.display()))?;
        if cwd_metadata.file_type().is_symlink() || !cwd_metadata.is_dir() {
            bail!(
                "managed session cwd is not a directory: {}",
                expected.display()
            );
        }
        Ok(())
    }

    pub async fn fork_session_from_parent(
        &self,
        parent_workspace_id: &str,
        parent_workspaces: &[SessionWorkspace],
        child_workspace_id: &str,
    ) -> Result<(String, Vec<SessionWorkspace>)> {
        if parent_workspace_id == child_workspace_id {
            bail!("child session id must differ from parent session id");
        }
        self.ensure_session(parent_workspace_id, parent_workspaces)
            .await?;

        let child_root = self.session_root(child_workspace_id);
        let parent_cwd = self.resolve(parent_workspace_id);
        if child_root.starts_with(&parent_cwd) {
            bail!(
                "child session root {} must not be inside parent cwd {}",
                child_root.display(),
                parent_cwd.display()
            );
        }
        if child_root.exists() {
            bail!(
                "child session workspace already exists: {}",
                child_root.display()
            );
        }
        tokio::fs::create_dir(&child_root)
            .await
            .with_context(|| format!("create child session root {}", child_root.display()))?;

        let result = async {
            self.ensure_session_owns_cwd(parent_workspace_id).await?;
            let child_cwd = child_root.join("cwd");
            snapshot_session(&parent_cwd, &child_cwd)
                .await
                .with_context(|| {
                    format!(
                        "fork parent session cwd {} to child cwd {}",
                        parent_cwd.display(),
                        child_cwd.display()
                    )
                })?;

            let mut child_workspaces = Vec::with_capacity(parent_workspaces.len());
            for workspace in parent_workspaces {
                validate_workspace_dir(&workspace.workspace_dir)?;
                let child_workspace_root = child_cwd.join(&workspace.workspace_dir);
                let mut child_workspace = workspace.clone();
                if workspace.kind == WorkspaceKind::Git {
                    validate_git_workspace_isolated(&child_workspace_root).await?;
                    let local_branch = local_branch(child_workspace_id, &workspace.workspace_dir);
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

            Ok((child_workspace_id.to_string(), child_workspaces))
        }
        .await;
        match result {
            Ok(result) => Ok(result),
            Err(error) => match self.destroy_session_workspaces(child_workspace_id).await {
                Ok(()) => Err(error),
                Err(cleanup_error) => {
                    Err(error.context(format!("clean up failed fork: {cleanup_error:#}")))
                }
            },
        }
    }

    /// Reclaim a session's entire workspace tree, including any btrfs
    /// subvolumes created while instantiating or forking it. This is the single
    /// teardown primitive; callers that only want directory removal still route
    /// through it so subvolume metadata is never leaked.
    pub async fn destroy_session_workspaces(&self, workspace_id: &str) -> Result<()> {
        let root = self.session_root(workspace_id);
        destroy_session_subvolume(&root.join("cwd")).await?;
        // Idempotent: a re-issued teardown (or the materialize cleanup path
        // above) may find the root already gone.
        match tokio::fs::remove_dir(&root).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn reconcile_project_bases(
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

    pub async fn remove_project_bases(&self, project_id: Uuid) -> Result<()> {
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
            tokio::fs::create_dir_all(&base_path).await?;
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

        populate_workspace(&base.path, &target).await?;
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
        populate_workspace(&base.path, &target).await?;
        Ok(SessionWorkspace::local(workspace_dir, source_path))
    }
}

#[derive(Debug)]
struct WorkspaceBase {
    path: PathBuf,
    config: WorkspaceBaseConfig,
}

async fn collect_instruction(
    path: &Path,
    workspace: Option<&str>,
    files: &mut Vec<RawInstructionFile>,
) -> Result<()> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) if !contents.trim().is_empty() => files.push(RawInstructionFile {
            workspace: workspace.map(str::to_string),
            path: path.display().to_string(),
            contents,
        }),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Read every `<dir>/<name>/SKILL.md` into a package map. A missing directory
/// is not an error. Later sources replace same-named packages.
async fn collect_skill_dir(
    dir: &Path,
    workspace: Option<&str>,
    kind: SkillKind,
    origin: SkillOrigin,
    files: &mut std::collections::BTreeMap<String, RawSkillFile>,
) -> Result<()> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let Ok(contents) = tokio::fs::read_to_string(entry.path().join("SKILL.md")).await else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let package_key = match workspace {
            Some(workspace) => format!("{workspace}/{name}"),
            None => name.to_string(),
        };
        let skill_path = entry.path().join("SKILL.md");
        files.insert(
            package_key,
            RawSkillFile {
                kind: kind.clone(),
                origin: origin.clone(),
                workspace: workspace.map(str::to_string),
                package_name: name.to_string(),
                path: skill_path.display().to_string(),
                contents,
            },
        );
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
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "pi-runtime-{prefix}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, contents).expect("write");
    }

    #[tokio::test]
    async fn collect_skill_dir_loads_immediate_children_only() {
        let root = temp_dir("collect-skill");
        let skills = root.join(".agents/skills");
        write_file(
            &skills.join("reviewer/SKILL.md"),
            "---\nname: reviewer\ndescription: review\n---\nbody",
        );
        // A loose file directly under `.agents/skills` is not a skill package.
        write_file(&skills.join("loose.md"), "not a package");
        // A `SKILL.md` nested deeper than an immediate child is not discovered.
        write_file(
            &skills.join("group/deep/SKILL.md"),
            "---\nname: deep\ndescription: deep\n---\nbody",
        );

        let mut files = std::collections::BTreeMap::new();
        collect_skill_dir(
            &skills,
            Some("repo"),
            SkillKind::Skill,
            SkillOrigin::WorkspaceProject,
            &mut files,
        )
        .await
        .expect("collect");
        assert_eq!(files.len(), 1);
        let file = files.get("repo/reviewer").expect("reviewer");
        assert_eq!(file.workspace.as_deref(), Some("repo"));
        assert_eq!(file.origin, SkillOrigin::WorkspaceProject);
        assert_eq!(
            file.path,
            skills.join("reviewer/SKILL.md").display().to_string()
        );
        assert!(file.contents.contains("name: reviewer"));

        // A missing directory is not an error and yields nothing.
        let mut none = std::collections::BTreeMap::new();
        collect_skill_dir(
            &root.join("absent"),
            None,
            SkillKind::Skill,
            SkillOrigin::HomeGlobal,
            &mut none,
        )
        .await
        .expect("missing dir is ok");
        assert!(none.is_empty());

        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn runtime_context_merges_sources_with_personal_precedence() {
        let root = temp_dir("runtime-context");
        let state = root.join("state");
        let config = root.join("config/pi-relay/runtime");
        let home = root.join("home");
        let cwd = state.join("sessions/session-test/cwd");
        write_file(&config.join("AGENTS.md"), "global instructions");
        write_file(
            &home.join(".agents/skills/swe/SKILL.md"),
            "---\nname: swe\ndescription: global\n---\nglobal",
        );
        write_file(
            &config.join("skills/workflow-review/SKILL.md"),
            "---\nname: workflow-review\ndescription: workflow\n---\nworkflow",
        );
        write_file(
            &config.join("subagent-roles/reviewer/SKILL.md"),
            "---\nname: reviewer\ndescription: role\n---\nrole",
        );
        write_file(&cwd.join("repo/AGENTS.md"), "repo instructions");
        write_file(
            &cwd.join("other/.agents/skills/placeholder/SKILL.md"),
            "---\nname: placeholder\ndescription: selected workspace only\n---\nother",
        );
        write_file(
            &cwd.join("repo/.agents/skills/kubernetes/SKILL.md"),
            "---\nname: kubernetes\ndescription: contributed\n---\ncontributed",
        );
        write_file(
            &home.join(".agents/projects/repo/skills/kubernetes/SKILL.md"),
            "---\nname: kubernetes\ndescription: personal\n---\npersonal",
        );

        let manager = WorkspaceManager::new(state, config, home);
        // Home project overlays key off project_key, not selected workspace_dirs.
        // Selecting only "other" still surfaces projects/repo/skills/kubernetes.
        let context = manager
            .read_runtime_context("session-test", &["other".to_string()], Some("repo"))
            .await
            .expect("runtime context");

        assert_eq!(context.instructions.len(), 1);
        assert_eq!(context.instructions[0].workspace, None);
        assert!(context.skills.iter().any(|skill| {
            skill.package_name == "swe" && skill.origin == SkillOrigin::HomeGlobal
        }));
        assert!(context.skills.iter().any(|skill| {
            skill.package_name == "workflow-review" && skill.origin == SkillOrigin::RuntimeWorkflow
        }));
        assert!(context.skills.iter().any(|skill| {
            skill.package_name == "reviewer" && skill.kind == SkillKind::SubagentRole
        }));
        assert!(context.skills.iter().any(|skill| {
            skill.package_name == "placeholder" && skill.origin == SkillOrigin::WorkspaceProject
        }));
        let kubernetes = context
            .skills
            .iter()
            .find(|skill| skill.package_name == "kubernetes")
            .expect("kubernetes");
        assert_eq!(kubernetes.origin, SkillOrigin::HomeProject);
        assert_eq!(kubernetes.workspace.as_deref(), Some("repo"));
        assert!(kubernetes.contents.contains("personal"));

        std::fs::remove_dir_all(root).ok();
    }
}
