use serde::{Deserialize, Serialize};

use crate::daemon_observation::DaemonToolObservation;
use crate::ids::TurnId;
use crate::message::{AssistantMessage, ToolCall, ToolResultMessage, UserMessage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnOutcome {
    Graceful,
    Interrupted,
    Crashed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptItem {
    TurnStarted {
        turn_id: TurnId,
    },
    UserMessage(UserMessage),
    AssistantMessage(AssistantMessage),
    ToolCallStarted {
        turn_id: TurnId,
        tool_call: ToolCall,
    },
    ToolResult(ToolResultMessage),
    TurnFinished {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
    CompactionSummary(CompactionSummary),
    DaemonToolObservation(DaemonToolObservation),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionSummary {
    pub source_session_id: String,
    pub source_leaf_id: String,
    pub summary: String,
    pub tokens_before: Option<usize>,
    pub last_turn_id: TurnId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_at_ms: Option<u64>,
}

impl CompactionSummary {
    pub fn new(
        source_session_id: impl Into<String>,
        source_leaf_id: impl Into<String>,
        summary: impl Into<String>,
        tokens_before: Option<usize>,
        last_turn_id: TurnId,
    ) -> Self {
        Self {
            source_session_id: source_session_id.into(),
            source_leaf_id: source_leaf_id.into(),
            summary: summary.into(),
            tokens_before,
            last_turn_id,
            turn_started_at_ms: None,
        }
    }

    pub fn with_turn_started_at_ms(mut self, turn_started_at_ms: Option<u64>) -> Self {
        self.turn_started_at_ms = turn_started_at_ms;
        self
    }
}

impl TranscriptItem {
    pub fn turn_id(&self) -> Option<TurnId> {
        match self {
            TranscriptItem::TurnStarted { turn_id }
            | TranscriptItem::ToolCallStarted { turn_id, .. }
            | TranscriptItem::TurnFinished { turn_id, .. } => Some(*turn_id),
            TranscriptItem::CompactionSummary(summary) => Some(summary.last_turn_id),
            TranscriptItem::UserMessage(_)
            | TranscriptItem::AssistantMessage(_)
            | TranscriptItem::ToolResult(_)
            | TranscriptItem::DaemonToolObservation(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolResultStatus;
    use serde_json::json;

    #[test]
    fn daemon_tool_observation_transcript_item_round_trips_with_type_tag() {
        let item = TranscriptItem::DaemonToolObservation(DaemonToolObservation {
            tool_name: "delegation_status".to_string(),
            args_json: "{\"delegation_id\":\"delegation_1\"}".to_string(),
            result_json: json!({ "delegation_id": "delegation_1", "status": "done" }),
            status: ToolResultStatus::Success,
            summary: Some("done".to_string()),
        });

        let value = serde_json::to_value(&item).expect("serialize");
        assert_eq!(value["type"], "daemon_tool_observation");
        assert_eq!(value["tool_name"], "delegation_status");

        let round_trip: TranscriptItem = serde_json::from_value(value).expect("deserialize");
        assert_eq!(round_trip, item);
        assert_eq!(round_trip.turn_id(), None);
    }

    /// Rows written before `tool_call_id` was retired still carry the key.
    /// They must deserialize (serde ignores it) and render unchanged, so the
    /// one-time cleanup migration stays optional.
    #[test]
    fn daemon_tool_observation_transcript_item_ignores_retired_tool_call_id() {
        let stored = json!({
            "type": "daemon_tool_observation",
            "tool_call_id": "call_inspect_delegation_deadbeef",
            "tool_name": "inspect_delegation",
            "args_json": "{\"delegation_id\":\"delegation_1\"}",
            "result_json": { "delegation_id": "delegation_1", "status": "done" },
            "status": "Success",
            "summary": "Delegation delegation_1 completed",
        });

        let item: TranscriptItem = serde_json::from_value(stored).expect("deserialize old row");
        let TranscriptItem::DaemonToolObservation(observation) = &item else {
            panic!("expected a daemon observation");
        };

        assert_eq!(observation.tool_name, "inspect_delegation");
        let text = observation.render_text().expect("observation renders");
        assert!(text.starts_with("Daemon observation: inspect_delegation"));
        assert!(text.contains("Summary: Delegation delegation_1 completed"));
    }
}
