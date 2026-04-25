use agent_core::{ActionId, AgentAction, ToolCall, TurnId};

use crate::auto_compaction::StatelessModelRequest;

/// Session-level work requested by `AgentSession`.
///
/// Model/tool/turn-cancel actions are produced by `agent-core` and surfaced
/// here with the same correlation ids. Stateless model work is owned by the
/// session layer and bypasses the turn FSM while still flowing through the
/// same action/completion boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    RequestModel {
        action_id: ActionId,
        turn_id: TurnId,
    },
    RequestTool {
        action_id: ActionId,
        turn_id: TurnId,
        tool_call: ToolCall,
    },
    CancelTurn {
        turn_id: TurnId,
    },
    RequestModelStateless {
        request_id: StatelessModelRequestId,
        request: StatelessModelRequest,
    },
}

impl From<AgentAction> for SessionAction {
    fn from(action: AgentAction) -> Self {
        match action {
            AgentAction::RequestModel { action_id, turn_id } => {
                Self::RequestModel { action_id, turn_id }
            }
            AgentAction::RequestTool {
                action_id,
                turn_id,
                tool_call,
            } => Self::RequestTool {
                action_id,
                turn_id,
                tool_call,
            },
            AgentAction::CancelTurn { turn_id } => Self::CancelTurn { turn_id },
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StatelessModelRequestId(pub u64);

impl StatelessModelRequestId {
    pub fn first() -> Self {
        Self(1)
    }

    pub fn take_next(next: &mut Self) -> Self {
        let current = *next;
        next.0 += 1;
        current
    }
}
