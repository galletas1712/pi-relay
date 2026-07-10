use agent_mcp::McpSessionSnapshot;
use agent_session::{AgentSession, SessionAction};
use agent_store::{PostCompactionDispatchLease, SessionConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub(crate) struct RpcRequest {
    pub(crate) id: Value,
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) params: Value,
}

impl From<agent_mcp::McpManagerError> for RpcError {
    fn from(error: agent_mcp::McpManagerError) -> Self {
        match error {
            agent_mcp::McpManagerError::InventoryChanged { current_revision } => Self {
                code: "mcp_inventory_changed".to_string(),
                message: "MCP inventory changed; refresh and review the selection".to_string(),
                data: json!({ "current_revision": current_revision }),
            },
            agent_mcp::McpManagerError::SelectionInvalid { message } => {
                Self::new("mcp_selection_invalid", message)
            }
            agent_mcp::McpManagerError::Unavailable { server } => Self::new(
                "mcp_unavailable",
                format!("selected MCP server {server} is unavailable"),
            ),
            agent_mcp::McpManagerError::Catalog(error) => Self::new(
                "mcp_selection_invalid",
                format!("invalid MCP catalog: {error:#}"),
            ),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RpcResponse {
    pub(crate) id: Value,
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<RpcErrorBody>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RpcErrorBody {
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) data: Value,
}

pub(crate) struct RuntimeSession {
    pub(crate) session: AgentSession,
    pub(crate) config: SessionConfig,
    pub(crate) persisted_active_leaf_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DispatchAction {
    pub(crate) row_id: String,
    pub(crate) attempt_id: String,
    pub(crate) post_compaction_dispatch_lease: Option<PostCompactionDispatchLease>,
    pub(crate) action: SessionAction,
    pub(crate) config: SessionConfig,
    pub(crate) mcp_snapshot: McpSessionSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RpcMethod {
    SessionStart,
    SessionList,
    SessionGet,
    SessionSyncActiveBranch,
    SessionRename,
    SessionConfigure,
    SessionDelete,
    ProjectList,
    ProjectCreate,
    ProjectUpdate,
    ProjectDelete,
    SystemPrompt,
    EventsSubscribe,
    EventsUnsubscribe,
    InputFollowUp,
    InputPromoteQueued,
    InputUpdateQueued,
    InputCancelQueued,
    InputReorderQueuedFollowUps,
    InputInterrupt,
    TranscriptIndex,
    TranscriptEntries,
    TranscriptTurns,
    TranscriptTurnDetail,
    HistoryTree,
    HistoryContext,
    HistorySwitch,
    TurnResume,
    McpInventory,
    ToolsList,
    CompactionRequest,
    DelegationStartFull,
    DelegationStartReadonlyFanout,
    DelegationStatus,
    DelegationCancel,
    DelegationSteerSubagent,
    DelegationList,
    DelegationReadHandoffFile,
    HarnessModelComplete,
    HarnessModelFail,
}

impl RpcMethod {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "session.start" => Some(Self::SessionStart),
            "session.list" => Some(Self::SessionList),
            "session.get" => Some(Self::SessionGet),
            "session.sync_active_branch" => Some(Self::SessionSyncActiveBranch),
            "session.rename" => Some(Self::SessionRename),
            "session.configure" => Some(Self::SessionConfigure),
            "session.delete" => Some(Self::SessionDelete),
            "project.list" => Some(Self::ProjectList),
            "project.create" => Some(Self::ProjectCreate),
            "project.update" => Some(Self::ProjectUpdate),
            "project.delete" => Some(Self::ProjectDelete),
            "system.prompt" => Some(Self::SystemPrompt),
            "events.subscribe" => Some(Self::EventsSubscribe),
            "events.unsubscribe" => Some(Self::EventsUnsubscribe),
            "input.follow_up" => Some(Self::InputFollowUp),
            "input.promote_queued" => Some(Self::InputPromoteQueued),
            "input.update_queued" => Some(Self::InputUpdateQueued),
            "input.cancel_queued" => Some(Self::InputCancelQueued),
            "input.reorder_queued_follow_ups" => Some(Self::InputReorderQueuedFollowUps),
            "input.interrupt" => Some(Self::InputInterrupt),
            "transcript.index" => Some(Self::TranscriptIndex),
            "transcript.entries" => Some(Self::TranscriptEntries),
            "transcript.turns" => Some(Self::TranscriptTurns),
            "transcript.turn_detail" => Some(Self::TranscriptTurnDetail),
            "history.tree" => Some(Self::HistoryTree),
            "history.context" => Some(Self::HistoryContext),
            "history.switch" => Some(Self::HistorySwitch),
            "turn.resume" => Some(Self::TurnResume),
            "mcp.inventory" => Some(Self::McpInventory),
            "tools.list" => Some(Self::ToolsList),
            "compaction.request" => Some(Self::CompactionRequest),
            "delegation.start_full" => Some(Self::DelegationStartFull),
            "delegation.start_readonly_fanout" => Some(Self::DelegationStartReadonlyFanout),
            "delegation.status" => Some(Self::DelegationStatus),
            "delegation.cancel" => Some(Self::DelegationCancel),
            "delegation.steer_subagent" => Some(Self::DelegationSteerSubagent),
            "delegation.list" => Some(Self::DelegationList),
            "delegation.read_handoff_file" => Some(Self::DelegationReadHandoffFile),
            "harness.model.complete" => Some(Self::HarnessModelComplete),
            "harness.model.fail" => Some(Self::HarnessModelFail),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct RpcError {
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) data: Value,
}

impl RpcError {
    pub(crate) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: json!({}),
        }
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(error: anyhow::Error) -> Self {
        Self::new("internal_error", format!("{error:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_store::EventFrame;
    use serde_json::{json, Value};

    #[test]
    fn rpc_methods_parse_at_the_boundary() {
        assert_eq!(
            RpcMethod::parse("input.follow_up"),
            Some(RpcMethod::InputFollowUp)
        );
        assert_eq!(
            RpcMethod::parse("input.update_queued"),
            Some(RpcMethod::InputUpdateQueued)
        );
        assert_eq!(
            RpcMethod::parse("input.cancel_queued"),
            Some(RpcMethod::InputCancelQueued)
        );
        assert_eq!(
            RpcMethod::parse("input.reorder_queued_follow_ups"),
            Some(RpcMethod::InputReorderQueuedFollowUps)
        );
        assert_eq!(
            RpcMethod::parse("session.sync_active_branch"),
            Some(RpcMethod::SessionSyncActiveBranch)
        );
        assert_eq!(
            RpcMethod::parse("transcript.index"),
            Some(RpcMethod::TranscriptIndex)
        );
        assert_eq!(
            RpcMethod::parse("transcript.entries"),
            Some(RpcMethod::TranscriptEntries)
        );
        assert_eq!(
            RpcMethod::parse("transcript.turns"),
            Some(RpcMethod::TranscriptTurns)
        );
        assert_eq!(
            RpcMethod::parse("transcript.turn_detail"),
            Some(RpcMethod::TranscriptTurnDetail)
        );
        assert_eq!(RpcMethod::parse("turn.resume"), Some(RpcMethod::TurnResume));
        assert_eq!(
            RpcMethod::parse("delegation.start_full"),
            Some(RpcMethod::DelegationStartFull)
        );
        assert_eq!(
            RpcMethod::parse("delegation.start_readonly_fanout"),
            Some(RpcMethod::DelegationStartReadonlyFanout)
        );
        assert_eq!(
            RpcMethod::parse("delegation.status"),
            Some(RpcMethod::DelegationStatus)
        );
        assert_eq!(
            RpcMethod::parse("delegation.cancel"),
            Some(RpcMethod::DelegationCancel)
        );
        assert_eq!(
            RpcMethod::parse("delegation.steer_subagent"),
            Some(RpcMethod::DelegationSteerSubagent)
        );
        assert_eq!(
            RpcMethod::parse("delegation.list"),
            Some(RpcMethod::DelegationList)
        );
        assert_eq!(
            RpcMethod::parse("delegation.read_handoff_file"),
            Some(RpcMethod::DelegationReadHandoffFile)
        );
        for old in [
            "stage.start_full",
            "stage.start_readonly_fanout",
            "stage.status",
            "stage.cancel",
            "stage.list",
            "stage.read_handoff_file",
        ] {
            assert_eq!(RpcMethod::parse(old), None, "{old} must not parse");
        }
        assert_eq!(
            RpcMethod::parse("history.switch"),
            Some(RpcMethod::HistorySwitch)
        );
        assert_eq!(RpcMethod::parse("history.rewind"), None);
        assert_eq!(RpcMethod::parse("input.fly"), None);
    }

    #[test]
    fn live_events_are_plain_persisted_events() {
        let event = EventFrame {
            event_id: 7,
            event: agent_store::EventType::TranscriptAppended,
            session_id: "session_1".to_string(),
            data: json!({ "entry_id": "entry_2" }),
        };

        let value = serde_json::to_value(event).expect("event serializes");
        assert_eq!(value["view_update"], Value::Null);
    }
}
