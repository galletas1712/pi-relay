use std::collections::VecDeque;

use crate::event::{AgentAction, AgentEvent};
use crate::ids::{EventId, TurnId};
use crate::mailbox::{Mailbox, MailboxCommand};
use crate::message::{
    AssistantMessage, CoreMessage, ToolCall, ToolResultMessage, ToolResultStatus, UserInput,
    UserMessage,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CoreTransition {
    pub events: Vec<AgentEvent>,
    pub actions: Vec<AgentAction>,
}

impl CoreTransition {
    fn extend(&mut self, other: CoreTransition) {
        self.events.extend(other.events);
        self.actions.extend(other.actions);
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty() && self.actions.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    // The last turn ended via interrupt; the next queued input starts a fresh turn.
    Interrupted,
    RunningModel {
        turn_id: TurnId,
    },
    RunningTool {
        turn_id: TurnId,
        tool_call_id: EventId,
        tool_name: String,
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
    pub transcript: Vec<CoreMessage>,
    pub phase: Phase,
    pub last_turn_id: TurnId,
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
            pending_tool_calls: VecDeque::new(),
            next_event_id: EventId::first(),
        }
    }
}

impl AgentCoreLoop {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_input(&mut self, input: AgentInput) -> CoreTransition {
        match input {
            AgentInput::Command(command) => {
                self.mailbox.push(command);
                self.drive()
            }
            AgentInput::Notification(notification) => match notification {
                AgentNotification::ModelCompleted { turn_id, assistant } => {
                    self.on_model_completed(turn_id, assistant)
                }
                AgentNotification::ToolCompleted { turn_id, result } => {
                    self.on_tool_completed(turn_id, result)
                }
            },
        }
    }

    fn on_model_completed(
        &mut self,
        turn_id: TurnId,
        assistant: AssistantMessage,
    ) -> CoreTransition {
        if !matches!(
            &self.phase,
            Phase::RunningModel { turn_id: active_turn_id } if *active_turn_id == turn_id
        ) {
            return CoreTransition::default();
        }

        self.transcript
            .push(CoreMessage::Assistant(assistant.clone()));
        self.pending_tool_calls = assistant.tool_calls().cloned().collect();

        if let Some(tool_call) = self.pending_tool_calls.pop_front() {
            self.phase = Phase::RunningTool {
                turn_id,
                tool_call_id: tool_call.id,
                tool_name: tool_call.tool_name.clone(),
            };
            return CoreTransition {
                events: vec![AgentEvent::ToolCallStarted {
                    turn_id,
                    tool_call_id: tool_call.id,
                    tool_name: tool_call.tool_name.clone(),
                }],
                actions: vec![AgentAction::RequestTool { turn_id, tool_call }],
            };
        }

        self.phase = Phase::Idle;
        let mut transition = CoreTransition {
            events: vec![AgentEvent::TurnFinished { turn_id }],
            actions: Vec::new(),
        };
        transition.extend(self.drive());
        transition
    }

    fn on_tool_completed(&mut self, turn_id: TurnId, result: ToolResultMessage) -> CoreTransition {
        let running = match &self.phase {
            Phase::RunningTool {
                turn_id: active_turn_id,
                tool_call_id,
                tool_name,
            } if *active_turn_id == turn_id => (*tool_call_id, tool_name.clone()),
            _ => return CoreTransition::default(),
        };

        if running.0 != result.tool_call_id || running.1 != result.tool_name {
            return CoreTransition::default();
        }

        self.transcript
            .push(CoreMessage::ToolResult(result.clone()));

        if let Some(tool_call) = self.pending_tool_calls.pop_front() {
            self.phase = Phase::RunningTool {
                turn_id,
                tool_call_id: tool_call.id,
                tool_name: tool_call.tool_name.clone(),
            };
            return CoreTransition {
                events: vec![
                    AgentEvent::ToolCallFinished {
                        turn_id,
                        tool_call_id: result.tool_call_id,
                        tool_name: result.tool_name,
                        status: result.status,
                    },
                    AgentEvent::ToolCallStarted {
                        turn_id,
                        tool_call_id: tool_call.id,
                        tool_name: tool_call.tool_name.clone(),
                    },
                ],
                actions: vec![AgentAction::RequestTool { turn_id, tool_call }],
            };
        }

        self.phase = Phase::RunningModel { turn_id };
        CoreTransition {
            events: vec![AgentEvent::ToolCallFinished {
                turn_id,
                tool_call_id: result.tool_call_id,
                tool_name: result.tool_name,
                status: result.status,
            }],
            actions: vec![AgentAction::RequestModel { turn_id }],
        }
    }

    fn drive(&mut self) -> CoreTransition {
        if self.mailbox.take_interrupt() {
            let interrupt_transition = self.handle_interrupt();
            if !interrupt_transition.is_empty() {
                return interrupt_transition;
            }
        }

        match &self.phase {
            Phase::Idle | Phase::Interrupted => {
                let Some(input) = self.mailbox.pop_next() else {
                    return CoreTransition::default();
                };

                self.last_turn_id = self.last_turn_id.next();
                let turn_id = self.last_turn_id;
                let user_message = self.create_user_message(input);
                self.transcript.push(CoreMessage::User(user_message));
                self.phase = Phase::RunningModel { turn_id };

                CoreTransition {
                    events: vec![AgentEvent::TurnStarted { turn_id }],
                    actions: vec![AgentAction::RequestModel { turn_id }],
                }
            }
            Phase::RunningModel { .. } | Phase::RunningTool { .. } => CoreTransition::default(),
        }
    }

    fn handle_interrupt(&mut self) -> CoreTransition {
        match self.phase.clone() {
            Phase::Idle | Phase::Interrupted => CoreTransition::default(),
            Phase::RunningModel { turn_id } => {
                self.phase = Phase::Interrupted;
                self.pending_tool_calls.clear();

                let mut transition = CoreTransition {
                    events: vec![AgentEvent::Interrupted { turn_id }],
                    actions: vec![AgentAction::CancelActive { turn_id }],
                };
                transition.extend(self.drive());
                transition
            }
            Phase::RunningTool {
                turn_id,
                tool_call_id,
                tool_name,
            } => {
                self.phase = Phase::Interrupted;
                self.pending_tool_calls.clear();
                let message_id = EventId::take_next(&mut self.next_event_id);
                let interrupted =
                    ToolResultMessage::interrupted(message_id, tool_call_id, tool_name.clone());
                self.transcript.push(CoreMessage::ToolResult(interrupted));

                let mut transition = CoreTransition {
                    events: vec![
                        AgentEvent::Interrupted { turn_id },
                        AgentEvent::ToolCallFinished {
                            turn_id,
                            tool_call_id,
                            tool_name,
                            status: ToolResultStatus::Interrupted,
                        },
                    ],
                    actions: vec![AgentAction::CancelActive { turn_id }],
                };
                transition.extend(self.drive());
                transition
            }
        }
    }

    fn create_user_message(&mut self, input: UserInput) -> UserMessage {
        UserMessage {
            id: EventId::take_next(&mut self.next_event_id),
            text: input.text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentAction, AgentEvent};
    use crate::message::AssistantItem;
    use crate::message::UserInput;

    fn assistant_message(id: EventId, items: Vec<AssistantItem>) -> AssistantMessage {
        AssistantMessage { id, items }
    }

    fn text_assistant(loop_state: &mut AgentCoreLoop, text: &str) -> AssistantMessage {
        assistant_message(
            EventId::take_next(&mut loop_state.next_event_id),
            vec![AssistantItem::Text(text.to_string())],
        )
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
            status: ToolResultStatus::Success,
        }
    }

    #[test]
    fn starting_a_turn_commits_the_user_message_and_requests_the_model() {
        let mut loop_state = AgentCoreLoop::new();

        let transition = loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));

        assert_eq!(
            transition.events,
            vec![AgentEvent::TurnStarted { turn_id: TurnId(1) }]
        );
        assert_eq!(
            transition.actions,
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );
        assert_eq!(
            loop_state.transcript,
            vec![CoreMessage::User(UserMessage {
                id: EventId(1),
                text: "hello".to_string(),
            })]
        );
        assert_eq!(loop_state.phase, Phase::RunningModel { turn_id: TurnId(1) });
    }

    #[test]
    fn model_completion_with_a_tool_call_requests_the_tool() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        let tool_call = tool_call(&mut loop_state, "bash");
        let assistant = assistant_message(
            EventId::take_next(&mut loop_state.next_event_id),
            vec![
                AssistantItem::Text("Let me inspect that.".to_string()),
                AssistantItem::ToolCall(tool_call.clone()),
            ],
        );

        let transition = loop_state.on_input(AgentInput::Notification(
            AgentNotification::ModelCompleted {
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            },
        ));

        assert_eq!(
            transition.events,
            vec![AgentEvent::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call_id: tool_call.id,
                tool_name: "bash".to_string(),
            }]
        );
        assert_eq!(
            transition.actions,
            vec![AgentAction::RequestTool {
                turn_id: TurnId(1),
                tool_call: tool_call.clone(),
            }]
        );
        assert_eq!(
            loop_state.phase,
            Phase::RunningTool {
                turn_id: TurnId(1),
                tool_call_id: tool_call.id,
                tool_name: "bash".to_string(),
            }
        );
        assert_eq!(
            loop_state.transcript.last(),
            Some(&CoreMessage::Assistant(assistant))
        );
    }

    #[test]
    fn tool_completion_appends_a_tool_result_and_resumes_the_model() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
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
        let result = successful_tool_result(&mut loop_state, tool_call.id, "bash");

        let transition =
            loop_state.on_input(AgentInput::Notification(AgentNotification::ToolCompleted {
                turn_id: TurnId(1),
                result: result.clone(),
            }));

        assert_eq!(
            transition.events,
            vec![AgentEvent::ToolCallFinished {
                turn_id: TurnId(1),
                tool_call_id: tool_call.id,
                tool_name: "bash".to_string(),
                status: ToolResultStatus::Success,
            }]
        );
        assert_eq!(
            transition.actions,
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.phase, Phase::RunningModel { turn_id: TurnId(1) });
        assert_eq!(
            loop_state.transcript.last(),
            Some(&CoreMessage::ToolResult(result))
        );
    }

    #[test]
    fn multiple_tool_calls_run_before_the_model_resumes() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
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

        let first_result = successful_tool_result(&mut loop_state, first.id, "bash");
        let transition =
            loop_state.on_input(AgentInput::Notification(AgentNotification::ToolCompleted {
                turn_id: TurnId(1),
                result: first_result,
            }));

        assert_eq!(
            transition.events,
            vec![
                AgentEvent::ToolCallFinished {
                    turn_id: TurnId(1),
                    tool_call_id: first.id,
                    tool_name: "bash".to_string(),
                    status: ToolResultStatus::Success,
                },
                AgentEvent::ToolCallStarted {
                    turn_id: TurnId(1),
                    tool_call_id: second.id,
                    tool_name: "read".to_string(),
                },
            ]
        );
        assert_eq!(
            transition.actions,
            vec![AgentAction::RequestTool {
                turn_id: TurnId(1),
                tool_call: second.clone(),
            }]
        );
        assert_eq!(
            loop_state.phase,
            Phase::RunningTool {
                turn_id: TurnId(1),
                tool_call_id: second.id,
                tool_name: "read".to_string(),
            }
        );
    }

    #[test]
    fn interrupting_a_running_tool_closes_the_transcript_and_starts_queued_steering_work() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("initial"),
        )));
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
        loop_state.on_input(AgentInput::Command(MailboxCommand::Steer(UserInput::from(
            "urgent",
        ))));

        let transition = loop_state.on_input(AgentInput::Command(MailboxCommand::Interrupt));

        assert_eq!(
            transition.events,
            vec![
                AgentEvent::Interrupted { turn_id: TurnId(1) },
                AgentEvent::ToolCallFinished {
                    turn_id: TurnId(1),
                    tool_call_id: tool_call.id,
                    tool_name: "bash".to_string(),
                    status: ToolResultStatus::Interrupted,
                },
                AgentEvent::TurnStarted { turn_id: TurnId(2) },
            ]
        );
        assert_eq!(
            transition.actions,
            vec![
                AgentAction::CancelActive { turn_id: TurnId(1) },
                AgentAction::RequestModel { turn_id: TurnId(2) },
            ]
        );
        assert_eq!(loop_state.phase, Phase::RunningModel { turn_id: TurnId(2) });
        assert_eq!(
            loop_state.transcript,
            vec![
                CoreMessage::User(UserMessage {
                    id: EventId(1),
                    text: "initial".to_string(),
                }),
                CoreMessage::Assistant(assistant_message(
                    EventId(3),
                    vec![AssistantItem::ToolCall(tool_call.clone())],
                )),
                CoreMessage::ToolResult(ToolResultMessage::interrupted(
                    EventId(4),
                    tool_call.id,
                    "bash",
                )),
                CoreMessage::User(UserMessage {
                    id: EventId(5),
                    text: "urgent".to_string(),
                }),
            ]
        );
    }

    #[test]
    fn interrupting_a_running_model_without_queued_work_leaves_interrupted_phase() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));

        let transition = loop_state.on_input(AgentInput::Command(MailboxCommand::Interrupt));

        assert_eq!(
            transition.events,
            vec![AgentEvent::Interrupted { turn_id: TurnId(1) }]
        );
        assert_eq!(
            transition.actions,
            vec![AgentAction::CancelActive { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.phase, Phase::Interrupted);
    }

    #[test]
    fn stale_model_completion_is_ignored_after_an_interrupt() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        loop_state.on_input(AgentInput::Command(MailboxCommand::Interrupt));

        let stale_assistant = text_assistant(&mut loop_state, "stale");
        let transition = loop_state.on_input(AgentInput::Notification(
            AgentNotification::ModelCompleted {
                turn_id: TurnId(1),
                assistant: stale_assistant,
            },
        ));

        assert_eq!(transition, CoreTransition::default());
        assert_eq!(loop_state.transcript.len(), 1);
        assert_eq!(loop_state.phase, Phase::Interrupted);
    }

    #[test]
    fn stale_tool_completion_is_ignored_once_the_active_call_changes() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::Command(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
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
        let first_result = successful_tool_result(&mut loop_state, first.id, "bash");
        loop_state.on_input(AgentInput::Notification(AgentNotification::ToolCompleted {
            turn_id: TurnId(1),
            result: first_result,
        }));

        let stale_result = successful_tool_result(&mut loop_state, first.id, "bash");
        let transition =
            loop_state.on_input(AgentInput::Notification(AgentNotification::ToolCompleted {
                turn_id: TurnId(1),
                result: stale_result,
            }));

        assert_eq!(transition, CoreTransition::default());
        assert_eq!(
            loop_state.phase,
            Phase::RunningTool {
                turn_id: TurnId(1),
                tool_call_id: second.id,
                tool_name: "read".to_string(),
            }
        );
    }
}
