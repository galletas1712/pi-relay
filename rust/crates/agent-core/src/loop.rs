use std::collections::VecDeque;

use crate::action::AgentAction;
use crate::event::AgentInput;
use crate::ids::TurnId;
use crate::mailbox::Mailbox;
use crate::message::CompactMessage;
use crate::state::AgentState;
use crate::transcript::{Transcript, TranscriptRecord, TurnOutcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCoreLoop {
    pub mailbox: Mailbox,
    pub transcript: Transcript,
    pub state: AgentState,
    pub last_turn_id: TurnId,
    action_outbox: VecDeque<AgentAction>,
}

impl Default for AgentCoreLoop {
    fn default() -> Self {
        Self {
            mailbox: Mailbox::default(),
            transcript: Transcript::new(),
            state: AgentState::Idle,
            last_turn_id: TurnId::default(),
            action_outbox: VecDeque::new(),
        }
    }
}

impl AgentCoreLoop {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_records(records: Vec<TranscriptRecord>) -> Self {
        Self::from_transcript(Transcript::from_records(records))
    }

    pub fn from_transcript(transcript: Transcript) -> Self {
        let last_turn_id = transcript.last_turn_id();
        let state = match transcript.tail_outcome() {
            Some(TurnOutcome::Interrupted) => AgentState::Interrupted,
            Some(TurnOutcome::Crashed) => AgentState::Crashed,
            Some(TurnOutcome::Graceful) | None => AgentState::Idle,
        };

        Self {
            mailbox: Mailbox::default(),
            transcript,
            state,
            last_turn_id,
            action_outbox: VecDeque::new(),
        }
    }

    pub fn enqueue_input(&mut self, input: AgentInput) {
        self.mailbox.push_input(input);
    }

    pub fn drain_actions(&mut self) -> Vec<AgentAction> {
        self.action_outbox.drain(..).collect()
    }

    pub fn compact_transcript(&self) -> Vec<CompactMessage> {
        self.transcript.compact()
    }

    pub(crate) fn drive(&mut self) {
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
            self.apply_transition(records, actions);
        }
    }

    fn apply_transition(&mut self, records: Vec<TranscriptRecord>, actions: Vec<AgentAction>) {
        for record in records {
            self.transcript.append(record);
        }
        self.action_outbox.extend(actions);
    }
}

