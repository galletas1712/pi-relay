use std::collections::VecDeque;

use crate::action::AgentAction;
use crate::event::AgentInput;
use crate::ids::TurnId;
use crate::mailbox::Mailbox;
use crate::record::TranscriptRecord;
use crate::state::AgentState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCoreLoop {
    mailbox: Mailbox,
    state: AgentState,
    last_turn_id: TurnId,
    action_outbox: VecDeque<AgentAction>,
    record_outbox: VecDeque<TranscriptRecord>,
}

impl Default for AgentCoreLoop {
    fn default() -> Self {
        Self {
            mailbox: Mailbox::default(),
            state: AgentState::Idle,
            last_turn_id: TurnId::default(),
            action_outbox: VecDeque::new(),
            record_outbox: VecDeque::new(),
        }
    }
}

impl AgentCoreLoop {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resume a fresh idle core at the given turn boundary.
    ///
    /// Callers own durable history; the core itself no longer buffers records.
    /// The session derives `last_turn_id` from its log before calling this.
    pub fn resume_at_boundary(last_turn_id: TurnId) -> Self {
        Self {
            mailbox: Mailbox::default(),
            state: AgentState::Idle,
            last_turn_id,
            action_outbox: VecDeque::new(),
            record_outbox: VecDeque::new(),
        }
    }

    pub fn enqueue_input(&mut self, input: AgentInput) {
        self.mailbox.push_input(input);
    }

    /// True when the core is between turns and has no in-flight model/tool work.
    ///
    /// Exposed so callers can observe liveness without reaching into the
    /// underlying `AgentState` enum, which is a private implementation detail.
    pub fn is_idle(&self) -> bool {
        self.state == AgentState::Idle
    }

    /// True when the mailbox still has queued inputs waiting to be processed.
    ///
    /// Exposed so callers can observe liveness without reaching into the
    /// underlying `Mailbox`, which is a private implementation detail.
    pub fn has_pending_work(&self) -> bool {
        self.mailbox.total_len() > 0
    }

    /// The most recent turn id observed by the core.
    pub fn last_turn_id(&self) -> TurnId {
        self.last_turn_id
    }

    pub fn drain_actions(&mut self) -> Vec<AgentAction> {
        self.action_outbox.drain(..).collect()
    }

    pub fn drain_records(&mut self) -> Vec<TranscriptRecord> {
        self.record_outbox.drain(..).collect()
    }

    pub fn drive(&mut self) {
        loop {
            let next_turn_id = self.last_turn_id.next();
            let Some(event) = self.mailbox.next_event(&self.state, next_turn_id) else {
                return;
            };

            let (records, actions) = self.state.step(event);
            if records.is_empty() && actions.is_empty() {
                continue;
            }

            if let Some(turn_id) = started_turn_id(&records) {
                self.last_turn_id = turn_id;
            }
            self.record_outbox.extend(records);
            self.action_outbox.extend(actions);
        }
    }
}

