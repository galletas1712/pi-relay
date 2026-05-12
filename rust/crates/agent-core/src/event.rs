use agent_vocab::{
    ActionId, AssistantMessage, ToolResultMessage, TranscriptItem, TurnId, UserMessage,
};

/// First transcript record for a new turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnInput(pub UserMessage);

impl TurnInput {
    pub(crate) fn into_transcript_item(self) -> TranscriptItem {
        TranscriptItem::UserMessage(self.0)
    }
}

/// External input to the live agent FSM.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentInput {
    // User asked to stop the active model/tool work.
    Interrupt,
    // High-priority input. Runs before queued follow-up work.
    Steer {
        content: TurnInput,
    },
    // Normal-priority input for the next available turn.
    FollowUp {
        content: TurnInput,
    },
    // Volatile model completion delivered by the caller.
    ModelCompleted {
        action_id: ActionId,
        turn_id: TurnId,
        assistant: AssistantMessage,
    },
    // Volatile model failure delivered by the caller.
    ModelFailed {
        action_id: ActionId,
        turn_id: TurnId,
        error: String,
    },
    // Volatile tool completion delivered by the caller.
    ToolCompleted {
        action_id: ActionId,
        turn_id: TurnId,
        result: ToolResultMessage,
    },
}

impl AgentInput {
    /// Steer input from the human user (or unknown origin).
    pub fn steer(content: impl Into<String>) -> Self {
        Self::Steer {
            content: TurnInput(UserMessage::text(content)),
        }
    }

    /// Steer input with structured user content, including images.
    pub fn steer_message(content: UserMessage) -> Self {
        Self::Steer {
            content: TurnInput(content),
        }
    }

    /// Follow-up input from the human user (or unknown origin).
    pub fn follow_up(content: impl Into<String>) -> Self {
        Self::FollowUp {
            content: TurnInput(UserMessage::text(content)),
        }
    }

    /// Follow-up input with structured user content, including images.
    pub fn follow_up_message(content: UserMessage) -> Self {
        Self::FollowUp {
            content: TurnInput(content),
        }
    }
}

/// Runtime input to the live agent FSM.
///
/// These events are volatile control inputs. Accepted transitions may append
/// durable transcript items, but AgentEvent itself is not persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentEvent {
    Interrupt,
    StartTurn {
        turn_id: TurnId,
        input: TurnInput,
    },
    Steer {
        input: TurnInput,
    },
    ModelCompleted {
        action_id: ActionId,
        turn_id: TurnId,
        assistant: AssistantMessage,
    },
    ModelFailed {
        action_id: ActionId,
        turn_id: TurnId,
        error: String,
    },
    ToolCompleted {
        action_id: ActionId,
        turn_id: TurnId,
        result: ToolResultMessage,
    },
    ContinueModel,
}
