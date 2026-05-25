use agent_session::{AgentSession, SessionAction};
use agent_store::SessionConfig;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub(crate) struct RpcRequest {
    pub(crate) id: Value,
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) params: Value,
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
    #[serde(default)]
    pub(crate) data: Value,
}

pub(crate) struct RuntimeSession {
    pub(crate) session: AgentSession,
    pub(crate) config: SessionConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct DispatchAction {
    pub(crate) row_id: String,
    pub(crate) attempt_id: String,
    pub(crate) action: SessionAction,
    pub(crate) config: SessionConfig,
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
    InputInterrupt,
    HistoryTree,
    HistoryContext,
    HistorySwitch,
    HistoryFork,
    TurnResume,
    ToolsList,
    CompactionRequest,
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
            "input.interrupt" => Some(Self::InputInterrupt),
            "history.tree" => Some(Self::HistoryTree),
            "history.context" => Some(Self::HistoryContext),
            "history.switch" => Some(Self::HistorySwitch),
            "history.fork" => Some(Self::HistoryFork),
            "turn.resume" => Some(Self::TurnResume),
            "tools.list" => Some(Self::ToolsList),
            "compaction.request" => Some(Self::CompactionRequest),
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
            RpcMethod::parse("session.sync_active_branch"),
            Some(RpcMethod::SessionSyncActiveBranch)
        );
        assert_eq!(RpcMethod::parse("turn.resume"), Some(RpcMethod::TurnResume));
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
