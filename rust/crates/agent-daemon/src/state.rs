use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
};

use agent_store::{ActionKind, EventFrame, PostgresAgentStore};
use agent_tools::{ToolContext, ToolRegistry};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use crate::overlay::OverlayManager;
use crate::provider_runtime::ProviderConnectionRegistry;
use crate::types::RuntimeSession;

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
    pub(crate) overlays: OverlayManager,
    pub(crate) provider_connections: ProviderConnectionRegistry,
    pub(crate) default_tool_context: ToolContext,
    pub(crate) default_workspace: PathBuf,
}
