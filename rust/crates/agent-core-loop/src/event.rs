use crate::ids::TurnId;
use crate::message::{AssistantMessage, ToolCall, ToolResultMessage, UserMessage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Graceful,
    Interrupted,
    Crashed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
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
