use std::collections::VecDeque;

use agent_core::{AgentAction, AgentInput};
use agent_vocab::{ActionId, ToolCallId, TranscriptItem, TurnId, TurnOutcome};

use crate::action::CompletionTarget;
use crate::event::{SessionActionKind, SessionEvent};

/// Tracks model/tool actions that have left the session but have not been
/// accepted back into the transcript.
///
/// The core may be idle after requesting model/tool work. This ledger keeps the
/// session from accepting stale completions after interrupts/history edits and
/// delays completion events until the transcript proves the core accepted them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct OutstandingActions {
    pending: VecDeque<CompletionTarget>,
    // Completion events are only meaningful while `pending` describes the same
    // live work. `clear` must always empty both queues together.
    completion_events_pending_transcript: VecDeque<RecordedCompletion>,
}

impl OutstandingActions {
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
        self.completion_events_pending_transcript.clear();
    }

    pub(crate) fn track_request(&mut self, action: &AgentAction) {
        if let Some(action) = CompletionTarget::from_core_action(action) {
            self.pending.push_back(action);
        }
    }

    pub(crate) fn accept_completion(&mut self, input: &AgentInput) -> bool {
        let Some(completion) = RecordedCompletion::from_input(input) else {
            return false;
        };
        let target = completion.target();
        let Some(position) = self.pending.iter().position(|action| action == &target) else {
            return false;
        };

        self.pending.remove(position);
        self.completion_events_pending_transcript
            .push_back(completion);
        true
    }

    pub(crate) fn emit_events_after_core_accepts(
        &mut self,
        items: &[TranscriptItem],
        event_outbox: &mut VecDeque<SessionEvent>,
    ) {
        let mut still_waiting = VecDeque::new();
        while let Some(completion) = self.completion_events_pending_transcript.pop_front() {
            if completion.accepted_by(items) {
                completion.push_session_event(event_outbox);
            } else if completion.should_wait_for_more_items(items, &self.pending) {
                still_waiting.push_back(completion);
            }
        }
        self.completion_events_pending_transcript = still_waiting;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordedCompletion {
    ModelCompleted {
        action_id: ActionId,
        turn_id: TurnId,
    },
    ModelFailed {
        action_id: ActionId,
        turn_id: TurnId,
        error: String,
    },
    ToolCompleted {
        action_id: ActionId,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        tool_name: String,
    },
}

impl RecordedCompletion {
    fn from_input(input: &AgentInput) -> Option<Self> {
        match input {
            AgentInput::ModelCompleted {
                action_id, turn_id, ..
            } => Some(Self::ModelCompleted {
                action_id: *action_id,
                turn_id: *turn_id,
            }),
            AgentInput::ModelFailed {
                action_id,
                turn_id,
                error,
            } => Some(Self::ModelFailed {
                action_id: *action_id,
                turn_id: *turn_id,
                error: error.clone(),
            }),
            AgentInput::ToolCompleted {
                action_id,
                turn_id,
                result,
            } => Some(Self::ToolCompleted {
                action_id: *action_id,
                turn_id: *turn_id,
                tool_call_id: result.tool_call_id.clone(),
                tool_name: result.tool_name.clone(),
            }),
            AgentInput::Interrupt | AgentInput::Steer { .. } | AgentInput::FollowUp { .. } => None,
        }
    }

    fn target(&self) -> CompletionTarget {
        match self {
            Self::ModelCompleted { action_id, turn_id }
            | Self::ModelFailed {
                action_id, turn_id, ..
            } => CompletionTarget {
                action_id: *action_id,
                turn_id: *turn_id,
                tool: None,
            },
            Self::ToolCompleted {
                action_id,
                turn_id,
                tool_call_id,
                tool_name,
            } => CompletionTarget {
                action_id: *action_id,
                turn_id: *turn_id,
                tool: Some(crate::action::CompletionToolTarget {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                }),
            },
        }
    }

    fn accepted_by(&self, items: &[TranscriptItem]) -> bool {
        match self {
            Self::ModelCompleted { turn_id, .. } => {
                let Some(assistant_index) = items
                    .iter()
                    .position(|item| matches!(item, TranscriptItem::AssistantMessage(_)))
                else {
                    return false;
                };

                items[assistant_index + 1..]
                    .iter()
                    .find_map(TranscriptItem::turn_id)
                    == Some(*turn_id)
            }
            Self::ModelFailed { turn_id, .. } => items.iter().any(|item| {
                matches!(
                    item,
                    TranscriptItem::TurnFinished {
                        turn_id: item_turn_id,
                        outcome: TurnOutcome::Crashed,
                    } if item_turn_id == turn_id
                )
            }),
            Self::ToolCompleted {
                tool_call_id,
                tool_name,
                ..
            } => items.iter().any(|item| {
                matches!(
                    item,
                    TranscriptItem::ToolResult(result)
                        if result.tool_call_id == *tool_call_id
                            && result.tool_name == *tool_name
                )
            }),
        }
    }

    fn should_wait_for_more_items(
        &self,
        items: &[TranscriptItem],
        pending: &VecDeque<CompletionTarget>,
    ) -> bool {
        match self {
            Self::ToolCompleted { turn_id, .. } => {
                !items.iter().any(|item| {
                    matches!(item, TranscriptItem::TurnFinished { turn_id: item_turn_id, .. } if item_turn_id == turn_id)
                }) && pending.iter().any(|action| action.turn_id == *turn_id)
            }
            Self::ModelCompleted { .. } | Self::ModelFailed { .. } => false,
        }
    }

    fn push_session_event(self, event_outbox: &mut VecDeque<SessionEvent>) {
        match self {
            Self::ModelCompleted { action_id, .. } => {
                event_outbox.push_back(SessionEvent::ActionCompleted {
                    kind: SessionActionKind::Model,
                    id: action_id.0.to_string(),
                });
            }
            Self::ModelFailed {
                action_id, error, ..
            } => {
                event_outbox.push_back(SessionEvent::ActionFailed {
                    kind: SessionActionKind::Model,
                    id: action_id.0.to_string(),
                    error,
                });
            }
            Self::ToolCompleted { action_id, .. } => {
                event_outbox.push_back(SessionEvent::ActionCompleted {
                    kind: SessionActionKind::Tool,
                    id: action_id.0.to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_vocab::{
        AssistantMessage, ToolCall, ToolCallId, ToolResultMessage, ToolResultStatus,
    };

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
                id: ToolCallId::from_u64(id),
                tool_name: "bash".to_string(),
                args_json: "{}".to_string(),
            },
        }
    }

    #[test]
    fn track_request_inserts_model_and_tool_actions() {
        let mut actions = OutstandingActions::default();
        for action in [
            model_request(1, 1),
            tool_request(2, 1, 1),
            tool_request(3, 1, 2),
        ] {
            actions.track_request(&action);
        }
        assert!(!actions.is_empty());
    }

    #[test]
    fn accept_completion_removes_matching_action() {
        let mut actions = OutstandingActions::default();
        for action in [model_request(1, 1), tool_request(2, 1, 1)] {
            actions.track_request(&action);
        }
        actions.accept_completion(&AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: ToolCallId::from_u64(1),
                tool_name: "bash".to_string(),
                output: "ok".to_string(),
                status: ToolResultStatus::Success,
            },
        });
        assert!(!actions.is_empty()); // model still pending
        actions.accept_completion(&AgentInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        });
        assert!(actions.is_empty());
    }

    #[test]
    fn tool_completion_must_match_tool_identity() {
        let mut actions = OutstandingActions::default();
        actions.track_request(&tool_request(2, 1, 1));
        assert!(!actions.accept_completion(&AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: ToolCallId::from_u64(99),
                tool_name: "bash".to_string(),
                output: "wrong call".to_string(),
                status: ToolResultStatus::Success,
            },
        }));
        assert!(!actions.accept_completion(&AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: ToolCallId::from_u64(1),
                tool_name: "other".to_string(),
                output: "wrong tool".to_string(),
                status: ToolResultStatus::Success,
            },
        }));
        assert!(!actions.is_empty());
    }

    #[test]
    fn stale_completion_is_a_no_op() {
        let mut actions = OutstandingActions::default();
        actions.accept_completion(&AgentInput::ModelCompleted {
            action_id: ActionId(99),
            turn_id: TurnId(99),
            assistant: AssistantMessage { items: Vec::new() },
        });
        assert!(actions.is_empty());
    }

    #[test]
    fn clear_empties_outstanding_actions() {
        let mut actions = OutstandingActions::default();
        actions.track_request(&model_request(1, 1));
        actions.clear();
        assert!(actions.is_empty());
    }
}
