use std::collections::VecDeque;

use crate::action::AgentAction;
use crate::ids::TurnId;
use crate::mailbox::{Mailbox, MailboxNotification};
use crate::message::{
    AssistantMessage, CompactMessage, ToolCall, ToolResultMessage, UserInput, UserMessage,
};
use crate::state::AgentState;
use crate::transcript::Transcript;
use crate::transcript_record::{TranscriptRecord, TurnOutcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentInput {
    // User asked to stop the active model/tool work.
    Interrupt,
    // High-priority user input. Runs before queued follow-up work.
    Steer(UserInput),
    // Normal-priority user input for the next available turn.
    FollowUp(UserInput),
    // Volatile model/tool completion delivered by the orchestrator.
    Notification(MailboxNotification),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCoreLoop {
    pub mailbox: Mailbox,
    pub transcript: Transcript,
    pub state: AgentState,
    pub last_turn_id: TurnId,
    action_outbox: VecDeque<AgentAction>,
    interrupt_requested: bool,
}

impl Default for AgentCoreLoop {
    fn default() -> Self {
        Self {
            mailbox: Mailbox::default(),
            transcript: Transcript::new(),
            state: AgentState::Idle,
            last_turn_id: TurnId::default(),
            action_outbox: VecDeque::new(),
            interrupt_requested: false,
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
            interrupt_requested: false,
        }
    }

    pub fn on_input(&mut self, input: AgentInput) {
        match input {
            AgentInput::Interrupt => {
                self.interrupt_requested = true;
            }
            AgentInput::Steer(input) => {
                self.mailbox.push_steer(input);
            }
            AgentInput::FollowUp(input) => {
                self.mailbox.push_follow_up(input);
            }
            AgentInput::Notification(notification) => {
                // External completions should preempt queued future work for the current turn.
                self.mailbox.push_notification_front(notification);
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
            if self.handle_interrupt() {
                continue;
            }

            if self.consume_ready_notification() {
                continue;
            }

            if self.start_queued_tool_if_ready() {
                continue;
            }

            if self.resume_model_if_ready() {
                continue;
            }

            if self.start_next_turn() {
                continue;
            }

            return;
        }
    }

    fn consume_ready_notification(&mut self) -> bool {
        let Some(notification) = self.mailbox.front_notification().cloned() else {
            return false;
        };

        if !self.state.validate_mailbox_notification(&notification) {
            let _ = self.pop_notification();
            return true;
        }

        let notification = self.pop_notification();
        self.handle_mailbox_notification(notification);
        true
    }

    fn start_queued_tool_if_ready(&mut self) -> bool {
        let AgentState::ReadyToContinue { turn_id } = &self.state else {
            return false;
        };
        let turn_id = *turn_id;

        let Some((queued_turn_id, tool_call)) = self.mailbox.pop_tool_call() else {
            return false;
        };

        debug_assert_eq!(
            queued_turn_id, turn_id,
            "queued tool call belonged to a different turn"
        );

        if queued_turn_id != turn_id {
            return true;
        }

        self.start_tool_call(turn_id, tool_call);
        true
    }

    fn resume_model_if_ready(&mut self) -> bool {
        let Some(turn_id) = self.state.resume_model() else {
            return false;
        };

        self.enqueue_action(AgentAction::RequestModel { turn_id });
        true
    }

    fn start_next_turn(&mut self) -> bool {
        match &self.state {
            AgentState::Idle | AgentState::Interrupted | AgentState::Crashed => {
                let Some(input) = self.mailbox.pop_user_input() else {
                    return false;
                };
                self.start_turn(input);
                true
            }
            AgentState::RunningModel { .. }
            | AgentState::RunningTool { .. }
            | AgentState::ReadyToContinue { .. } => false,
        }
    }

    fn pop_notification(&mut self) -> MailboxNotification {
        self.mailbox
            .pop_notification()
            .expect("front notification disappeared before it could be consumed")
    }

    fn start_turn(&mut self, input: UserInput) {
        self.mailbox.clear_tool_calls();
        self.last_turn_id = self.last_turn_id.next();
        let turn_id = self.last_turn_id;
        let user_message = self.create_user_message(input);
        let started = self.state.start_turn(turn_id);
        debug_assert!(started, "start_turn called while turn is already active");

        self.append_record(TranscriptRecord::TurnStarted { turn_id });
        self.append_record(TranscriptRecord::UserMessage(user_message));
        self.enqueue_action(AgentAction::RequestModel { turn_id });
    }

    fn handle_mailbox_notification(&mut self, notification: MailboxNotification) {
        match notification {
            MailboxNotification::AssistantMessage { turn_id, assistant } => {
                self.on_assistant_message(turn_id, assistant);
            }
            MailboxNotification::ToolResult { turn_id, result } => {
                self.on_tool_result(turn_id, result);
            }
        }
    }

    fn on_assistant_message(&mut self, turn_id: TurnId, assistant: AssistantMessage) {
        if !matches!(
            &self.state,
            AgentState::RunningModel { turn_id: active_turn_id } if *active_turn_id == turn_id
        ) {
            return;
        }

        self.append_record(TranscriptRecord::AssistantMessage(assistant.clone()));

        let mut tool_calls = assistant.tool_calls().cloned();
        let Some(first_tool_call) = tool_calls.next() else {
            self.mailbox.clear_tool_calls();
            let finished = self.state.finish_model_turn(turn_id);
            debug_assert!(finished, "assistant message consumed outside model state");
            self.append_record(TranscriptRecord::TurnFinished {
                turn_id,
                outcome: TurnOutcome::Graceful,
            });
            return;
        };

        for tool_call in tool_calls {
            self.mailbox.push_tool_call(turn_id, tool_call);
        }

        self.start_tool_call(turn_id, first_tool_call);
    }

    fn start_tool_call(&mut self, turn_id: TurnId, tool_call: ToolCall) {
        if !self.state.start_tool(turn_id, tool_call.clone()) {
            return;
        }

        self.append_record(TranscriptRecord::ToolCallStarted {
            turn_id,
            tool_call: tool_call.clone(),
        });
        self.enqueue_action(AgentAction::RequestTool { turn_id, tool_call });
    }

    fn on_tool_result(&mut self, turn_id: TurnId, result: ToolResultMessage) {
        if !self.state.finish_tool(turn_id, &result) {
            return;
        }

        self.append_record(TranscriptRecord::ToolResult(result));
    }

    fn handle_interrupt(&mut self) -> bool {
        if !self.interrupt_requested {
            return false;
        }

        self.interrupt_requested = false;

        let Some(interrupted) = self.state.interrupt() else {
            return false;
        };

        self.mailbox.clear_tool_calls();

        if let Some(tool_call) = interrupted.tool_call {
            let interrupted_tool_result =
                ToolResultMessage::interrupted(tool_call.id, tool_call.tool_name);
            self.append_record(TranscriptRecord::ToolResult(interrupted_tool_result));
        }

        self.append_record(TranscriptRecord::TurnFinished {
            turn_id: interrupted.turn_id,
            outcome: TurnOutcome::Interrupted,
        });

        if interrupted.cancel_active {
            self.enqueue_action(AgentAction::CancelActive {
                turn_id: interrupted.turn_id,
            });
        }

        true
    }

    fn create_user_message(&mut self, input: UserInput) -> UserMessage {
        UserMessage { text: input.text }
    }

    fn append_record(&mut self, record: TranscriptRecord) {
        self.transcript.append(record);
    }

    fn enqueue_action(&mut self, action: AgentAction) {
        self.action_outbox.push_back(action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::AgentAction;
    use crate::ids::ToolCallId;
    use crate::message::AssistantItem;
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

        loop_state.on_input(AgentInput::Notification(
            MailboxNotification::AssistantMessage {
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            },
        ));

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
        loop_state.on_input(AgentInput::Notification(
            MailboxNotification::AssistantMessage {
                turn_id: TurnId(1),
                assistant,
            },
        ));
        loop_state.drain_actions();

        let result = successful_tool_result(tool_call.id, "bash");
        loop_state.on_input(AgentInput::Notification(MailboxNotification::ToolResult {
            turn_id: TurnId(1),
            result: result.clone(),
        }));

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
        loop_state.on_input(AgentInput::Notification(
            MailboxNotification::AssistantMessage {
                turn_id: TurnId(1),
                assistant,
            },
        ));
        loop_state.drain_actions();

        let first_result = successful_tool_result(first.id, "bash");
        loop_state.on_input(AgentInput::Notification(MailboxNotification::ToolResult {
            turn_id: TurnId(1),
            result: first_result.clone(),
        }));

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
        loop_state.on_input(AgentInput::Notification(
            MailboxNotification::AssistantMessage {
                turn_id: TurnId(1),
                assistant,
            },
        ));
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
        loop_state.on_input(AgentInput::Notification(
            MailboxNotification::AssistantMessage {
                turn_id: TurnId(1),
                assistant: stale_assistant,
            },
        ));

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
        loop_state.on_input(AgentInput::Notification(
            MailboxNotification::AssistantMessage {
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            },
        ));

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
