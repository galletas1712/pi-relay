use std::{collections::HashMap, sync::Arc};

use agent_store::{EventFrame, PostgresAgentStore};
use agent_tools::{ToolContext, ToolRegistry};
use tokio::sync::{broadcast, Mutex};

use crate::types::RuntimeSession;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) repo: Arc<PostgresAgentStore>,
    pub(crate) active: Arc<Mutex<HashMap<String, Arc<Mutex<RuntimeSession>>>>>,
    pub(crate) pump_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    pub(crate) events: broadcast::Sender<EventFrame>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) tool_context: ToolContext,
}
