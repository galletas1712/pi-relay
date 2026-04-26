use crate::ids::{ActionId, TurnId};
use crate::message::ToolCall;
use serde::{Deserialize, Serialize};

/// Side effects requested by the core loop.
///
/// The orchestrator executes these and may wrap them with hooks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum AgentAction {
    RequestModel {
        action_id: ActionId,
        turn_id: TurnId,
    },
    RequestTool {
        action_id: ActionId,
        turn_id: TurnId,
        tool_call: ToolCall,
    },
    /// Cancel all active model/tool work for the turn.
    ///
    /// For parallel tool execution, the orchestrator should fan this out to
    /// every running tool handle associated with `turn_id`.
    CancelTurn { turn_id: TurnId },
}
