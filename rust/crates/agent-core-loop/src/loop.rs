use std::collections::VecDeque;

use crate::action::AgentAction;
use crate::event::AgentEvent;
use crate::ids::TurnId;
use crate::mailbox::Mailbox;
use crate::message::{AssistantMessage, CompactMessage, ToolResultMessage, UserInput};
use crate::state::{AgentState, AgentTransition};
use crate::transcript::Transcript;
use crate::transcript_record::TranscriptRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentInput {
    // User asked to stop the active model/tool work.
    Interrupt,
    // High-priority user input. Runs before queued follow-up work.
    Steer(UserInput),
    // Normal-priority user input for the next available turn.
    FollowUp(UserInput),
    // Volatile model completion delivered by the orchestrator.
    ModelCompleted {
        turn_id: TurnId,
        assistant: AssistantMessage,
    },
    // Volatile tool completion delivered by the orchestrator.
    ToolCompleted {
        turn_id: TurnId,
        result: ToolResultMessage,
    },
}

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
        let state = AgentState::from_tail_outcome(transcript.tail_outcome());

        Self {
            mailbox: Mailbox::default(),
            transcript,
            state,
            last_turn_id,
            action_outbox: VecDeque::new(),
        }
    }

    pub fn on_input(&mut self, input: AgentInput) {
        match input {
            AgentInput::Interrupt => {
                self.mailbox.request_interrupt();
            }
            AgentInput::Steer(input) => {
                self.mailbox.push_steer(input);
            }
            AgentInput::FollowUp(input) => {
                self.mailbox.push_follow_up(input);
            }
            AgentInput::ModelCompleted { turn_id, assistant } => {
                // External completions should preempt queued future work for the current turn.
                self.mailbox
                    .push_notification_front(AgentEvent::ModelCompleted { turn_id, assistant });
            }
            AgentInput::ToolCompleted { turn_id, result } => {
                // External completions should preempt queued future work for the current turn.
                self.mailbox
                    .push_notification_front(AgentEvent::ToolCompleted { turn_id, result });
            }
        }

        self.drive();
    }

    pub fn drain_actions(&mut self) -> Vec<AgentAction> {
        self.action_outbox.drain(..).collect()
    }

    pub fn compact_transcript(&self) -> Vec<CompactMessage> {
        self.transcript.compact()
    }

    fn drive(&mut self) {
        loop {
            let next_turn_id = self.last_turn_id.next();
            let Some(event) = self.mailbox.next_event(&self.state, next_turn_id) else {
                return;
            };

            let started_turn_id = match &event {
                AgentEvent::StartTurn { turn_id, .. } => Some(*turn_id),
                AgentEvent::Interrupt
                | AgentEvent::ModelCompleted { .. }
                | AgentEvent::ToolReady(_)
                | AgentEvent::ToolCompleted { .. }
                | AgentEvent::ContinueModel => None,
            };
            let transition = self.state.step(event);
            if transition.is_empty() {
                continue;
            }

            if let Some(turn_id) = started_turn_id {
                self.last_turn_id = turn_id;
            }
            self.apply_transition(transition);
        }
    }

    fn apply_transition(&mut self, transition: AgentTransition) {
        for record in transition.records {
            self.transcript.append(record);
        }
        self.action_outbox.extend(transition.actions);

        if transition.clear_tool_calls {
            self.mailbox.clear_tool_calls();
        }
        for tool_call in transition.queued_tool_calls {
            self.mailbox.push_tool_call(tool_call);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::AgentAction;
    use crate::ids::ToolCallId;
    use crate::message::{AssistantItem, ToolCall, UserMessage};
    use crate::transcript_record::{TranscriptRecord, TurnOutcome};

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
            status: crate::message::ToolResultStatus::Success,
        }
    }

    #[test]
    fn starting_a_turn_appends_boundary_events_and_requests_the_model() {
        let mut loop_state = AgentCoreLoop::new();

        loop_state.on_input(AgentInput::FollowUp(UserInput::from("hello")));

        assert_eq!(
            loop_state.transcript.records(),
            vec![
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage(UserMessage {
                    text: "hello".to_string(),
                }),
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
        loop_state.on_input(AgentInput::FollowUp(UserInput::from("hello")));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![
            AssistantItem::Text("Let me inspect that.".to_string()),
            AssistantItem::ToolCall(tool_call.clone()),
        ]);

        loop_state.on_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: assistant.clone(),
        });

        assert_eq!(
            loop_state.transcript.records(),
            vec![
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage(UserMessage {
                    text: "hello".to_string(),
                }),
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
            AgentState::RunningTool {
                turn_id: TurnId(1),
                tool_call,
            }
        );
    }

    #[test]
    fn tool_completion_appends_a_result_and_resumes_the_model() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        loop_state.on_input(AgentInput::FollowUp(UserInput::from("hello")));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![AssistantItem::ToolCall(tool_call.clone())]);
        loop_state.on_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant,
        });
        loop_state.drain_actions();

        let result = successful_tool_result(tool_call.id, "bash");
        loop_state.on_input(AgentInput::ToolCompleted {
            turn_id: TurnId(1),
            result: result.clone(),
        });

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
    fn multiple_tool_calls_run_before_the_model_resumes() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        loop_state.on_input(AgentInput::FollowUp(UserInput::from("hello")));
        loop_state.drain_actions();

        let first = tool_call(&mut next_tool_call_id, "bash");
        let second = tool_call(&mut next_tool_call_id, "read");
        let assistant = assistant_message(vec![
            AssistantItem::ToolCall(first.clone()),
            AssistantItem::ToolCall(second.clone()),
        ]);
        loop_state.on_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant,
        });
        loop_state.drain_actions();

        let first_result = successful_tool_result(first.id, "bash");
        loop_state.on_input(AgentInput::ToolCompleted {
            turn_id: TurnId(1),
            result: first_result.clone(),
        });

        assert_eq!(
            loop_state.transcript.records().last(),
            Some(&TranscriptRecord::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: second.clone(),
            })
        );
        assert_eq!(
            loop_state.transcript.records()[4],
            TranscriptRecord::ToolResult(first_result)
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestTool {
                turn_id: TurnId(1),
                tool_call: second.clone(),
            }]
        );
        assert_eq!(
            loop_state.state,
            AgentState::RunningTool {
                turn_id: TurnId(1),
                tool_call: second,
            }
        );
    }

    #[test]
    fn interrupting_a_running_tool_closes_the_turn_and_starts_queued_steer_work() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        loop_state.on_input(AgentInput::FollowUp(UserInput::from("initial")));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![AssistantItem::ToolCall(tool_call.clone())]);
        loop_state.on_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant,
        });
        loop_state.drain_actions();

        loop_state.on_input(AgentInput::Steer(UserInput::from("urgent")));

        assert!(loop_state.drain_actions().is_empty());

        loop_state.on_input(AgentInput::Interrupt);

        assert_eq!(
            loop_state.transcript.records(),
            vec![
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage(UserMessage {
                    text: "initial".to_string(),
                }),
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
                TranscriptRecord::UserMessage(UserMessage {
                    text: "urgent".to_string(),
                }),
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![
                AgentAction::CancelActive { turn_id: TurnId(1) },
                AgentAction::RequestModel { turn_id: TurnId(2) },
            ]
        );
        assert_eq!(
            loop_state.state,
            AgentState::RunningModel { turn_id: TurnId(2) }
        );
    }

    #[test]
    fn interrupting_a_running_model_without_queued_work_finishes_interrupted() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::FollowUp(UserInput::from("hello")));
        loop_state.drain_actions();

        loop_state.on_input(AgentInput::Interrupt);

        assert_eq!(
            loop_state.transcript.records(),
            vec![
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage(UserMessage {
                    text: "hello".to_string(),
                }),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Interrupted,
                },
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::CancelActive { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.state, AgentState::Interrupted);
    }

    #[test]
    fn stale_completions_are_ignored_after_an_interrupt() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::FollowUp(UserInput::from("hello")));
        loop_state.drain_actions();
        loop_state.on_input(AgentInput::Interrupt);
        loop_state.drain_actions();

        let stale_assistant = assistant_message(vec![AssistantItem::Text("stale".to_string())]);
        loop_state.on_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: stale_assistant,
        });

        assert_eq!(loop_state.transcript.records().len(), 3);
        assert!(loop_state.drain_actions().is_empty());
        assert_eq!(loop_state.state, AgentState::Interrupted);
    }

    #[test]
    fn compact_transcript_filters_to_user_and_assistant_messages() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::FollowUp(UserInput::from("hello")));
        loop_state.drain_actions();

        let assistant = assistant_message(vec![AssistantItem::Text("hi".to_string())]);
        loop_state.on_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: assistant.clone(),
        });

        assert_eq!(
            loop_state.compact_transcript(),
            vec![
                CompactMessage::User(UserMessage {
                    text: "hello".to_string(),
                }),
                CompactMessage::Assistant(assistant),
            ]
        );
    }

    #[test]
    fn rehydrating_an_incomplete_transcript_patches_a_crashed_finish() {
        let transcript = vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(7) },
            TranscriptRecord::UserMessage(UserMessage {
                text: "hello".to_string(),
            }),
        ];

        let loop_state = AgentCoreLoop::from_records(transcript);

        assert_eq!(
            loop_state.transcript.records(),
            vec![
                TranscriptRecord::TurnStarted { turn_id: TurnId(7) },
                TranscriptRecord::UserMessage(UserMessage {
                    text: "hello".to_string(),
                }),
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
            TranscriptRecord::UserMessage(UserMessage {
                text: "hello".to_string(),
            }),
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
