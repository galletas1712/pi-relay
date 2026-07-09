use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc, Mutex as StdMutex},
};

use agent_mcp::McpManager;
use agent_store::{ActionKind, EventFrame, PostCompactionDispatchLease, PostgresAgentStore};
use agent_tools::ToolRegistry;
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::provider_runtime::{ProviderConnectionRegistry, SessionTitleScheduler};
use crate::types::RuntimeSession;
use crate::workspaces::WorkspaceManager;

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
    pub(crate) session_driver_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    pub(crate) tasks: Arc<StdMutex<HashMap<String, RunningTask>>>,
    pub(crate) auxiliary_tasks: Arc<StdMutex<Vec<JoinHandle<()>>>>,
    pub(crate) task_registration_lock: Arc<StdMutex<()>>,
    pub(crate) post_compaction_recovery_scheduled: Arc<AtomicBool>,
    pub(crate) post_compaction_recovery_notify: Arc<tokio::sync::Notify>,
    pub(crate) post_compaction_recovery_task: Arc<StdMutex<Option<JoinHandle<()>>>>,
    pub(crate) shutting_down: Arc<AtomicBool>,
    pub(crate) events: broadcast::Sender<EventFrame>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) mcp: Arc<McpManager>,
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
