use std::collections::BTreeMap;

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
    Injected(InjectedMessage),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectedMessage {
    pub kind: String,
    pub content: String,
    pub metadata: BTreeMap<String, String>,
}

impl InjectedMessage {
    pub fn new(kind: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            content: content.into(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn with_metadata(
        kind: impl Into<String>,
        content: impl Into<String>,
        metadata: BTreeMap<String, String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            content: content.into(),
            metadata,
        }
    }
}

impl TranscriptItem {
    pub fn turn_id(&self) -> Option<TurnId> {
        match self {
            TranscriptItem::TurnStarted { turn_id }
            | TranscriptItem::ToolCallStarted { turn_id, .. }
            | TranscriptItem::TurnFinished { turn_id, .. } => Some(*turn_id),
            TranscriptItem::UserMessage(_)
            | TranscriptItem::AssistantMessage(_)
            | TranscriptItem::ToolResult(_)
            | TranscriptItem::Injected(_) => None,
        }
    }
}
