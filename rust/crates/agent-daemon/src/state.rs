use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc, Mutex as StdMutex},
};

use agent_prompt::PromptProfile;
use agent_store::{ActionKind, EventFrame, PostCompactionDispatchLease, PostgresAgentStore};
use agent_tools::{ProviderTool, ToolRegistry};
use agent_vocab::ProviderKind;
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::provider_runtime::{ProviderConnectionRegistry, SessionTitleScheduler};
use crate::runtime::SessionLockRegistry;
use crate::types::RuntimeSession;
use crate::workspaces::WorkspaceManager;

#[derive(Clone)]
pub(crate) struct ProviderToolSnapshots {
    openai_parent: Arc<[ProviderTool]>,
    openai_subagent: Arc<[ProviderTool]>,
    claude_parent: Arc<[ProviderTool]>,
    claude_subagent: Arc<[ProviderTool]>,
}

impl ProviderToolSnapshots {
    pub(crate) fn new(registry: &ToolRegistry) -> Self {
        let openai_parent: Arc<[ProviderTool]> = registry
            .provider_tools_for_provider(ProviderKind::OpenAi)
            .into();
        let claude_parent: Arc<[ProviderTool]> = registry
            .provider_tools_for_provider(ProviderKind::Claude)
            .into();
        Self {
            openai_subagent: subagent_tools(&openai_parent).into(),
            claude_subagent: subagent_tools(&claude_parent).into(),
            openai_parent,
            claude_parent,
        }
    }

    pub(crate) fn get(
        &self,
        provider: ProviderKind,
        profile: PromptProfile,
    ) -> Arc<[ProviderTool]> {
        match (provider, profile) {
            (ProviderKind::OpenAi, PromptProfile::Parent) => Arc::clone(&self.openai_parent),
            (ProviderKind::OpenAi, PromptProfile::Subagent) => Arc::clone(&self.openai_subagent),
            (ProviderKind::Claude, PromptProfile::Parent) => Arc::clone(&self.claude_parent),
            (ProviderKind::Claude, PromptProfile::Subagent) => Arc::clone(&self.claude_subagent),
        }
    }
}

fn subagent_tools(tools: &[ProviderTool]) -> Vec<ProviderTool> {
    tools
        .iter()
        .filter(|tool| {
            !matches!(
                tool.canonical_name.as_str(),
                "delegate_writing_task"
                    | "delegate_readonly_tasks"
                    | "inspect_delegation"
                    | "cancel_delegation"
                    | "steer_subagent"
                    | "interrupt_subagent"
            )
        })
        .cloned()
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskRegistrationId(Uuid);

impl TaskRegistrationId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

pub(crate) struct RunningTask {
    pub(crate) session_id: String,
    pub(crate) action_row_id: String,
    pub(crate) registration_id: TaskRegistrationId,
    pub(crate) post_compaction_dispatch_lease: Option<PostCompactionDispatchLease>,
    pub(crate) kind: ActionKind,
    pub(crate) handle: JoinHandle<()>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) repo: Arc<PostgresAgentStore>,
    pub(crate) active: Arc<Mutex<HashMap<String, Arc<Mutex<RuntimeSession>>>>>,
    pub(crate) session_driver_locks: SessionLockRegistry,
    pub(crate) tasks: Arc<StdMutex<HashMap<String, RunningTask>>>,
    pub(crate) auxiliary_tasks: Arc<StdMutex<Vec<JoinHandle<()>>>>,
    pub(crate) task_registration_lock: Arc<StdMutex<()>>,
    pub(crate) post_compaction_recovery_scheduled: Arc<AtomicBool>,
    pub(crate) post_compaction_recovery_notify: Arc<tokio::sync::Notify>,
    pub(crate) post_compaction_recovery_task: Arc<StdMutex<Option<JoinHandle<()>>>>,
    pub(crate) shutting_down: Arc<AtomicBool>,
    pub(crate) events: broadcast::Sender<EventFrame>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) provider_tools: ProviderToolSnapshots,
    pub(crate) provider_connections: ProviderConnectionRegistry,
    pub(crate) session_titles: SessionTitleScheduler,
    pub(crate) workspaces: WorkspaceManager,
    pub(crate) prompt_root: PathBuf,
    #[cfg(test)]
    pub(crate) pause_subagent_control_after_commit: Arc<AtomicBool>,
    #[cfg(test)]
    pub(crate) subagent_control_committed: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    pub(crate) fail_subagent_control_reload_after_commit: Arc<AtomicBool>,
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