fn started_turn_id(records: &[TranscriptRecord]) -> Option<TurnId> {
    records.iter().find_map(|record| match record {
        TranscriptRecord::TurnStarted { turn_id } => Some(*turn_id),
        TranscriptRecord::UserMessage(_)
        | TranscriptRecord::AssistantMessage(_)
        | TranscriptRecord::ToolCallStarted { .. }
        | TranscriptRecord::ToolResult(_)
        | TranscriptRecord::TurnFinished { .. }
        | TranscriptRecord::Custom(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::AgentAction;
    use crate::ids::ToolCallId;
    use crate::message::{
        AssistantItem, AssistantMessage, ToolCall, ToolResultMessage, ToolResultStatus,
    };
    use crate::record::TurnOutcome;

    fn assistant_message(items: Vec<AssistantItem>) -> AssistantMessage {
        AssistantMessage { items }
    }

    fn tool_call(next_tool_call_id: &mut ToolCallId, name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::take_next(next_tool_call_id),
            tool_name: name.to_string(),
            args_json: "{}".to_string(),
        }
    }

    fn successful_tool_result(tool_call_id: ToolCallId, tool_name: &str) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id,
            tool_name: tool_name.to_string(),
            output: "ok".to_string(),
            status: ToolResultStatus::Success,
        }
    }

    /// Drive a single input end-to-end and collect every record emitted during
    /// that cycle. Tests accumulate these into a running transcript so they can
    /// assert the same shape they used to read off `loop_state.transcript`.
    fn drive_collect(loop_state: &mut AgentCoreLoop, input: AgentInput) -> Vec<TranscriptRecord> {
        loop_state.enqueue_input(input);
        loop_state.drive();
        loop_state.drain_records()
    }

    #[test]
    fn starting_a_turn_appends_boundary_events_and_requests_the_model() {
        let mut loop_state = AgentCoreLoop::new();

        let records = drive_collect(&mut loop_state, AgentInput::FollowUp("hello".to_string()));

        assert_eq!(
            records,
            vec![
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage("hello".to_string()),
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );
        assert_eq!(
            loop_state.state,
            AgentState::RunningModel { turn_id: TurnId(1) }
        );
    }

    #[test]
    fn model_completion_with_a_tool_call_appends_assistant_and_starts_the_tool() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        let mut records = drive_collect(&mut loop_state, AgentInput::FollowUp("hello".to_string()));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![
            AssistantItem::Text("Let me inspect that.".to_string()),
            AssistantItem::ToolCall(tool_call.clone()),
        ]);

        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            },
        ));

        assert_eq!(
            records,
            vec![
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage("hello".to_string()),
                TranscriptRecord::AssistantMessage(assistant.clone()),
                TranscriptRecord::ToolCallStarted {
                    turn_id: TurnId(1),
                    tool_call: tool_call.clone(),
                },
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestTool {
                turn_id: TurnId(1),
                tool_call: tool_call.clone(),
            }]
        );
        assert_eq!(
            loop_state.state,
            AgentState::RunningTools {
                turn_id: TurnId(1),
                tool_calls: vec![tool_call],
                completed_results: vec![None],
                next_result_index: 0,
            }
        );
    }

    #[test]
    fn tool_completion_appends_a_result_and_resumes_the_model() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        drive_collect(&mut loop_state, AgentInput::FollowUp("hello".to_string()));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![AssistantItem::ToolCall(tool_call.clone())]);
        drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant,
            },
        );
        loop_state.drain_actions();

        let result = successful_tool_result(tool_call.id, "bash");
        let records = drive_collect(
            &mut loop_state,
            AgentInput::ToolCompleted {
                turn_id: TurnId(1),
                result: result.clone(),
            },
        );

        assert_eq!(records.last(), Some(&TranscriptRecord::ToolResult(result)));
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );
        assert_eq!(
            loop_state.state,
            AgentState::RunningModel { turn_id: TurnId(1) }
        );
    }

    #[test]
    fn multiple_tool_calls_run_in_parallel_and_results_are_recorded_in_source_order() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        let mut records = drive_collect(&mut loop_state, AgentInput::FollowUp("hello".to_string()));
        loop_state.drain_actions();

        let first = tool_call(&mut next_tool_call_id, "bash");
        let second = tool_call(&mut next_tool_call_id, "read");
        let assistant = assistant_message(vec![
            AssistantItem::ToolCall(first.clone()),
            AssistantItem::ToolCall(second.clone()),
        ]);
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant,
            },
        ));

        assert_eq!(
            loop_state.drain_actions(),
            vec![
                AgentAction::RequestTool {
                    turn_id: TurnId(1),
                    tool_call: first.clone(),
                },
                AgentAction::RequestTool {
                    turn_id: TurnId(1),
                    tool_call: second.clone(),
                },
            ]
        );
        assert_eq!(
            records.last(),
            Some(&TranscriptRecord::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: second.clone(),
            })
        );

        let second_result = successful_tool_result(second.id, "read");
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ToolCompleted {
                turn_id: TurnId(1),
                result: second_result.clone(),
            },
        ));
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::Steer("urgent".to_string()),
        ));

        assert_eq!(
            records.last(),
            Some(&TranscriptRecord::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: second.clone(),
            })
        );
        assert!(loop_state.drain_actions().is_empty());
        assert!(matches!(
            loop_state.state,
            AgentState::RunningTools {
                next_result_index: 0,
                ..
            }
        ));

        let first_result = successful_tool_result(first.id, "bash");
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ToolCompleted {
                turn_id: TurnId(1),
                result: first_result.clone(),
            },
        ));

        assert_eq!(
            records.last(),
            Some(&TranscriptRecord::ToolResult(second_result.clone()))
        );
        assert_eq!(records[5], TranscriptRecord::ToolResult(first_result));
        assert_eq!(records[6], TranscriptRecord::ToolResult(second_result));
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.mailbox.steer_len(), 1);
        assert_eq!(
            loop_state.state,
            AgentState::RunningModel { turn_id: TurnId(1) }
        );
    }

    #[test]
    fn interrupting_a_running_tool_closes_the_turn_and_starts_queued_steer_work() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        let mut records =
            drive_collect(&mut loop_state, AgentInput::FollowUp("initial".to_string()));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![AssistantItem::ToolCall(tool_call.clone())]);
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant,
            },
        ));
        loop_state.drain_actions();

        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::Steer("urgent".to_string()),
        ));

        assert!(loop_state.drain_actions().is_empty());

        records.extend(drive_collect(&mut loop_state, AgentInput::Interrupt));

        assert_eq!(
            records,
            vec![
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage("initial".to_string()),
                TranscriptRecord::AssistantMessage(assistant_message(vec![
                    AssistantItem::ToolCall(tool_call.clone(),)
                ])),
                TranscriptRecord::ToolCallStarted {
                    turn_id: TurnId(1),
                    tool_call: tool_call.clone(),
                },
                TranscriptRecord::ToolResult(ToolResultMessage::interrupted(tool_call.id, "bash")),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Interrupted,
                },
                TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
                TranscriptRecord::UserMessage("urgent".to_string()),
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![
                AgentAction::CancelTurn { turn_id: TurnId(1) },
                AgentAction::RequestModel { turn_id: TurnId(2) },
            ]
        );
        assert_eq!(
            loop_state.state,
            AgentState::RunningModel { turn_id: TurnId(2) }
        );
    }

    #[test]
    fn interrupting_parallel_tools_cancels_the_turn_and_records_unfinished_tools() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        let mut records =
            drive_collect(&mut loop_state, AgentInput::FollowUp("initial".to_string()));
        loop_state.drain_actions();

        let first = tool_call(&mut next_tool_call_id, "bash");
        let second = tool_call(&mut next_tool_call_id, "read");
        let assistant = assistant_message(vec![
            AssistantItem::ToolCall(first.clone()),
            AssistantItem::ToolCall(second.clone()),
        ]);
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant,
            },
        ));
        loop_state.drain_actions();

        let second_result = successful_tool_result(second.id, "read");
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ToolCompleted {
                turn_id: TurnId(1),
                result: second_result.clone(),
            },
        ));
        records.extend(drive_collect(&mut loop_state, AgentInput::Interrupt));

        assert_eq!(
            records.last(),
            Some(&TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Interrupted,
            })
        );
        assert_eq!(
            records[5],
            TranscriptRecord::ToolResult(ToolResultMessage::interrupted(first.id, "bash"))
        );
        assert_eq!(records[6], TranscriptRecord::ToolResult(second_result));
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::CancelTurn { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.state, AgentState::Idle);
    }

    #[test]
    fn interrupting_a_running_model_without_queued_work_finishes_interrupted() {
        let mut loop_state = AgentCoreLoop::new();
        let mut records = drive_collect(&mut loop_state, AgentInput::FollowUp("hello".to_string()));
        loop_state.drain_actions();

        records.extend(drive_collect(&mut loop_state, AgentInput::Interrupt));

        assert_eq!(
            records,
            vec![
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage("hello".to_string()),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Interrupted,
                },
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::CancelTurn { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.state, AgentState::Idle);
    }

    #[test]
    fn stale_completions_are_ignored_after_an_interrupt() {
        let mut loop_state = AgentCoreLoop::new();
        let mut records = drive_collect(&mut loop_state, AgentInput::FollowUp("hello".to_string()));
        loop_state.drain_actions();
        records.extend(drive_collect(&mut loop_state, AgentInput::Interrupt));
        loop_state.drain_actions();

        let stale_assistant = assistant_message(vec![AssistantItem::Text("stale".to_string())]);
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant: stale_assistant,
            },
        ));

        assert_eq!(records.len(), 3);
        assert!(loop_state.drain_actions().is_empty());
        assert_eq!(loop_state.state, AgentState::Idle);
    }
}
