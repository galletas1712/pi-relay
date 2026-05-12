use agent_vocab::{ActionId, ToolCall, TurnId};

/// Side effects requested by the core loop.
///
/// The caller executes these and may wrap them with hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// For parallel tool execution, the caller should fan this out to
    /// every running tool handle associated with `turn_id`.
    CancelTurn { turn_id: TurnId },
}
