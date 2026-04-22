use crate::ids::{EventId, TurnId};
use crate::message::{ToolCall, ToolResultStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    TurnStarted {
        turn_id: TurnId,
    },
    ToolCallStarted {
        turn_id: TurnId,
        tool_call_id: EventId,
        tool_name: String,
    },
    ToolCallFinished {
        turn_id: TurnId,
        tool_call_id: EventId,
        tool_name: String,
        status: ToolResultStatus,
    },
    Interrupted {
        turn_id: TurnId,
    },
    TurnFinished {
        turn_id: TurnId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    RequestModel {
        turn_id: TurnId,
    },
    RequestTool {
        turn_id: TurnId,
        tool_call: ToolCall,
    },
    CancelActive {
        turn_id: TurnId,
    },
}
