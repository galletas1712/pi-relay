use std::fmt;

use crate::ids::{ActionId, TurnId};
use crate::message::{AssistantMessage, ToolResultMessage};

/// External input to the live agent FSM.
///
/// **Invariant (for `Steer` and `FollowUp`):** `from.is_some() == kind.is_some()`.
/// Either both `None` (input came from the human user or unknown origin —
/// same thing at the core layer) or both `Some` (input was injected by another
/// session, e.g. a parent directive or a child report). The core crate is
/// oblivious to what a session id *is*, and to what specific `kind` strings
/// mean — both are opaque tags that ride along with the content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentInput {
    // User asked to stop the active model/tool work.
    Interrupt,
    // High-priority input. Runs before queued follow-up work.
    Steer {
        from: Option<String>,
        kind: Option<String>,
        content: String,
    },
    // Normal-priority input for the next available turn.
    FollowUp {
        from: Option<String>,
        kind: Option<String>,
        content: String,
    },
    // Volatile model completion delivered by the orchestrator.
    ModelCompleted {
        action_id: ActionId,
        turn_id: TurnId,
        assistant: AssistantMessage,
    },
    // Volatile model failure delivered by the orchestrator.
    ModelFailed {
        action_id: ActionId,
        turn_id: TurnId,
        error: String,
    },
    // Volatile tool completion delivered by the orchestrator.
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
            from: None,
            kind: None,
            content: content.into(),
        }
    }

    /// Follow-up input from the human user (or unknown origin).
    pub fn follow_up(content: impl Into<String>) -> Self {
        Self::FollowUp {
            from: None,
            kind: None,
            content: content.into(),
        }
    }

    /// Steer input tagged as coming from the given session with the given kind.
    ///
    /// Used for agent-routed injections (e.g. parent directives); see
    /// `agent-orchestrator` for the well-known kind constants.
    pub fn steer_tagged(
        from: impl Into<String>,
        kind: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self::Steer {
            from: Some(from.into()),
            kind: Some(kind.into()),
            content: content.into(),
        }
    }

    /// Follow-up input tagged as coming from the given session with the given
    /// kind.
    ///
    /// Used for agent-routed injections (e.g. child reports); see
    /// `agent-orchestrator` for the well-known kind constants.
    pub fn follow_up_tagged(
        from: impl Into<String>,
        kind: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self::FollowUp {
            from: Some(from.into()),
            kind: Some(kind.into()),
            content: content.into(),
        }
    }

    pub fn validate(&self) -> Result<(), AgentInputError> {
        match self {
            AgentInput::Steer { from, kind, .. } | AgentInput::FollowUp { from, kind, .. } => {
                if from.is_some() == kind.is_some() {
                    Ok(())
                } else {
                    Err(AgentInputError::UnpairedOriginTags)
                }
            }
            AgentInput::Interrupt
            | AgentInput::ModelCompleted { .. }
            | AgentInput::ModelFailed { .. }
            | AgentInput::ToolCompleted { .. } => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentInputError {
    UnpairedOriginTags,
}

impl fmt::Display for AgentInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnpairedOriginTags => write!(f, "from and kind tags must be paired"),
        }
    }
}

impl std::error::Error for AgentInputError {}

/// Runtime input to the live agent FSM.
///
/// These events are volatile control inputs. Accepted transitions may append
/// durable transcript items, but AgentEvent itself is not persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentEvent {
    Interrupt,
    StartTurn {
        turn_id: TurnId,
        input: String,
        origin: Option<TurnOrigin>,
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

/// Origin tag attached to a turn that was started from an agent-routed input
/// (e.g. a parent directive or a child report) rather than from a human user.
///
/// Present iff the originating `AgentInput::Steer`/`FollowUp` carried both a
/// `from` and a `kind` — the paired-invariant enforced on `AgentInput`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnOrigin {
    pub from: String,
    pub kind: String,
}
