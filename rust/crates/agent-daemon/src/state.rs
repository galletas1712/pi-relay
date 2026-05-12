use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
};

use agent_store::{EventFrame, PostgresAgentStore};
use agent_tools::{ToolContext, ToolRegistry};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use crate::types::RuntimeSession;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) repo: Arc<PostgresAgentStore>,
    pub(crate) active: Arc<Mutex<HashMap<String, Arc<Mutex<RuntimeSession>>>>>,
    pub(crate) session_driver_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    pub(crate) dispatch_tasks: Arc<StdMutex<Vec<JoinHandle<()>>>>,
    pub(crate) events: broadcast::Sender<EventFrame>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) tool_context: ToolContext,
}
