use std::collections::VecDeque;

use agent_core::{ActionId, AgentAction, AgentInput, TurnId};

/// Tracks drained `RequestModel` / `RequestTool` actions the session is
/// waiting to hear back about, in FIFO insertion order.
///
/// `record_drained` pushes an entry for each `RequestModel` / `RequestTool` in
/// a drained action batch, and clears everything for a `CancelTurn`'s turn id.
/// `record_input` removes the matching key when a `ModelCompleted` /
/// `ToolCompleted` arrives; removal preserves the relative order of the
/// remaining entries. Stale completions (no matching key) are no-ops.
///
/// Duplicates are kept: if the same key is recorded twice, both are held in
/// FIFO position. Callers that care about "once per drain" should rely on
/// the drain semantics rather than this type deduplicating.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ActionQueue {
    entries: VecDeque<PendingActionKey>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct PendingActionKey {
    action_id: ActionId,
    turn_id: TurnId,
    kind: PendingActionKind,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum PendingActionKind {
    Model,
    Tool,
}

impl ActionQueue {
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
                AgentAction::RequestModel { action_id, turn_id } => {
                    self.entries.push_back(PendingActionKey {
                        action_id: *action_id,
                        turn_id: *turn_id,
                        kind: PendingActionKind::Model,
                    });
                }
                AgentAction::RequestTool {
                    action_id, turn_id, ..
                } => {
                    self.entries.push_back(PendingActionKey {
                        action_id: *action_id,
                        turn_id: *turn_id,
                        kind: PendingActionKind::Tool,
                    });
                }
                AgentAction::CancelTurn { turn_id } => {
                    self.entries.retain(|k| k.turn_id != *turn_id);
                }
            }
        }
    }

    pub(crate) fn record_input(&mut self, input: &AgentInput) -> bool {
        let target = match input {
            AgentInput::ModelCompleted {
                action_id, turn_id, ..
            }
            | AgentInput::ModelFailed {
                action_id, turn_id, ..
            } => Some(PendingActionKey {
                action_id: *action_id,
                turn_id: *turn_id,
                kind: PendingActionKind::Model,
            }),
            AgentInput::ToolCompleted {
                action_id, turn_id, ..
            } => Some(PendingActionKey {
                action_id: *action_id,
                turn_id: *turn_id,
                kind: PendingActionKind::Tool,
            }),
            _ => None,
        };
        let Some(target) = target else {
            return false;
        };
        if let Some(position) = self.entries.iter().position(|k| *k == target) {
            self.entries.remove(position);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ToolCall, ToolCallId, ToolResultMessage, ToolResultStatus};

    fn model_request(action: u64, turn: u64) -> AgentAction {
        AgentAction::RequestModel {
            action_id: ActionId(action),
            turn_id: TurnId(turn),
        }
    }

    fn tool_request(action: u64, turn: u64, id: u64) -> AgentAction {
        AgentAction::RequestTool {
            action_id: ActionId(action),
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
        let mut q = ActionQueue::new();
        q.record_drained(&[
            model_request(1, 1),
            tool_request(2, 1, 1),
            tool_request(3, 1, 2),
        ]);
        assert!(!q.is_empty());
    }

    #[test]
    fn record_input_removes_matching_completion() {
        let mut q = ActionQueue::new();
        q.record_drained(&[model_request(1, 1), tool_request(2, 1, 1)]);
        q.record_input(&AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: ToolCallId(1),
                tool_name: "bash".to_string(),
                output: "ok".to_string(),
                status: ToolResultStatus::Success,
            },
        });
        assert!(!q.is_empty()); // model still pending
        q.record_input(&AgentInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: agent_core::AssistantMessage { items: Vec::new() },
        });
        assert!(q.is_empty());
    }

    #[test]
    fn stale_completion_is_a_no_op() {
        let mut q = ActionQueue::new();
        q.record_input(&AgentInput::ModelCompleted {
            action_id: ActionId(99),
            turn_id: TurnId(99),
            assistant: agent_core::AssistantMessage { items: Vec::new() },
        });
        assert!(q.is_empty());
    }

    #[test]
    fn cancel_turn_clears_pending_actions_for_that_turn_only() {
        let mut q = ActionQueue::new();
        q.record_drained(&[
            model_request(1, 1),
            tool_request(2, 1, 1),
            model_request(3, 2),
        ]);
        q.record_drained(&[AgentAction::CancelTurn { turn_id: TurnId(1) }]);
        // Turn 2 survives.
        assert!(!q.is_empty());
        q.record_input(&AgentInput::ModelCompleted {
            action_id: ActionId(3),
            turn_id: TurnId(2),
            assistant: agent_core::AssistantMessage { items: Vec::new() },
        });
        assert!(q.is_empty());
    }

    #[test]
    fn clear_empties_the_queue() {
        let mut q = ActionQueue::new();
        q.record_drained(&[model_request(1, 1)]);
        q.clear();
        assert!(q.is_empty());
    }

    #[test]
    fn record_drained_preserves_insertion_order_across_duplicates() {
        let mut q = ActionQueue::new();
        q.record_drained(&[
            model_request(1, 1),
            model_request(1, 1),
            tool_request(2, 1, 1),
        ]);
        // Two model entries plus one tool entry; neither is deduplicated.
        assert_eq!(q.entries.len(), 3);
    }
}