fn started_turn_id(records: &[TranscriptRecord]) -> Option<TurnId> {
    records.iter().find_map(|record| match record {
        TranscriptRecord::TurnStarted { turn_id } => Some(*turn_id),
        TranscriptRecord::UserMessage(_)
        | TranscriptRecord::AssistantMessage(_)
        | TranscriptRecord::ToolCallStarted { .. }
        | TranscriptRecord::ToolResult(_)
        | TranscriptRecord::TurnFinished { .. } => None,
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

    fn drive_input(loop_state: &mut AgentCoreLoop, input: AgentInput) {
        loop_state.enqueue_input(input);
        loop_state.drive();
    }

    #[test]
    fn starting_a_turn_appends_boundary_events_and_requests_the_model() {
        let mut loop_state = AgentCoreLoop::new();

        drive_input(&mut loop_state, AgentInput::FollowUp("hello".to_string()));

        assert_eq!(
            loop_state.transcript.records(),
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
        drive_input(&mut loop_state, AgentInput::FollowUp("hello".to_string()));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![
            AssistantItem::Text("Let me inspect that.".to_string()),
            AssistantItem::ToolCall(tool_call.clone()),
        ]);

        drive_input(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            },
        );

        assert_eq!(
            loop_state.transcript.records(),
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
        drive_input(&mut loop_state, AgentInput::FollowUp("hello".to_string()));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![AssistantItem::ToolCall(tool_call.clone())]);
        drive_input(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant,
            },
        );
        loop_state.drain_actions();

        let result = successful_tool_result(tool_call.id, "bash");
        drive_input(
            &mut loop_state,
            AgentInput::ToolCompleted {
                turn_id: TurnId(1),
                result: result.clone(),
            },
        );

        assert_eq!(
            loop_state.transcript.records().last(),
            Some(&TranscriptRecord::ToolResult(result))
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
    fn multiple_tool_calls_run_in_parallel_and_results_are_recorded_in_source_order() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        drive_input(&mut loop_state, AgentInput::FollowUp("hello".to_string()));
        loop_state.drain_actions();

        let first = tool_call(&mut next_tool_call_id, "bash");
        let second = tool_call(&mut next_tool_call_id, "read");
        let assistant = assistant_message(vec![
            AssistantItem::ToolCall(first.clone()),
            AssistantItem::ToolCall(second.clone()),
        ]);
        drive_input(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant,
            },
        );

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
            loop_state.transcript.records().last(),
            Some(&TranscriptRecord::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: second.clone(),
            })
        );

        let second_result = successful_tool_result(second.id, "read");
        drive_input(
            &mut loop_state,
            AgentInput::ToolCompleted {
                turn_id: TurnId(1),
                result: second_result.clone(),
            },
        );
        drive_input(&mut loop_state, AgentInput::Steer("urgent".to_string()));

        assert_eq!(
            loop_state.transcript.records().last(),
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
        drive_input(
            &mut loop_state,
            AgentInput::ToolCompleted {
                turn_id: TurnId(1),
                result: first_result.clone(),
            },
        );

        assert_eq!(
            loop_state.transcript.records().last(),
            Some(&TranscriptRecord::ToolResult(second_result.clone()))
        );
        assert_eq!(
            loop_state.transcript.records()[5],
            TranscriptRecord::ToolResult(first_result)
        );
        assert_eq!(
            loop_state.transcript.records()[6],
            TranscriptRecord::ToolResult(second_result)
        );
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
        drive_input(&mut loop_state, AgentInput::FollowUp("initial".to_string()));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![AssistantItem::ToolCall(tool_call.clone())]);
        drive_input(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant,
            },
        );
        loop_state.drain_actions();

        drive_input(&mut loop_state, AgentInput::Steer("urgent".to_string()));

        assert!(loop_state.drain_actions().is_empty());

        drive_input(&mut loop_state, AgentInput::Interrupt);

        assert_eq!(
            loop_state.transcript.records(),
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
        drive_input(&mut loop_state, AgentInput::FollowUp("initial".to_string()));
        loop_state.drain_actions();

        let first = tool_call(&mut next_tool_call_id, "bash");
        let second = tool_call(&mut next_tool_call_id, "read");
        let assistant = assistant_message(vec![
            AssistantItem::ToolCall(first.clone()),
            AssistantItem::ToolCall(second.clone()),
        ]);
        drive_input(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant,
            },
        );
        loop_state.drain_actions();

        let second_result = successful_tool_result(second.id, "read");
        drive_input(
            &mut loop_state,
            AgentInput::ToolCompleted {
                turn_id: TurnId(1),
                result: second_result.clone(),
            },
        );
        drive_input(&mut loop_state, AgentInput::Interrupt);

        assert_eq!(
            loop_state.transcript.records().last(),
            Some(&TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Interrupted,
            })
        );
        assert_eq!(
            loop_state.transcript.records()[5],
            TranscriptRecord::ToolResult(ToolResultMessage::interrupted(first.id, "bash"))
        );
        assert_eq!(
            loop_state.transcript.records()[6],
            TranscriptRecord::ToolResult(second_result)
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::CancelTurn { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.state, AgentState::Interrupted);
    }

    #[test]
    fn interrupting_a_running_model_without_queued_work_finishes_interrupted() {
        let mut loop_state = AgentCoreLoop::new();
        drive_input(&mut loop_state, AgentInput::FollowUp("hello".to_string()));
        loop_state.drain_actions();

        drive_input(&mut loop_state, AgentInput::Interrupt);

        assert_eq!(
            loop_state.transcript.records(),
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
        assert_eq!(loop_state.state, AgentState::Interrupted);
    }

    #[test]
    fn stale_completions_are_ignored_after_an_interrupt() {
        let mut loop_state = AgentCoreLoop::new();
        drive_input(&mut loop_state, AgentInput::FollowUp("hello".to_string()));
        loop_state.drain_actions();
        drive_input(&mut loop_state, AgentInput::Interrupt);
        loop_state.drain_actions();

        let stale_assistant = assistant_message(vec![AssistantItem::Text("stale".to_string())]);
        drive_input(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant: stale_assistant,
            },
        );

        assert_eq!(loop_state.transcript.records().len(), 3);
        assert!(loop_state.drain_actions().is_empty());
        assert_eq!(loop_state.state, AgentState::Interrupted);
    }

    #[test]
    fn compact_transcript_filters_to_user_and_assistant_messages() {
        let mut loop_state = AgentCoreLoop::new();
        drive_input(&mut loop_state, AgentInput::FollowUp("hello".to_string()));
        loop_state.drain_actions();

        let assistant = assistant_message(vec![AssistantItem::Text("hi".to_string())]);
        drive_input(
            &mut loop_state,
            AgentInput::ModelCompleted {
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            },
        );

        assert_eq!(
            loop_state.compact_transcript(),
            vec![
                CompactMessage::User("hello".to_string()),
                CompactMessage::Assistant(assistant),
            ]
        );
    }

    #[test]
    fn rehydrating_an_incomplete_transcript_patches_a_crashed_finish() {
        let transcript = vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(7) },
            TranscriptRecord::UserMessage("hello".to_string()),
        ];

        let loop_state = AgentCoreLoop::from_records(transcript);

        assert_eq!(
            loop_state.transcript.records(),
            vec![
                TranscriptRecord::TurnStarted { turn_id: TurnId(7) },
                TranscriptRecord::UserMessage("hello".to_string()),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(7),
                    outcome: TurnOutcome::Crashed,
                },
            ]
        );
        assert_eq!(loop_state.state, AgentState::Crashed);
        assert_eq!(loop_state.last_turn_id, TurnId(7));
    }

    #[test]
    fn rehydrating_a_graceful_boundary_restores_idle_state() {
        let transcript = vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
            TranscriptRecord::UserMessage("hello".to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ];

        let loop_state = AgentCoreLoop::from_records(transcript.clone());

        assert_eq!(loop_state.transcript.records(), transcript.as_slice());
        assert_eq!(loop_state.state, AgentState::Idle);
        assert_eq!(loop_state.last_turn_id, TurnId(2));
    }
}
