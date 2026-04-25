use std::collections::BTreeMap;

use crate::ids::TurnId;
use crate::message::{AssistantMessage, ToolCall, ToolResultMessage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Graceful,
    Interrupted,
    Crashed,
}

/// One model-visible item in an agent's materialized context.
///
/// These items are persisted inside transcript entries, replayed, compacted,
/// forked, and rewound. They are not hook/lifecycle events; hooks should
/// attach to a separate lifecycle notification stream derived while the loop
/// is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextItem {
    // Produced by the FSM during a turn:
    TurnStarted {
        turn_id: TurnId,
    },
    UserMessage(String),
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

    /// Durable, model-visible context injected by orchestration or session
    /// machinery. This covers tagged turn-opening inputs, compaction
    /// summaries, and future spawn/report extension messages. Different uses
    /// are discriminated by `InjectedMessage::kind` plus metadata.
    Injected(InjectedMessage),
}

/// Back-compat name for `ContextItem`.
pub type TranscriptRecord = ContextItem;

/// Payload carried by `ContextItem::Injected`.
///
/// `kind` is a free-form tag chosen by the session, orchestrator, or another
/// extension point to classify the injected context. `content` is the textual
/// body surfaced to the model; `metadata` carries routing or anchor
/// information, such as sender id or the first kept entry after compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
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

    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

impl ContextItem {
    pub fn turn_id(&self) -> Option<TurnId> {
        match self {
            ContextItem::TurnStarted { turn_id }
            | ContextItem::ToolCallStarted { turn_id, .. }
            | ContextItem::TurnFinished { turn_id, .. } => Some(*turn_id),
            ContextItem::UserMessage(_)
            | ContextItem::AssistantMessage(_)
            | ContextItem::ToolResult(_)
            | ContextItem::Injected(_) => None,
        }
    }
}
