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

impl AgentEvent {
    pub fn turn_id(&self) -> Option<TurnId> {
        match self {
            AgentEvent::TurnStarted { turn_id }
            | AgentEvent::ToolCallStarted { turn_id, .. }
            | AgentEvent::TurnFinished { turn_id, .. } => Some(*turn_id),
            AgentEvent::UserMessage(_)
            | AgentEvent::AssistantMessage(_)
            | AgentEvent::ToolResult(_) => None,
        }
    }
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
