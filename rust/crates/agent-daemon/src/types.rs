use agent_session::{AgentSession, SessionAction};
use agent_store::{EventFrame, SessionConfig};
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LiveEventFrame {
    pub(crate) event_id: i64,
    pub(crate) event: agent_store::EventType,
    pub(crate) session_id: String,
    pub(crate) data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) view_update: Option<SessionViewUpdate>,
}

impl LiveEventFrame {
    pub(crate) fn from_event(event: EventFrame) -> Self {
        let view_update = SessionViewUpdate::from_event(&event);
        Self {
            event_id: event.event_id,
            event: event.event,
            session_id: event.session_id,
            data: event.data,
            view_update,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SessionViewUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) overview: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) active_branch: Option<ActiveBranchViewUpdate>,
}

impl SessionViewUpdate {
    pub(crate) fn from_event(event: &EventFrame) -> Option<Self> {
        match event.event.view_update_policy() {
            agent_store::EventViewUpdatePolicy::AppendActiveBranchEntry => {
                let entry = event.data.get("entry")?.clone();
                if entry.is_null() {
                    return None;
                }
                let overview = entry.get("id").and_then(Value::as_str).map(|entry_id| {
                    json!({
                        "active_leaf_id": entry_id,
                        "has_transcript_entries": true,
                    })
                });
                Some(Self {
                    overview,
                    active_branch: Some(ActiveBranchViewUpdate::AppendEntry { entry }),
                })
            }
            agent_store::EventViewUpdatePolicy::ReloadActiveBranch => Some(Self {
                overview: activity_overview_patch(&event.data),
                active_branch: Some(ActiveBranchViewUpdate::ReloadRequired {
                    reason: Some(format!("{}", event.event)),
                }),
            }),
            agent_store::EventViewUpdatePolicy::OverviewChanged => None,
            agent_store::EventViewUpdatePolicy::Noop => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ActiveBranchViewUpdate {
    AppendEntry {
        entry: Value,
    },
    ReloadRequired {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

fn activity_overview_patch(data: &Value) -> Option<Value> {
    data.get("activity")
        .and_then(Value::as_str)
        .map(|activity| json!({ "activity": activity }))
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
    HistoryRewind,
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
            "history.rewind" => Some(Self::HistoryRewind),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForkPlacement {
    At,
    Before,
}

impl ForkPlacement {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "at" => Some(Self::At),
            "before" => Some(Self::Before),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::At => "at",
            Self::Before => "before",
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
    use agent_store::{EventFrame, EventType};
    use serde_json::{json, Value};

    #[test]
    fn rpc_methods_parse_at_the_boundary() {
        assert_eq!(
            RpcMethod::parse("input.follow_up"),
            Some(RpcMethod::InputFollowUp)
        );
        assert_eq!(RpcMethod::parse("turn.resume"), Some(RpcMethod::TurnResume));
        assert_eq!(RpcMethod::parse("input.fly"), None);
    }

    #[test]
    fn fork_placement_has_explicit_wire_values() {
        assert_eq!(ForkPlacement::parse("before"), Some(ForkPlacement::Before));
        assert_eq!(ForkPlacement::Before.as_str(), "before");
        assert_eq!(ForkPlacement::parse("root"), None);
    }

    #[test]
    fn live_event_adds_append_entry_view_update_without_mutating_data() {
        let event = EventFrame {
            event_id: 7,
            event: EventType::TranscriptAppended,
            session_id: "session_1".to_string(),
            data: json!({
                "entry_id": "entry_2",
                "entry": {
                    "id": "entry_2",
                    "parent_id": "entry_1",
                    "timestamp_ms": 1,
                    "item": { "type": "user_message", "content": [] },
                    "provider_replay": []
                }
            }),
        };

        let live = LiveEventFrame::from_event(event);
        let value = serde_json::to_value(live).expect("live event serializes");

        assert_eq!(value["data"]["view_update"], Value::Null);
        assert_eq!(
            value["view_update"]["active_branch"]["kind"],
            "append_entry"
        );
        assert_eq!(
            value["view_update"]["active_branch"]["entry"]["id"],
            "entry_2"
        );
    }

    #[test]
    fn live_event_marks_branch_reload_without_embedding_branch() {
        let event = EventFrame {
            event_id: 8,
            event: EventType::HistoryRewound,
            session_id: "session_1".to_string(),
            data: json!({ "active_leaf_id": "entry_1" }),
        };

        let live = LiveEventFrame::from_event(event);
        let value = serde_json::to_value(live).expect("live event serializes");

        assert_eq!(
            value["view_update"]["active_branch"]["kind"],
            "reload_required"
        );
        assert!(value["view_update"]["active_branch"]
            .get("entries")
            .is_none());
    }
}
