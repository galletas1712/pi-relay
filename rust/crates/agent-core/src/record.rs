use std::collections::BTreeMap;

use crate::ids::TurnId;
use crate::message::{AssistantMessage, ToolCall, ToolResultMessage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Graceful,
    Interrupted,
    Crashed,
}

/// Durable append-only session record.
///
/// These records are persisted, replayed, compacted, forked, and rewound. They
/// are not hook/lifecycle events; hooks should attach to a separate lifecycle
/// notification stream derived while the loop is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptRecord {
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

    /// Appended by the session layer between turns. The FSM never produces
    /// this variant. Different uses (compaction summary, branch summary,
    /// future spawn briefs / child reports / extension messages) are
    /// discriminated by `CustomMessage::kind` plus metadata. See
    /// `agent-session` for well-known kind constants and constructors.
    Custom(CustomMessage),
}

/// Payload carried by `TranscriptRecord::Custom`.
///
/// `kind` is a free-form tag chosen by the session (or an extension) to
/// classify the injection; `content` is the textual body surfaced to the
/// model; `metadata` carries anchor information (e.g. the id of the first
/// kept entry after a compaction, or the turn count the summary replaced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomMessage {
    pub kind: String,
    pub content: String,
    pub metadata: BTreeMap<String, String>,
}

impl CustomMessage {
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

impl TranscriptRecord {
    pub fn turn_id(&self) -> Option<TurnId> {
        match self {
            TranscriptRecord::TurnStarted { turn_id }
            | TranscriptRecord::ToolCallStarted { turn_id, .. }
            | TranscriptRecord::TurnFinished { turn_id, .. } => Some(*turn_id),
            TranscriptRecord::UserMessage(_)
            | TranscriptRecord::AssistantMessage(_)
            | TranscriptRecord::ToolResult(_)
            | TranscriptRecord::Custom(_) => None,
        }
    }
}
