use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionSummary {
    pub source_session_id: String,
    pub source_leaf_id: String,
    pub summary: String,
    pub tokens_before: Option<usize>,
    pub last_turn_id: TurnId,
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
        }
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
            | TranscriptItem::ToolResult(_) => None,
        }
    }
}
