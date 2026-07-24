use std::collections::{HashMap, VecDeque};

use agent_core::{AgentAction, AgentInput};
use agent_vocab::{ActionId, ToolCallId, TranscriptItem, TurnId, TurnOutcome};

use crate::action::{CompletionTarget, SessionAction};
use crate::event::{SessionActionKind, SessionEvent};

/// Tracks model/tool actions that have left the session but have not been
/// accepted back into the transcript.
///
/// The core may be idle after requesting model/tool work. This ledger keeps the
/// session from accepting stale completions after interrupts/history edits and
/// delays completion events until the transcript proves the core accepted them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct OutstandingActions {
    // Every ID appears here exactly once, in provider source order, until its
    // accepted completion is released or proven stale by the transcript.
    source_order: VecDeque<ActionId>,
    // IDs in `source_order` are partitioned between this map and
    // `accepted_by_id`.
    pending_by_id: HashMap<ActionId, CompletionTarget>,
    pending_by_turn: HashMap<TurnId, usize>,
    // Completion events are only meaningful while the other fields describe
    // the same live work. `clear` must always empty the entire ledger.
    accepted_by_id: HashMap<ActionId, RecordedCompletion>,
}

impl OutstandingActions {
    pub(crate) fn is_empty(&self) -> bool {
        self.pending_by_id.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.source_order.clear();
        self.pending_by_id.clear();
        self.pending_by_turn.clear();
        self.accepted_by_id.clear();
    }

    pub(crate) fn track_request(&mut self, action: &AgentAction) {
        if let Some(action) = CompletionTarget::from_core_action(action) {
            self.track(action);
        }
    }

    pub(crate) fn track_session_action(&mut self, action: &SessionAction) {
        if let Some(action) = CompletionTarget::from_session_action(action) {
            self.track(action);
        }
    }

    pub(crate) fn accept_completion(&mut self, input: &AgentInput) -> bool {
        let Some(completion) = RecordedCompletion::from_input(input) else {
            return false;
        };
        let target = completion.target();
        #[cfg(test)]
        count_completion_operation();
        if self.pending_by_id.get(&target.action_id) != Some(&target) {
            return false;
        }

        self.pending_by_id.remove(&target.action_id);
        let pending_for_turn = self
            .pending_by_turn
            .get_mut(&target.turn_id)
            .expect("tracked target must have a pending turn count");
        *pending_for_turn -= 1;
        if *pending_for_turn == 0 {
            self.pending_by_turn.remove(&target.turn_id);
        }
        self.accepted_by_id.insert(target.action_id, completion);
        true
    }

    pub(crate) fn emit_events_after_core_accepts(
        &mut self,
        items: &[TranscriptItem],
        event_outbox: &mut VecDeque<SessionEvent>,
    ) {
        let mut accepted_items = AcceptedTranscriptItems::from_items(items);
        while let Some(action_id) = self.source_order.front() {
            #[cfg(test)]
            count_completion_operation();
            let Some(completion) = self.accepted_by_id.get(action_id) else {
                break;
            };
            if completion.take_accepted_item(&mut accepted_items) {
                let action_id = self
                    .source_order
                    .pop_front()
                    .expect("source-order front must exist");
                let completion = self
                    .accepted_by_id
                    .remove(&action_id)
                    .expect("accepted completion must exist");
                completion.push_session_event(event_outbox);
            } else if completion
                .should_wait_for_more_items(&accepted_items.finished_turns, &self.pending_by_turn)
            {
                break;
            } else {
                let action_id = self
                    .source_order
                    .pop_front()
                    .expect("source-order front must exist");
                self.accepted_by_id.remove(&action_id);
            }
        }
    }

    fn track(&mut self, target: CompletionTarget) {
        let action_id = target.action_id;
        let turn_id = target.turn_id;
        assert!(
            !self.pending_by_id.contains_key(&action_id)
                && !self.accepted_by_id.contains_key(&action_id),
            "duplicate outstanding action id {}",
            action_id.0
        );
        self.pending_by_id.insert(action_id, target);
        self.source_order.push_back(action_id);
        *self.pending_by_turn.entry(turn_id).or_default() += 1;
    }
}

