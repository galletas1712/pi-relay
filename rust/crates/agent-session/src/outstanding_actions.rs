use std::collections::VecDeque;

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

    pub(crate) fn track_session_action(&mut self, action: &SessionAction) {
        if let Some(action) = CompletionTarget::from_session_action(action) {
            self.pending.push_back(action);
        }
    }

    pub(crate) fn accept_completion(&mut self, input: &AgentInput) -> bool {
        let Some(completion) = RecordedCompletion::from_input(input) else {
            return false;
        };
        let target = completion.target();
        let position = if agent_perf::is_recording() {
            let mut entries_scanned = 0;
            let position = self.pending.iter().position(|action| {
                entries_scanned += 1;
                action == &target
            });
            agent_perf::action_completion_scan(entries_scanned);
            position
        } else {
            self.pending.iter().position(|action| action == &target)
        };
        let Some(position) = position else {
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

    #[test]
    #[ignore = "deterministic Stage 0 scaling fixture; run explicitly"]
    fn reverse_order_completion_scaling_k_1_10_100_1000() {
        for count in [1_u64, 10, 100, 1_000] {
            let mut actions = OutstandingActions::default();
            for action_id in 1..=count {
                actions.track_request(&model_request(action_id, action_id));
            }
            let metrics = agent_perf::Metrics::for_test(agent_perf::Operation::ModelAction);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime builds");
            let started = std::time::Instant::now();
            runtime.block_on(metrics.scope(async {
                for action_id in (1..=count).rev() {
                    assert!(actions.accept_completion(&AgentInput::ModelCompleted {
                        action_id: ActionId(action_id),
                        turn_id: TurnId(action_id),
                        assistant: AssistantMessage { items: Vec::new() },
                    }));
                }
            }));
            let elapsed = started.elapsed();
            let snapshot = metrics.snapshot();
            assert_eq!(snapshot.action_completion_scans, count);
            assert_eq!(
                snapshot.action_completion_entries_scanned,
                count.saturating_mul(count.saturating_add(1)) / 2
            );
            eprintln!(
                "perf fixture=reverse_action_completion k={count} scans={} entries_scanned={} elapsed_ns={} entries_per_second={:.0}",
                snapshot.action_completion_scans,
                snapshot.action_completion_entries_scanned,
                elapsed.as_nanos(),
                snapshot.action_completion_entries_scanned as f64 / elapsed.as_secs_f64()
            );
        }
    }
}
