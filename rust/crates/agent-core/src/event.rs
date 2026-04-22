use crate::ids::TurnId;
use crate::message::{AssistantMessage, ToolResultMessage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentInput {
    // User asked to stop the active model/tool work.
    Interrupt,
    // High-priority user input. Runs before queued follow-up work.
    Steer(String),
    // Normal-priority user input for the next available turn.
    FollowUp(String),
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