#[derive(Default)]
struct AcceptedTranscriptItems {
    model_completed_turns: HashMap<TurnId, usize>,
    model_failed_turns: HashMap<TurnId, usize>,
    tool_results: HashMap<(ToolCallId, String), usize>,
    finished_turns: HashMap<TurnId, usize>,
}

impl AcceptedTranscriptItems {
    fn from_items(items: &[TranscriptItem]) -> Self {
        let mut accepted = Self::default();
        let mut saw_assistant = false;
        for item in items {
            #[cfg(test)]
            count_completion_operation();
            match item {
                TranscriptItem::AssistantMessage(_) => saw_assistant = true,
                TranscriptItem::ToolResult(result) => {
                    *accepted
                        .tool_results
                        .entry((result.tool_call_id.clone(), result.tool_name.clone()))
                        .or_default() += 1;
                }
                TranscriptItem::TurnFinished { turn_id, outcome } => {
                    *accepted.finished_turns.entry(*turn_id).or_default() += 1;
                    if *outcome == TurnOutcome::Crashed {
                        *accepted.model_failed_turns.entry(*turn_id).or_default() += 1;
                    }
                }
                TranscriptItem::TurnStarted { .. }
                | TranscriptItem::UserMessage(_)
                | TranscriptItem::DaemonToolObservation(_)
                | TranscriptItem::ToolCallStarted { .. }
                | TranscriptItem::CompactionSummary(_) => {}
            }
            if let Some(turn_id) = item.turn_id().filter(|_| saw_assistant) {
                *accepted.model_completed_turns.entry(turn_id).or_default() += 1;
                saw_assistant = false;
            }
        }
        accepted
    }

    fn take_count<K: Eq + std::hash::Hash>(counts: &mut HashMap<K, usize>, key: &K) -> bool {
        let Some(count) = counts.get_mut(key) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            counts.remove(key);
        }
        true
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
            AgentInput::Interrupt
            | AgentInput::Steer { .. }
            | AgentInput::FollowUp { .. }
            | AgentInput::DaemonObservation { .. } => None,
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

    fn take_accepted_item(&self, items: &mut AcceptedTranscriptItems) -> bool {
        match self {
            Self::ModelCompleted { turn_id, .. } => {
                AcceptedTranscriptItems::take_count(&mut items.model_completed_turns, turn_id)
            }
            Self::ModelFailed { turn_id, .. } => {
                AcceptedTranscriptItems::take_count(&mut items.model_failed_turns, turn_id)
            }
            Self::ToolCompleted {
                tool_call_id,
                tool_name,
                ..
            } => AcceptedTranscriptItems::take_count(
                &mut items.tool_results,
                &(tool_call_id.clone(), tool_name.clone()),
            ),
        }
    }

