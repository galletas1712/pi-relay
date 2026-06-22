use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
};

use agent_store::{ActionKind, EventFrame, PostgresAgentStore};
use agent_tools::ToolRegistry;
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use crate::provider_runtime::{ProviderConnectionRegistry, SessionTitleScheduler};
use crate::repl::ReplRegistry;
use crate::types::RuntimeSession;
use crate::workspaces::WorkspaceManager;

pub(crate) struct RunningTask {
    pub(crate) session_id: String,
    pub(crate) action_row_id: String,
    pub(crate) kind: ActionKind,
    pub(crate) handle: JoinHandle<()>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) repo: Arc<PostgresAgentStore>,
    pub(crate) active: Arc<Mutex<HashMap<String, Arc<Mutex<RuntimeSession>>>>>,
    pub(crate) session_driver_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    pub(crate) tasks: Arc<StdMutex<HashMap<String, RunningTask>>>,
    pub(crate) events: broadcast::Sender<EventFrame>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) provider_connections: ProviderConnectionRegistry,
    pub(crate) session_titles: SessionTitleScheduler,
    pub(crate) repls: ReplRegistry,
    pub(crate) workspaces: WorkspaceManager,
    pub(crate) prompt_root: PathBuf,
}
