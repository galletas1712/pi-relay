use crate::ids::TurnId;
use crate::message::{AssistantMessage, ToolCall, ToolResultMessage, UserInput};

/// Runtime input to the live agent FSM.
///
/// These events are volatile control inputs. Accepted transitions may append
/// durable TranscriptRecords, but AgentEvent itself is not persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentEvent {
    Interrupt,
    StartTurn {
        turn_id: TurnId,
        input: UserInput,
    },
    ModelCompleted {
        turn_id: TurnId,
        assistant: AssistantMessage,
    },
    ToolReady(ToolCall),
    ToolCompleted {
        turn_id: TurnId,
        result: ToolResultMessage,
    },
    ContinueModel,
}