    fn should_wait_for_more_items(
        &self,
        finished_turns: &HashMap<TurnId, usize>,
        pending_by_turn: &HashMap<TurnId, usize>,
    ) -> bool {
        match self {
            Self::ToolCompleted { turn_id, .. } => {
                !finished_turns.contains_key(turn_id) && pending_by_turn.contains_key(turn_id)
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
thread_local! {
    static COMPLETION_OPERATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
fn count_completion_operation() {
    COMPLETION_OPERATIONS.set(COMPLETION_OPERATIONS.get() + 1);
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

    fn tool_completion(action: u64, turn: u64, id: u64) -> AgentInput {
        AgentInput::ToolCompleted {
            action_id: ActionId(action),
            turn_id: TurnId(turn),
            result: tool_result(id),
        }
    }

    fn tool_result(id: u64) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: ToolCallId::from_u64(id),
            tool_name: "bash".to_string(),
            output: "ok".to_string(),
            status: ToolResultStatus::Success,
        }
    }

    fn reset_completion_operations() {
        COMPLETION_OPERATIONS.set(0);
    }

    fn completion_operations() -> usize {
        COMPLETION_OPERATIONS.get()
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
    fn duplicate_completion_is_rejected_by_exact_action_id() {
        let mut actions = OutstandingActions::default();
        actions.track_request(&tool_request(2, 1, 1));

        assert!(actions.accept_completion(&tool_completion(2, 1, 1)));
        assert!(!actions.accept_completion(&tool_completion(2, 1, 1)));
    }

    #[test]
    fn duplicate_provider_identities_are_released_in_source_order() {
        let mut actions = OutstandingActions::default();
        actions.track_request(&tool_request(2, 1, 1));
        actions.track_request(&tool_request(3, 1, 1));
        assert!(actions.accept_completion(&tool_completion(3, 1, 1)));
        assert!(actions.accept_completion(&tool_completion(2, 1, 1)));
        let mut events = VecDeque::new();

        actions.emit_events_after_core_accepts(
            &[
                TranscriptItem::ToolResult(tool_result(1)),
                TranscriptItem::ToolResult(tool_result(1)),
            ],
            &mut events,
        );

        assert_eq!(
            events,
            VecDeque::from([
                SessionEvent::ActionCompleted {
                    kind: SessionActionKind::Tool,
                    id: "2".to_string(),
                },
                SessionEvent::ActionCompleted {
                    kind: SessionActionKind::Tool,
                    id: "3".to_string(),
                },
            ])
        );
    }

    #[test]
    fn in_order_completions_release_in_source_order() {
        let mut actions = OutstandingActions::default();
        actions.track_request(&tool_request(2, 1, 1));
        actions.track_request(&tool_request(3, 1, 2));
        let mut events = VecDeque::new();

        assert!(actions.accept_completion(&tool_completion(2, 1, 1)));
        actions.emit_events_after_core_accepts(
            &[TranscriptItem::ToolResult(tool_result(1))],
            &mut events,
        );
        assert!(actions.accept_completion(&tool_completion(3, 1, 2)));
        actions.emit_events_after_core_accepts(
            &[TranscriptItem::ToolResult(tool_result(2))],
            &mut events,
        );

        assert_eq!(
            events,
            VecDeque::from([
                SessionEvent::ActionCompleted {
                    kind: SessionActionKind::Tool,
                    id: "2".to_string(),
                },
                SessionEvent::ActionCompleted {
                    kind: SessionActionKind::Tool,
                    id: "3".to_string(),
                },
            ])
        );
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
        assert!(actions.accept_completion(&AgentInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        }));
        actions.clear();
        assert_eq!(actions, OutstandingActions::default());
    }

    #[test]
    fn completion_matching_and_source_order_release_scale_linearly() {
        for tool_count in [1, 10, 100, 1_000] {
            let mut actions = OutstandingActions::default();
            for index in 0..tool_count {
                actions.track_request(&tool_request(index as u64 + 1, 1, index as u64 + 1));
            }
            let mut events = VecDeque::new();
            reset_completion_operations();

            for index in (0..tool_count).rev() {
                assert!(actions.accept_completion(&tool_completion(
                    index as u64 + 1,
                    1,
                    index as u64 + 1,
                )));
                let items = if index == 0 {
                    (0..tool_count)
                        .map(|index| TranscriptItem::ToolResult(tool_result(index as u64 + 1)))
                        .collect()
                } else {
                    Vec::new()
                };
                actions.emit_events_after_core_accepts(&items, &mut events);
            }

            let expected_events = (0..tool_count)
                .map(|index| SessionEvent::ActionCompleted {
                    kind: SessionActionKind::Tool,
                    id: (index + 1).to_string(),
                })
                .collect::<VecDeque<_>>();
            assert_eq!(events, expected_events);
            assert_eq!(actions, OutstandingActions::default());
            assert_eq!(
                completion_operations(),
                4 * tool_count - 1,
                "one lookup per completion plus source-front and transcript-item examinations"
            );
        }
    }
}
