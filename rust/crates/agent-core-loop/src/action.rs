use crate::ids::TurnId;
use crate::message::ToolCall;

/// Side effects requested by the core loop.
///
/// The orchestrator executes these and may wrap them with hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    RequestModel {
        turn_id: TurnId,
    },
    RequestTool {
        turn_id: TurnId,
        tool_call: ToolCall,
    },
    /// Cancel all active model/tool work for the turn.
    ///
    /// For parallel tool execution, the orchestrator should fan this out to
    /// every running tool handle associated with `turn_id`.
    CancelTurn {
        turn_id: TurnId,
    },
}
