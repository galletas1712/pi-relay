use std::collections::VecDeque;

use crate::event::{AgentAction, AgentEvent, TurnOutcome};
use crate::ids::{EventId, TurnId};
use crate::mailbox::{Mailbox, MailboxCommand};
use crate::message::{
    AssistantMessage, CompactMessage, ToolCall, ToolResultMessage, UserInput, UserMessage,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    // The last completed turn ended via interrupt.
    Interrupted,
    // The last completed turn was synthesized as crashed during recovery.
    Crashed,
    RunningModel {
        turn_id: TurnId,
    },
    RunningTool {
        turn_id: TurnId,
        tool_call: ToolCall,
    },
}

impl Default for Phase {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentNotification {
    ModelCompleted {
        turn_id: TurnId,
        assistant: AssistantMessage,
    },
    ToolCompleted {
        turn_id: TurnId,
        result: ToolResultMessage,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentInput {
    Command(MailboxCommand),
    Notification(AgentNotification),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCoreLoop {
    pub mailbox: Mailbox,
    // Canonical append-only session log.
    // TODO: Add first-class compaction, rewind/fork, and resume APIs on top of
    // this log instead of relying on direct transcript manipulation.
    pub transcript: Vec<AgentEvent>,
    pub phase: Phase,
    pub last_turn_id: TurnId,
    action_outbox: VecDeque<AgentAction>,
    pending_tool_calls: VecDeque<ToolCall>,
    next_event_id: EventId,
}

impl Default for AgentCoreLoop {
    fn default() -> Self {
        Self {
            mailbox: Mailbox::default(),
            transcript: Vec::new(),
            phase: Phase::Idle,
            last_turn_id: TurnId::default(),
            action_outbox: VecDeque::new(),
            pending_tool_calls: VecDeque::new(),
            next_event_id: EventId::first(),
        }
    }
}

impl AgentCoreLoop {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_transcript(mut transcript: Vec<AgentEvent>) -> Self {
        let mut last_turn_id = TurnId::default();
        let mut open_turn = None;
        let mut max_event_id = 0_u64;

        for event in &transcript {
            match event {
                AgentEvent::TurnStarted { turn_id } => {
                    last_turn_id = last_turn_id.max(*turn_id);
                    open_turn = Some(*turn_id);
                }
                AgentEvent::UserMessage(message) => {
                    max_event_id = max_event_id.max(message.id.0);
                }
                AgentEvent::AssistantMessage(message) => {
                    max_event_id = max_event_id.max(message.id.0);
                    for tool_call in message.tool_calls() {
                        max_event_id = max_event_id.max(tool_call.id.0);
                    }
                }
                AgentEvent::ToolCallStarted { turn_id, tool_call } => {
                    last_turn_id = last_turn_id.max(*turn_id);
                    max_event_id = max_event_id.max(tool_call.id.0);
                }
                AgentEvent::ToolResult(result) => {
                    max_event_id = max_event_id.max(result.id.0);
                    max_event_id = max_event_id.max(result.tool_call_id.0);
                }
                AgentEvent::TurnFinished { turn_id, .. } => {
                    last_turn_id = last_turn_id.max(*turn_id);
                    if open_turn == Some(*turn_id) {
                        open_turn = None;
                    }
                }
            }
        }

        if let Some(turn_id) = open_turn {
            transcript.push(AgentEvent::TurnFinished {
                turn_id,
                outcome: TurnOutcome::Crashed,
            });
            last_turn_id = last_turn_id.max(turn_id);
        }

        let phase = match transcript.last() {
            Some(AgentEvent::TurnFinished {
                outcome: TurnOutcome::Interrupted,
                ..
            }) => Phase::Interrupted,
            Some(AgentEvent::TurnFinished {
                outcome: TurnOutcome::Crashed,
                ..
            }) => Phase::Crashed,
            _ => Phase::Idle,
        };

        let next_event_id = match max_event_id {
            0 => EventId::first(),
            value => EventId(value + 1),
        };

        Self {
            mailbox: Mailbox::default(),
            transcript,
            phase,
            last_turn_id,
            action_outbox: VecDeque::new(),
            pending_tool_calls: VecDeque::new(),
            next_event_id,
        }
    }

    pub fn on_input(&mut self, input: AgentInput) {
        match input {
            AgentInput::Command(command) => {
                self.mailbox.push(command);
                self.drive();
            }
            AgentInput::Notification(notification) => match notification {
                AgentNotification::ModelCompleted { turn_id, assistant } => {
                    self.on_model_completed(turn_id, assistant);
                }
                AgentNotification::ToolCompleted { turn_id, result } => {
                    self.on_tool_completed(turn_id, result);
                }
            },
        }
    }

    pub fn drain_actions(&mut self) -> Vec<AgentAction> {
        self.action_outbox.drain(..).collect()
    }

    pub fn compact_transcript(&self) -> Vec<CompactMessage> {
        self.transcript
            .iter()
            .filter_map(|event| match event {
                AgentEvent::UserMessage(message) => Some(CompactMessage::User(message.clone())),
                AgentEvent::AssistantMessage(message) => {
                    Some(CompactMessage::Assistant(message.clone()))
                }
                AgentEvent::TurnStarted { .. }
                | AgentEvent::ToolCallStarted { .. }
                | AgentEvent::ToolResult(_)
                | AgentEvent::TurnFinished { .. } => None,
            })
            .collect()
    }

    fn on_model_completed(&mut self, turn_id: TurnId, assistant: AssistantMessage) {
        if !matches!(
            &self.phase,
            Phase::RunningModel { turn_id: active_turn_id } if *active_turn_id == turn_id
        ) {
            return;
        }

        self.append_event(AgentEvent::AssistantMessage(assistant.clone()));
        self.pending_tool_calls = assistant.tool_calls().cloned().collect();

        if let Some(tool_call) = self.pending_tool_calls.pop_front() {
            self.phase = Phase::RunningTool {
                turn_id,
                tool_call: tool_call.clone(),
            };
            self.append_event(AgentEvent::ToolCallStarted {
                turn_id,
                tool_call: tool_call.clone(),
            });
            self.enqueue_action(AgentAction::RequestTool { turn_id, tool_call });
            return;
        }

        self.phase = Phase::Idle;
        self.append_event(AgentEvent::TurnFinished {
            turn_id,
            outcome: TurnOutcome::Graceful,
        });
        self.drive();
    }

    fn on_tool_completed(&mut self, turn_id: TurnId, result: ToolResultMessage) {
        let running_tool_call = match &self.phase {
            Phase::RunningTool {
                turn_id: active_turn_id,
                tool_call,
            } if *active_turn_id == turn_id => tool_call.clone(),
            _ => return,
        };

        if running_tool_call.id != result.tool_call_id
            || running_tool_call.tool_name != result.tool_name
        {
            return;
        }

        self.append_event(AgentEvent::ToolResult(result.clone()));

        if let Some(tool_call) = self.pending_tool_calls.pop_front() {
            self.phase = Phase::RunningTool {
                turn_id,
                tool_call: tool_call.clone(),
            };
            self.append_event(AgentEvent::ToolCallStarted {
                turn_id,
                tool_call: tool_call.clone(),
            });
            self.enqueue_action(AgentAction::RequestTool { turn_id, tool_call });
            return;
        }

        self.phase = Phase::RunningModel { turn_id };
        self.enqueue_action(AgentAction::RequestModel { turn_id });
    }

    fn drive(&mut self) {
        if self.mailbox.take_interrupt() && self.handle_interrupt() {
            return;
        }

        match &self.phase {
            Phase::Idle | Phase::Interrupted | Phase::Crashed => {
                let Some(input) = self.mailbox.pop_next() else {
                    return;
                };

                self.last_turn_id = self.last_turn_id.next();
                let turn_id = self.last_turn_id;
                let user_message = self.create_user_message(input);
                self.phase = Phase::RunningModel { turn_id };

                self.append_event(AgentEvent::TurnStarted { turn_id });
                self.append_event(AgentEvent::UserMessage(user_message));
                self.enqueue_action(AgentAction::RequestModel { turn_id });
            }
            Phase::RunningModel { .. } | Phase::RunningTool { .. } => {}
        }
    }

    fn handle_interrupt(&mut self) -> bool {
        match self.phase.clone() {
            Phase::Idle | Phase::Interrupted | Phase::Crashed => false,
            Phase::RunningModel { turn_id } => {
                self.phase = Phase::Interrupted;
                self.pending_tool_calls.clear();

                self.append_event(AgentEvent::TurnFinished {
                    turn_id,
                    outcome: TurnOutcome::Interrupted,
                });
                self.enqueue_action(AgentAction::CancelActive { turn_id });
                self.drive();
                true
            }
            Phase::RunningTool { turn_id, tool_call } => {
                self.phase = Phase::Interrupted;
                self.pending_tool_calls.clear();

                let interrupted = ToolResultMessage::interrupted(
                    EventId::take_next(&mut self.next_event_id),
                    tool_call.id,
                    tool_call.tool_name.clone(),
                );
                self.append_event(AgentEvent::ToolResult(interrupted));
                self.append_event(AgentEvent::TurnFinished {
                    turn_id,
                    outcome: TurnOutcome::Interrupted,
                });
                self.enqueue_action(AgentAction::CancelActive { turn_id });
                self.drive();
                true
            }
        }
    }

    fn create_user_message(&mut self, input: UserInput) -> UserMessage {
        UserMessage {
            id: EventId::take_next(&mut self.next_event_id),
            text: input.text,
        }
    }

    fn append_event(&mut self, event: AgentEvent) {
        self.transcript.push(event);
    }

    fn enqueue_action(&mut self, action: AgentAction) {
        self.action_outbox.push_back(action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentAction, AgentEvent, TurnOutcome};
    use crate::message::AssistantItem;

    fn assistant_message(id: EventId, items: Vec<AssistantItem>) -> AssistantMessage {
        AssistantMessage { id, items }
    }

    fn tool_call(loop_state: &mut AgentCoreLoop, name: &str) -> ToolCall {
        ToolCall {
            id: EventId::take_next(&mut loop_state.next_event_id),
            tool_name: name.to_string(),
            args_json: "{}".to_string(),
        }
    }

    fn successful_tool_result(
        loop_state: &mut AgentCoreLoop,
        tool_call_id: EventId,
        tool_name: &str,
    ) -> ToolResultMessage {
        ToolResultMessage {
            id: EventId::take_next(&mut loop_state.next_event_id),
            tool_call_id,
            tool_name: tool_name.to_string(),
            output: "ok".to_string(),
            status: crate::message::ToolResultStatus::Success,
        }
    }

    #[test]
    fn starting_a_turn_appends_boundary_events_and_requests_the_model() {
        let mut loop_state = AgentCoreLoop::new();

        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));

        assert_eq!(
            loop_state.transcript,
            vec![
                AgentEvent::TurnStarted { turn_id: TurnId(1) },
                AgentEvent::UserMessage(UserMessage {
                    id: EventId(1),
                    text: "hello".to_string(),
                }),
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.phase, Phase::RunningModel { turn_id: TurnId(1) });
    }

    #[test]
    fn model_completion_with_a_tool_call_appends_assistant_and_starts_the_tool() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut loop_state, "bash");
        let assistant = assistant_message(
            EventId::take_next(&mut loop_state.next_event_id),
            vec![
                AssistantItem::Text("Let me inspect that.".to_string()),
                AssistantItem::ToolCall(tool_call.clone()),
            ],
        );

        loop_state.on_input(AgentInput::Notification(
            AgentNotification::ModelCompleted {
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            },
        ));

        assert_eq!(
            loop_state.transcript,
            vec![
                AgentEvent::TurnStarted { turn_id: TurnId(1) },
                AgentEvent::UserMessage(UserMessage {
                    id: EventId(1),
                    text: "hello".to_string(),
                }),
                AgentEvent::AssistantMessage(assistant.clone()),
                AgentEvent::ToolCallStarted {
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
            loop_state.phase,
            Phase::RunningTool {
                turn_id: TurnId(1),
                tool_call,
            }
        );
    }

    #[test]
    fn tool_completion_appends_a_result_and_resumes_the_model() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut loop_state, "bash");
        let assistant = assistant_message(
            EventId::take_next(&mut loop_state.next_event_id),
            vec![AssistantItem::ToolCall(tool_call.clone())],
        );
        loop_state.on_input(AgentInput::Notification(
            AgentNotification::ModelCompleted {
                turn_id: TurnId(1),
                assistant,
            },
        ));
        loop_state.drain_actions();

        let result = successful_tool_result(&mut loop_state, tool_call.id, "bash");
        loop_state.on_input(AgentInput::Notification(AgentNotification::ToolCompleted {
            turn_id: TurnId(1),
            result: result.clone(),
        }));

        assert_eq!(
            loop_state.transcript.last(),
            Some(&AgentEvent::ToolResult(result))
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.phase, Phase::RunningModel { turn_id: TurnId(1) });
    }

    #[test]
    fn multiple_tool_calls_run_before_the_model_resumes() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        loop_state.drain_actions();

        let first = tool_call(&mut loop_state, "bash");
        let second = tool_call(&mut loop_state, "read");
        let assistant = assistant_message(
            EventId::take_next(&mut loop_state.next_event_id),
            vec![
                AssistantItem::ToolCall(first.clone()),
                AssistantItem::ToolCall(second.clone()),
            ],
        );
        loop_state.on_input(AgentInput::Notification(
            AgentNotification::ModelCompleted {
                turn_id: TurnId(1),
                assistant,
            },
        ));
        loop_state.drain_actions();

        let first_result = successful_tool_result(&mut loop_state, first.id, "bash");
        loop_state.on_input(AgentInput::Notification(AgentNotification::ToolCompleted {
            turn_id: TurnId(1),
            result: first_result.clone(),
        }));

        assert_eq!(
            loop_state.transcript.last(),
            Some(&AgentEvent::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: second.clone(),
            })
        );
        assert_eq!(
            loop_state.transcript[4],
            AgentEvent::ToolResult(first_result)
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestTool {
                turn_id: TurnId(1),
                tool_call: second.clone(),
            }]
        );
        assert_eq!(
            loop_state.phase,
            Phase::RunningTool {
                turn_id: TurnId(1),
                tool_call: second,
            }
        );
    }

    #[test]
    fn interrupting_a_running_tool_closes_the_turn_and_starts_queued_steer_work() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("initial"),
        )));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut loop_state, "bash");
        let assistant = assistant_message(
            EventId::take_next(&mut loop_state.next_event_id),
            vec![AssistantItem::ToolCall(tool_call.clone())],
        );
        loop_state.on_input(AgentInput::Notification(
            AgentNotification::ModelCompleted {
                turn_id: TurnId(1),
                assistant,
            },
        ));
        loop_state.drain_actions();

        loop_state.on_input(AgentInput::Command(MailboxCommand::Steer(UserInput::from(
            "urgent",
        ))));

        assert!(loop_state.drain_actions().is_empty());

        loop_state.on_input(AgentInput::Command(MailboxCommand::Interrupt));

        assert_eq!(
            loop_state.transcript,
            vec![
                AgentEvent::TurnStarted { turn_id: TurnId(1) },
                AgentEvent::UserMessage(UserMessage {
                    id: EventId(1),
                    text: "initial".to_string(),
                }),
                AgentEvent::AssistantMessage(assistant_message(
                    EventId(3),
                    vec![AssistantItem::ToolCall(tool_call.clone())],
                )),
                AgentEvent::ToolCallStarted {
                    turn_id: TurnId(1),
                    tool_call: tool_call.clone(),
                },
                AgentEvent::ToolResult(ToolResultMessage::interrupted(
                    EventId(4),
                    tool_call.id,
                    "bash",
                )),
                AgentEvent::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Interrupted,
                },
                AgentEvent::TurnStarted { turn_id: TurnId(2) },
                AgentEvent::UserMessage(UserMessage {
                    id: EventId(5),
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
        assert_eq!(loop_state.phase, Phase::RunningModel { turn_id: TurnId(2) });
    }

    #[test]
    fn interrupting_a_running_model_without_queued_work_finishes_interrupted() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        loop_state.drain_actions();

        loop_state.on_input(AgentInput::Command(MailboxCommand::Interrupt));

        assert_eq!(
            loop_state.transcript,
            vec![
                AgentEvent::TurnStarted { turn_id: TurnId(1) },
                AgentEvent::UserMessage(UserMessage {
                    id: EventId(1),
                    text: "hello".to_string(),
                }),
                AgentEvent::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Interrupted,
                },
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::CancelActive { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.phase, Phase::Interrupted);
    }

    #[test]
    fn stale_completions_are_ignored_after_an_interrupt() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        loop_state.drain_actions();
        loop_state.on_input(AgentInput::Command(MailboxCommand::Interrupt));
        loop_state.drain_actions();

        let stale_assistant = assistant_message(
            EventId::take_next(&mut loop_state.next_event_id),
            vec![AssistantItem::Text("stale".to_string())],
        );
        loop_state.on_input(AgentInput::Notification(
            AgentNotification::ModelCompleted {
                turn_id: TurnId(1),
                assistant: stale_assistant,
            },
        ));

        assert_eq!(loop_state.transcript.len(), 3);
        assert!(loop_state.drain_actions().is_empty());
        assert_eq!(loop_state.phase, Phase::Interrupted);
    }

    #[test]
    fn compact_transcript_filters_to_user_and_assistant_messages() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        loop_state.drain_actions();

        let assistant = assistant_message(
            EventId::take_next(&mut loop_state.next_event_id),
            vec![AssistantItem::Text("hi".to_string())],
        );
        loop_state.on_input(AgentInput::Notification(
            AgentNotification::ModelCompleted {
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            },
        ));

        assert_eq!(
            loop_state.compact_transcript(),
            vec![
                CompactMessage::User(UserMessage {
                    id: EventId(1),
                    text: "hello".to_string(),
                }),
                CompactMessage::Assistant(assistant),
            ]
        );
    }

    #[test]
    fn rehydrating_an_incomplete_transcript_patches_a_crashed_finish() {
        let transcript = vec![
            AgentEvent::TurnStarted { turn_id: TurnId(7) },
            AgentEvent::UserMessage(UserMessage {
                id: EventId(1),
                text: "hello".to_string(),
            }),
        ];

        let loop_state = AgentCoreLoop::from_transcript(transcript);

        assert_eq!(
            loop_state.transcript,
            vec![
                AgentEvent::TurnStarted { turn_id: TurnId(7) },
                AgentEvent::UserMessage(UserMessage {
                    id: EventId(1),
                    text: "hello".to_string(),
                }),
                AgentEvent::TurnFinished {
                    turn_id: TurnId(7),
                    outcome: TurnOutcome::Crashed,
                },
            ]
        );
        assert_eq!(loop_state.phase, Phase::Crashed);
        assert_eq!(loop_state.last_turn_id, TurnId(7));
    }

    #[test]
    fn rehydrating_a_graceful_boundary_restores_idle_state() {
        let transcript = vec![
            AgentEvent::TurnStarted { turn_id: TurnId(2) },
            AgentEvent::UserMessage(UserMessage {
                id: EventId(3),
                text: "hello".to_string(),
            }),
            AgentEvent::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ];

        let loop_state = AgentCoreLoop::from_transcript(transcript.clone());

        assert_eq!(loop_state.transcript, transcript);
        assert_eq!(loop_state.phase, Phase::Idle);
        assert_eq!(loop_state.last_turn_id, TurnId(2));
    }
}
