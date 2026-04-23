use std::collections::HashSet;

use agent_core::{AgentAction, AgentInput, ToolCallId, TurnId};

/// Tracks drained `RequestModel` / `RequestTool` actions the session is
/// waiting to hear back about.
///
/// `record_drained` adds inserts for `RequestModel` / `RequestTool` in a
/// drained action batch, and clears everything for a `CancelTurn`'s turn id.
/// `record_input` removes the matching key when a `ModelCompleted` /
/// `ToolCompleted` arrives. Stale completions (no matching key) are no-ops.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PendingActions {
    entries: HashSet<PendingActionKey>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct PendingActionKey {
    turn_id: TurnId,
    kind: PendingActionKind,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum PendingActionKind {
    Model,
    Tool { tool_call_id: ToolCallId },
}

impl PendingActions {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn record_drained(&mut self, actions: &[AgentAction]) {
        for action in actions {
            match action {
                AgentAction::RequestModel { turn_id } => {
                    self.entries.insert(PendingActionKey {
                        turn_id: *turn_id,
                        kind: PendingActionKind::Model,
                    });
                }
                AgentAction::RequestTool { turn_id, tool_call } => {
                    self.entries.insert(PendingActionKey {
                        turn_id: *turn_id,
                        kind: PendingActionKind::Tool {
                            tool_call_id: tool_call.id,
                        },
                    });
                }
                AgentAction::CancelTurn { turn_id } => {
                    self.entries.retain(|k| k.turn_id != *turn_id);
                }
            }
        }
    }

    pub(crate) fn record_input(&mut self, input: &AgentInput) {
        match input {
            AgentInput::ModelCompleted { turn_id, .. } => {
                self.entries.remove(&PendingActionKey {
                    turn_id: *turn_id,
                    kind: PendingActionKind::Model,
                });
            }
            AgentInput::ToolCompleted { turn_id, result } => {
                self.entries.remove(&PendingActionKey {
                    turn_id: *turn_id,
                    kind: PendingActionKind::Tool {
                        tool_call_id: result.tool_call_id,
                    },
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ToolCall, ToolResultMessage, ToolResultStatus};

    fn model_request(turn: u64) -> AgentAction {
        AgentAction::RequestModel {
            turn_id: TurnId(turn),
        }
    }

    fn tool_request(turn: u64, id: u64) -> AgentAction {
        AgentAction::RequestTool {
            turn_id: TurnId(turn),
            tool_call: ToolCall {
                id: ToolCallId(id),
                tool_name: "bash".to_string(),
                args_json: "{}".to_string(),
            },
        }
    }

    #[test]
    fn record_drained_inserts_model_and_tool_actions() {
        let mut pa = PendingActions::new();
        pa.record_drained(&[model_request(1), tool_request(1, 1), tool_request(1, 2)]);
        assert!(!pa.is_empty());
    }

    #[test]
    fn record_input_removes_matching_completion() {
        let mut pa = PendingActions::new();
        pa.record_drained(&[model_request(1), tool_request(1, 1)]);
        pa.record_input(&AgentInput::ToolCompleted {
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: ToolCallId(1),
                tool_name: "bash".to_string(),
                output: "ok".to_string(),
                status: ToolResultStatus::Success,
            },
        });
        assert!(!pa.is_empty()); // model still pending
        pa.record_input(&AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: agent_core::AssistantMessage { items: Vec::new() },
        });
        assert!(pa.is_empty());
    }

    #[test]
    fn stale_completion_is_a_no_op() {
        let mut pa = PendingActions::new();
        pa.record_input(&AgentInput::ModelCompleted {
            turn_id: TurnId(99),
            assistant: agent_core::AssistantMessage { items: Vec::new() },
        });
        assert!(pa.is_empty());
    }

    #[test]
    fn cancel_turn_clears_pending_actions_for_that_turn_only() {
        let mut pa = PendingActions::new();
        pa.record_drained(&[model_request(1), tool_request(1, 1), model_request(2)]);
        pa.record_drained(&[AgentAction::CancelTurn { turn_id: TurnId(1) }]);
        // Turn 2 survives.
        assert!(!pa.is_empty());
        pa.record_input(&AgentInput::ModelCompleted {
            turn_id: TurnId(2),
            assistant: agent_core::AssistantMessage { items: Vec::new() },
        });
        assert!(pa.is_empty());
    }

    #[test]
    fn clear_empties_the_set() {
        let mut pa = PendingActions::new();
        pa.record_drained(&[model_request(1)]);
        pa.clear();
        assert!(pa.is_empty());
    }
}
