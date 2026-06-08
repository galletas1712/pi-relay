use std::collections::BTreeMap;

use agent_store::{ProjectWorkspace, WorkspaceKind};
use anyhow::{bail, Result};

/// A workspace a new session requests, naming a project workspace directory and an
/// optional git branch override for that session's copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestedWorkspace {
    pub(crate) workspace_dir: String,
    /// Per-session git branch to populate instead of the project's configured
    /// `remote_branch`. `None` keeps the project default. Only valid for git
    /// workspaces.
    pub(crate) branch: Option<String>,
}

/// A project workspace selected for a session, paired with any branch override to
/// apply when materializing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedWorkspace {
    pub(crate) workspace: ProjectWorkspace,
    pub(crate) branch_override: Option<String>,
}

/// Which of a project's workspaces a new session should materialize.
///
/// `session.start` may scope a session to a subset of its project's workspaces so
/// unrelated workspace directories — and their `AGENTS.md` files and skills — stay
/// out of the session `outer_cwd` and the rendered prompt, and may override the git
/// branch each selected workspace starts from. Resolving a selection against the
/// project validates the requested directories/branches and preserves the project's
/// declared workspace order so prompt rendering stays deterministic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceSelection {
    /// Materialize every workspace the project declares, at its default branch.
    All,
    /// Materialize only the named workspace directories, in project order.
    Subset(Vec<RequestedWorkspace>),
}

impl WorkspaceSelection {
    /// Build a selection from an optional list of requested workspaces, where `None`
    /// (an omitted RPC field) means "all project workspaces at their default branch".
    pub(crate) fn from_requested(requested: Option<Vec<RequestedWorkspace>>) -> Self {
        match requested {
            Some(workspaces) => Self::Subset(workspaces),
            None => Self::All,
        }
    }

    /// Resolve the selection against the project's declared workspaces, returning the
    /// subset to materialize in project-declared order with branch overrides attached.
    ///
    /// Errors when the selection names a directory the project does not declare,
    /// contains duplicates, is empty, or requests a branch override for a non-git
    /// workspace. These are client-input errors and should surface as
    /// `invalid_params` at the RPC boundary.
    pub(crate) fn resolve(
        &self,
        project_workspaces: &[ProjectWorkspace],
    ) -> Result<Vec<SelectedWorkspace>> {
        let Self::Subset(requested) = self else {
            return Ok(project_workspaces
                .iter()
                .map(|workspace| SelectedWorkspace {
                    workspace: workspace.clone(),
                    branch_override: None,
                })
                .collect());
        };
        if requested.is_empty() {
            bail!("workspaces must select at least one workspace");
        }
        let mut overrides: BTreeMap<&str, Option<String>> = BTreeMap::new();
        for entry in requested {
            let Some(workspace) = project_workspaces
                .iter()
                .find(|workspace| workspace.workspace_dir == entry.workspace_dir)
            else {
                bail!(
                    "workspaces names a workspace not in the project: {}",
                    entry.workspace_dir
                );
            };
            if entry.branch.is_some() && workspace.kind != WorkspaceKind::Git {
                bail!(
                    "branch override is only supported for git workspaces: {}",
                    entry.workspace_dir
                );
            }
            if overrides
                .insert(entry.workspace_dir.as_str(), entry.branch.clone())
                .is_some()
            {
                bail!(
                    "workspaces contains a duplicate workspace: {}",
                    entry.workspace_dir
                );
            }
        }
        Ok(project_workspaces
            .iter()
            .filter_map(|workspace| {
                overrides
                    .get(workspace.workspace_dir.as_str())
                    .map(|branch_override| SelectedWorkspace {
                        workspace: workspace.clone(),
                        branch_override: branch_override.clone(),
                    })
            })
            .collect())
    }
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
