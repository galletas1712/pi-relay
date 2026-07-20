use agent_runtime_protocol::ProjectWorkspace;

/// A project workspace selected for a session, paired with any branch override to
/// apply when materializing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedWorkspace {
    pub workspace: ProjectWorkspace,
    pub branch_override: Option<String>,
}
