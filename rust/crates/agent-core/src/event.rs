use crate::ids::TurnId;
use crate::message::{AssistantMessage, ToolResultMessage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentInput {
    // User asked to stop the active model/tool work.
    Interrupt,
    // High-priority input. Runs before queued follow-up work.
    //
    // `from = None` means the input came from the human user (or unknown
    // origin — same thing at the core layer). `from = Some(session_id)` means
    // the input came from another session (e.g. a parent directive). The
    // core crate is oblivious to what a session id *is* — it's just a tag
    // that rides along.
    Steer {
        from: Option<String>,
        content: String,
    },
    // Normal-priority input for the next available turn.
    //
    // Same `from` semantics as `Steer`. A child report arrives to its parent
    // as `FollowUp { from: Some(child_id), .. }`.
    FollowUp {
        from: Option<String>,
        content: String,
    },
    // Volatile model completion delivered by the orchestrator.
    ModelCompleted {
        turn_id: TurnId,
        assistant: AssistantMessage,
    },
    // Volatile tool completion delivered by the orchestrator.
    ToolCompleted {
        turn_id: TurnId,
        result: ToolResultMessage,
    },
}

impl AgentInput {
    /// Steer input from the human user (or unknown origin).
    pub fn steer(content: impl Into<String>) -> Self {
        Self::Steer {
            from: None,
            content: content.into(),
        }
    }

    /// Follow-up input from the human user (or unknown origin).
    pub fn follow_up(content: impl Into<String>) -> Self {
        Self::FollowUp {
            from: None,
            content: content.into(),
        }
    }

    /// Steer input tagged as coming from the given session.
    pub fn steer_from(from: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Steer {
            from: Some(from.into()),
            content: content.into(),
        }
    }

    /// Follow-up input tagged as coming from the given session.
    pub fn follow_up_from(from: impl Into<String>, content: impl Into<String>) -> Self {
        Self::FollowUp {
            from: Some(from.into()),
            content: content.into(),
        }
    }
}

/// Runtime input to the live agent FSM.
///
/// These events are volatile control inputs. Accepted transitions may append
/// durable TranscriptRecords, but AgentEvent itself is not persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentEvent {
    Interrupt,
    StartTurn {
        turn_id: TurnId,
        input: String,
    },
    ModelCompleted {
        turn_id: TurnId,
        assistant: AssistantMessage,
    },
    ToolCompleted {
        turn_id: TurnId,
        result: ToolResultMessage,
    },
    ContinueModel,
}
