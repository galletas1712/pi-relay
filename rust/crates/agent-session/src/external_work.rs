use std::collections::VecDeque;

use agent_core::{
    ActionId, AgentAction, AgentInput, ToolCallId, TranscriptItem, TurnId, TurnOutcome,
};

use crate::action::SessionAction;
use crate::event::{SessionActionKind, SessionEvent};

/// Tracks turn-scoped model/tool work that has crossed the session boundary.
///
/// This hides two bits of bookkeeping from `AgentSession`:
/// - unresolved work the harness may still be running;
/// - completion events that should only be emitted after the core accepts them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ExternalWork {
    unresolved_actions: VecDeque<UnresolvedTurnAction>,
    deferred_completion_events: VecDeque<DeferredCompletionEvent>,
}

impl ExternalWork {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.unresolved_actions.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.unresolved_actions.clear();
        self.deferred_completion_events.clear();
    }

    pub(crate) fn record_dispatched(&mut self, action: &AgentAction) {
        if let Some(action) = UnresolvedTurnAction::requested_by_core(action) {
            self.unresolved_actions.push_back(action);
        }
    }

    pub(crate) fn record_completion(&mut self, input: &AgentInput) -> bool {
        let Some(resolved) = UnresolvedTurnAction::resolved_by(input) else {
            return false;
        };
        let Some(position) = self
            .unresolved_actions
            .iter()
            .position(|pending| *pending == resolved)
        else {
            return false;
        };

        self.unresolved_actions.remove(position);
        if let Some(event) = DeferredCompletionEvent::for_completion(input) {
            self.deferred_completion_events.push_back(event);
        }
        true
    }

    pub(crate) fn action_matches_completion(action: &SessionAction, input: &AgentInput) -> bool {
        UnresolvedTurnAction::requested_by_session(action)
            .zip(UnresolvedTurnAction::resolved_by(input))
            .is_some_and(|(requested, resolved)| requested == resolved)
    }

    pub(crate) fn emit_events_after_core_accepts(
        &mut self,
        items: &[TranscriptItem],
        event_outbox: &mut VecDeque<SessionEvent>,
    ) {
        let mut still_deferred = VecDeque::new();
        while let Some(event) = self.deferred_completion_events.pop_front() {
            if event.accepted_by(items) {
                event.push_session_event(event_outbox);
            } else if event.should_wait_for_more_items(items, &self.unresolved_actions) {
                still_deferred.push_back(event);
            }
        }
        self.deferred_completion_events = still_deferred;
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum UnresolvedTurnAction {
    Model {
        action_id: ActionId,
        turn_id: TurnId,
    },
    Tool {
        action_id: ActionId,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        tool_name: String,
    },
}

impl UnresolvedTurnAction {
    fn requested_by_core(action: &AgentAction) -> Option<Self> {
        match action {
            AgentAction::RequestModel { action_id, turn_id } => {
                Some(Self::model(*action_id, *turn_id))
            }
            AgentAction::RequestTool {
                action_id,
                turn_id,
                tool_call,
            } => Some(Self::tool(
                *action_id,
                *turn_id,
                tool_call.id,
                tool_call.tool_name.clone(),
            )),
            AgentAction::CancelTurn { .. } => None,
        }
    }

    fn requested_by_session(action: &SessionAction) -> Option<Self> {
        match action {
            SessionAction::RequestModel {
                action_id, turn_id, ..
            } => Some(Self::model(*action_id, *turn_id)),
            SessionAction::RequestTool {
                action_id,
                turn_id,
                tool_call,
            } => Some(Self::tool(
                *action_id,
                *turn_id,
                tool_call.id,
                tool_call.tool_name.clone(),
            )),
            SessionAction::CancelSessionWork | SessionAction::RequestCompaction { .. } => None,
        }
    }

    fn resolved_by(input: &AgentInput) -> Option<Self> {
        match input {
            AgentInput::ModelCompleted {
                action_id, turn_id, ..
            }
            | AgentInput::ModelFailed {
                action_id, turn_id, ..
            } => Some(Self::model(*action_id, *turn_id)),
            AgentInput::ToolCompleted {
                action_id,
                turn_id,
                result,
            } => Some(Self::tool(
                *action_id,
                *turn_id,
                result.tool_call_id,
                result.tool_name.clone(),
            )),
            AgentInput::Interrupt | AgentInput::Steer { .. } | AgentInput::FollowUp { .. } => None,
        }
    }

    fn model(action_id: ActionId, turn_id: TurnId) -> Self {
        Self::Model { action_id, turn_id }
    }

    fn tool(
        action_id: ActionId,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        tool_name: String,
    ) -> Self {
        Self::Tool {
            action_id,
            turn_id,
            tool_call_id,
            tool_name,
        }
    }

    fn turn_id(&self) -> TurnId {
        match self {
            Self::Model { turn_id, .. } | Self::Tool { turn_id, .. } => *turn_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeferredCompletionEvent {
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

impl DeferredCompletionEvent {
    fn for_completion(input: &AgentInput) -> Option<Self> {
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
                tool_call_id: result.tool_call_id,
                tool_name: result.tool_name.clone(),
            }),
            AgentInput::Interrupt | AgentInput::Steer { .. } | AgentInput::FollowUp { .. } => None,
        }
    }

    fn accepted_by(&self, items: &[TranscriptItem]) -> bool {
        match self {
            Self::ModelCompleted { turn_id, .. } => model_completed_in_items(items, *turn_id),
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
        unresolved_actions: &VecDeque<UnresolvedTurnAction>,
    ) -> bool {
        match self {
            Self::ToolCompleted { turn_id, .. } => {
                !items.iter().any(|item| {
                    matches!(item, TranscriptItem::TurnFinished { turn_id: item_turn_id, .. } if item_turn_id == turn_id)
                }) && unresolved_actions.iter().any(|action| action.turn_id() == *turn_id)
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

fn model_completed_in_items(items: &[TranscriptItem], turn_id: TurnId) -> bool {
    let Some(assistant_index) = items
        .iter()
        .position(|item| matches!(item, TranscriptItem::AssistantMessage(_)))
    else {
        return false;
    };

    items[assistant_index + 1..]
        .iter()
        .find_map(TranscriptItem::turn_id)
        == Some(turn_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ToolCall, ToolResultMessage, ToolResultStatus};

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
    fn record_dispatched_inserts_model_and_tool_actions() {
        let mut work = ExternalWork::new();
        for action in [
            model_request(1, 1),
            tool_request(2, 1, 1),
            tool_request(3, 1, 2),
        ] {
            work.record_dispatched(&action);
        }
        assert!(!work.is_empty());
    }

    #[test]
    fn record_completion_removes_matching_action() {
        let mut work = ExternalWork::new();
        for action in [model_request(1, 1), tool_request(2, 1, 1)] {
            work.record_dispatched(&action);
        }
        work.record_completion(&AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: ToolCallId(1),
                tool_name: "bash".to_string(),
                output: "ok".to_string(),
                status: ToolResultStatus::Success,
            },
        });
        assert!(!work.is_empty()); // model still pending
        work.record_completion(&AgentInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: agent_core::AssistantMessage { items: Vec::new() },
        });
        assert!(work.is_empty());
    }

    #[test]
    fn tool_completion_must_match_tool_identity() {
        let mut work = ExternalWork::new();
        work.record_dispatched(&tool_request(2, 1, 1));
        assert!(!work.record_completion(&AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: ToolCallId(99),
                tool_name: "bash".to_string(),
                output: "wrong call".to_string(),
                status: ToolResultStatus::Success,
            },
        }));
        assert!(!work.record_completion(&AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: ToolCallId(1),
                tool_name: "other".to_string(),
                output: "wrong tool".to_string(),
                status: ToolResultStatus::Success,
            },
        }));
        assert!(!work.is_empty());
    }

    #[test]
    fn stale_completion_is_a_no_op() {
        let mut work = ExternalWork::new();
        work.record_completion(&AgentInput::ModelCompleted {
            action_id: ActionId(99),
            turn_id: TurnId(99),
            assistant: agent_core::AssistantMessage { items: Vec::new() },
        });
        assert!(work.is_empty());
    }

    #[test]
    fn clear_empties_external_work() {
        let mut work = ExternalWork::new();
        work.record_dispatched(&model_request(1, 1));
        work.clear();
        assert!(work.is_empty());
    }
}
