use std::collections::VecDeque;

use crate::event::LoopAction;
use crate::ids::{Epoch, MessageId, ToolCallId};
use crate::mailbox::{Mailbox, MailboxCommand};
use crate::message::{
    AssistantMessage, CoreMessage, ToolCall, ToolResultMessage, ToolResultStatus, UserInput,
    UserMessage,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CoreTransition {
    pub actions: Vec<LoopAction>,
}

impl CoreTransition {
    fn extend(&mut self, other: CoreTransition) {
        self.actions.extend(other.actions);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    RunningModel {
        epoch: Epoch,
    },
    RunningTool {
        epoch: Epoch,
        call_id: ToolCallId,
        tool_name: String,
    },
}

impl Default for Phase {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopSignal {
    Mailbox(MailboxCommand),
    ModelCompleted {
        epoch: Epoch,
        assistant: AssistantMessage,
    },
    ToolCompleted {
        epoch: Epoch,
        result: ToolResultMessage,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCoreLoop {
    pub mailbox: Mailbox,
    pub transcript: Vec<CoreMessage>,
    pub phase: Phase,
    pub epoch: Epoch,
    pending_tool_calls: VecDeque<ToolCall>,
    next_message_id: u64,
    next_tool_call_id: u64,
}

impl Default for AgentCoreLoop {
    fn default() -> Self {
        Self {
            mailbox: Mailbox::default(),
            transcript: Vec::new(),
            phase: Phase::Idle,
            epoch: Epoch::default(),
            pending_tool_calls: VecDeque::new(),
            next_message_id: 1,
            next_tool_call_id: 1,
        }
    }
}

impl AgentCoreLoop {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn alloc_message_id(&mut self) -> MessageId {
        let id = MessageId(self.next_message_id);
        self.next_message_id += 1;
        id
    }

    pub fn alloc_tool_call_id(&mut self) -> ToolCallId {
        let id = ToolCallId(self.next_tool_call_id);
        self.next_tool_call_id += 1;
        id
    }

    pub fn on_signal(&mut self, signal: LoopSignal) -> CoreTransition {
        match signal {
            LoopSignal::Mailbox(command) => {
                self.mailbox.push(command);
                self.drive()
            }
            LoopSignal::ModelCompleted { epoch, assistant } => {
                self.on_model_completed(epoch, assistant)
            }
            LoopSignal::ToolCompleted { epoch, result } => self.on_tool_completed(epoch, result),
        }
    }

    fn on_model_completed(&mut self, epoch: Epoch, assistant: AssistantMessage) -> CoreTransition {
        if !matches!(
            &self.phase,
            Phase::RunningModel { epoch: active_epoch } if *active_epoch == epoch
        ) {
            return CoreTransition::default();
        }

        self.transcript
            .push(CoreMessage::Assistant(assistant.clone()));
        self.pending_tool_calls = assistant.tool_calls().cloned().collect();

        if let Some(tool_call) = self.pending_tool_calls.pop_front() {
            self.phase = Phase::RunningTool {
                epoch,
                call_id: tool_call.call_id,
                tool_name: tool_call.tool_name.clone(),
            };
            return CoreTransition {
                actions: vec![
                    LoopAction::ToolCallStarted {
                        epoch,
                        call_id: tool_call.call_id,
                        tool_name: tool_call.tool_name.clone(),
                    },
                    LoopAction::RequestTool { epoch, tool_call },
                ],
            };
        }

        self.phase = Phase::Idle;
        let mut transition = CoreTransition {
            actions: vec![LoopAction::TurnFinished { epoch }],
        };
        transition.extend(self.drive());
        transition
    }

    fn on_tool_completed(&mut self, epoch: Epoch, result: ToolResultMessage) -> CoreTransition {
        let running = match &self.phase {
            Phase::RunningTool {
                epoch: active_epoch,
                call_id,
                tool_name,
            } if *active_epoch == epoch => (*call_id, tool_name.clone()),
            _ => return CoreTransition::default(),
        };

        if running.0 != result.call_id || running.1 != result.tool_name {
            return CoreTransition::default();
        }

        self.transcript
            .push(CoreMessage::ToolResult(result.clone()));

        if let Some(tool_call) = self.pending_tool_calls.pop_front() {
            self.phase = Phase::RunningTool {
                epoch,
                call_id: tool_call.call_id,
                tool_name: tool_call.tool_name.clone(),
            };
            return CoreTransition {
                actions: vec![
                    LoopAction::ToolCallFinished {
                        epoch,
                        call_id: result.call_id,
                        tool_name: result.tool_name,
                        status: result.status,
                    },
                    LoopAction::ToolCallStarted {
                        epoch,
                        call_id: tool_call.call_id,
                        tool_name: tool_call.tool_name.clone(),
                    },
                    LoopAction::RequestTool { epoch, tool_call },
                ],
            };
        }

        self.phase = Phase::RunningModel { epoch };
        CoreTransition {
            actions: vec![
                LoopAction::ToolCallFinished {
                    epoch,
                    call_id: result.call_id,
                    tool_name: result.tool_name,
                    status: result.status,
                },
                LoopAction::RequestModel { epoch },
            ],
        }
    }

    fn drive(&mut self) -> CoreTransition {
        if self.mailbox.take_interrupt() {
            let interrupt_transition = self.handle_interrupt();
            if !interrupt_transition.actions.is_empty() {
                return interrupt_transition;
            }
        }

        match &self.phase {
            Phase::Idle => {
                let Some(input) = self.mailbox.pop_next() else {
                    return CoreTransition::default();
                };

                self.epoch = self.epoch.next();
                let epoch = self.epoch;
                let user_message = self.create_user_message(input);
                self.transcript.push(CoreMessage::User(user_message));
                self.phase = Phase::RunningModel { epoch };

                CoreTransition {
                    actions: vec![
                        LoopAction::TurnStarted { epoch },
                        LoopAction::RequestModel { epoch },
                    ],
                }
            }
            Phase::RunningModel { .. } | Phase::RunningTool { .. } => CoreTransition::default(),
        }
    }

    fn handle_interrupt(&mut self) -> CoreTransition {
        match self.phase.clone() {
            Phase::Idle => CoreTransition::default(),
            Phase::RunningModel { epoch } => {
                self.phase = Phase::Idle;
                self.pending_tool_calls.clear();

                let mut transition = CoreTransition {
                    actions: vec![
                        LoopAction::Interrupted { epoch },
                        LoopAction::CancelActive { epoch },
                    ],
                };
                transition.extend(self.drive());
                transition
            }
            Phase::RunningTool {
                epoch,
                call_id,
                tool_name,
            } => {
                self.phase = Phase::Idle;
                self.pending_tool_calls.clear();
                let message_id = self.alloc_message_id();
                let interrupted =
                    ToolResultMessage::interrupted(message_id, call_id, tool_name.clone());
                self.transcript.push(CoreMessage::ToolResult(interrupted));

                let mut transition = CoreTransition {
                    actions: vec![
                        LoopAction::Interrupted { epoch },
                        LoopAction::ToolCallFinished {
                            epoch,
                            call_id,
                            tool_name,
                            status: ToolResultStatus::Interrupted,
                        },
                        LoopAction::CancelActive { epoch },
                    ],
                };
                transition.extend(self.drive());
                transition
            }
        }
    }

    fn create_user_message(&mut self, input: UserInput) -> UserMessage {
        UserMessage {
            id: self.alloc_message_id(),
            text: input.text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::AssistantItem;
    use crate::message::UserInput;

    fn assistant_message(id: MessageId, items: Vec<AssistantItem>) -> AssistantMessage {
        AssistantMessage { id, items }
    }

    fn text_assistant(loop_state: &mut AgentCoreLoop, text: &str) -> AssistantMessage {
        assistant_message(
            loop_state.alloc_message_id(),
            vec![AssistantItem::Text(text.to_string())],
        )
    }

    fn tool_call(loop_state: &mut AgentCoreLoop, name: &str) -> ToolCall {
        ToolCall {
            call_id: loop_state.alloc_tool_call_id(),
            tool_name: name.to_string(),
            args_json: "{}".to_string(),
        }
    }

    fn successful_tool_result(
        loop_state: &mut AgentCoreLoop,
        call_id: ToolCallId,
        tool_name: &str,
    ) -> ToolResultMessage {
        ToolResultMessage {
            id: loop_state.alloc_message_id(),
            call_id,
            tool_name: tool_name.to_string(),
            output: "ok".to_string(),
            status: ToolResultStatus::Success,
        }
    }

    #[test]
    fn starting_a_turn_commits_the_user_message_and_requests_the_model() {
        let mut loop_state = AgentCoreLoop::new();

        let transition = loop_state.on_signal(LoopSignal::Mailbox(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));

        assert_eq!(
            transition.actions,
            vec![
                LoopAction::TurnStarted { epoch: Epoch(1) },
                LoopAction::RequestModel { epoch: Epoch(1) },
            ]
        );
        assert_eq!(
            loop_state.transcript,
            vec![CoreMessage::User(UserMessage {
                id: MessageId(1),
                text: "hello".to_string(),
            })]
        );
        assert_eq!(loop_state.phase, Phase::RunningModel { epoch: Epoch(1) });
    }

    #[test]
    fn model_completion_with_a_tool_call_requests_the_tool() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_signal(LoopSignal::Mailbox(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        let tool_call = tool_call(&mut loop_state, "bash");
        let assistant = assistant_message(
            loop_state.alloc_message_id(),
            vec![
                AssistantItem::Text("Let me inspect that.".to_string()),
                AssistantItem::ToolCall(tool_call.clone()),
            ],
        );

        let transition = loop_state.on_signal(LoopSignal::ModelCompleted {
            epoch: Epoch(1),
            assistant: assistant.clone(),
        });

        assert_eq!(
            transition.actions,
            vec![
                LoopAction::ToolCallStarted {
                    epoch: Epoch(1),
                    call_id: tool_call.call_id,
                    tool_name: "bash".to_string(),
                },
                LoopAction::RequestTool {
                    epoch: Epoch(1),
                    tool_call: tool_call.clone(),
                },
            ]
        );
        assert_eq!(
            loop_state.phase,
            Phase::RunningTool {
                epoch: Epoch(1),
                call_id: tool_call.call_id,
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
        loop_state.on_signal(LoopSignal::Mailbox(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        let tool_call = tool_call(&mut loop_state, "bash");
        let assistant = assistant_message(
            loop_state.alloc_message_id(),
            vec![AssistantItem::ToolCall(tool_call.clone())],
        );
        loop_state.on_signal(LoopSignal::ModelCompleted {
            epoch: Epoch(1),
            assistant,
        });
        let result = successful_tool_result(&mut loop_state, tool_call.call_id, "bash");

        let transition = loop_state.on_signal(LoopSignal::ToolCompleted {
            epoch: Epoch(1),
            result: result.clone(),
        });

        assert_eq!(
            transition.actions,
            vec![
                LoopAction::ToolCallFinished {
                    epoch: Epoch(1),
                    call_id: tool_call.call_id,
                    tool_name: "bash".to_string(),
                    status: ToolResultStatus::Success,
                },
                LoopAction::RequestModel { epoch: Epoch(1) },
            ]
        );
        assert_eq!(loop_state.phase, Phase::RunningModel { epoch: Epoch(1) });
        assert_eq!(
            loop_state.transcript.last(),
            Some(&CoreMessage::ToolResult(result))
        );
    }

    #[test]
    fn multiple_tool_calls_run_before_the_model_resumes() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_signal(LoopSignal::Mailbox(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        let first = tool_call(&mut loop_state, "bash");
        let second = tool_call(&mut loop_state, "read");
        let assistant = assistant_message(
            loop_state.alloc_message_id(),
            vec![
                AssistantItem::ToolCall(first.clone()),
                AssistantItem::ToolCall(second.clone()),
            ],
        );
        loop_state.on_signal(LoopSignal::ModelCompleted {
            epoch: Epoch(1),
            assistant,
        });

        let first_result = successful_tool_result(&mut loop_state, first.call_id, "bash");
        let transition = loop_state.on_signal(LoopSignal::ToolCompleted {
            epoch: Epoch(1),
            result: first_result,
        });

        assert_eq!(
            transition.actions,
            vec![
                LoopAction::ToolCallFinished {
                    epoch: Epoch(1),
                    call_id: first.call_id,
                    tool_name: "bash".to_string(),
                    status: ToolResultStatus::Success,
                },
                LoopAction::ToolCallStarted {
                    epoch: Epoch(1),
                    call_id: second.call_id,
                    tool_name: "read".to_string(),
                },
                LoopAction::RequestTool {
                    epoch: Epoch(1),
                    tool_call: second.clone(),
                },
            ]
        );
        assert_eq!(
            loop_state.phase,
            Phase::RunningTool {
                epoch: Epoch(1),
                call_id: second.call_id,
                tool_name: "read".to_string(),
            }
        );
    }

    #[test]
    fn interrupting_a_running_tool_closes_the_transcript_and_starts_queued_steering_work() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_signal(LoopSignal::Mailbox(MailboxCommand::FollowUp(
            UserInput::from("initial"),
        )));
        let tool_call = tool_call(&mut loop_state, "bash");
        let assistant = assistant_message(
            loop_state.alloc_message_id(),
            vec![AssistantItem::ToolCall(tool_call.clone())],
        );
        loop_state.on_signal(LoopSignal::ModelCompleted {
            epoch: Epoch(1),
            assistant,
        });
        loop_state.on_signal(LoopSignal::Mailbox(MailboxCommand::Steer(UserInput::from(
            "urgent",
        ))));

        let transition = loop_state.on_signal(LoopSignal::Mailbox(MailboxCommand::Interrupt));

        assert_eq!(
            transition.actions,
            vec![
                LoopAction::Interrupted { epoch: Epoch(1) },
                LoopAction::ToolCallFinished {
                    epoch: Epoch(1),
                    call_id: tool_call.call_id,
                    tool_name: "bash".to_string(),
                    status: ToolResultStatus::Interrupted,
                },
                LoopAction::CancelActive { epoch: Epoch(1) },
                LoopAction::TurnStarted { epoch: Epoch(2) },
                LoopAction::RequestModel { epoch: Epoch(2) },
            ]
        );
        assert_eq!(loop_state.phase, Phase::RunningModel { epoch: Epoch(2) });
        assert_eq!(
            loop_state.transcript,
            vec![
                CoreMessage::User(UserMessage {
                    id: MessageId(1),
                    text: "initial".to_string(),
                }),
                CoreMessage::Assistant(assistant_message(
                    MessageId(2),
                    vec![AssistantItem::ToolCall(tool_call.clone())],
                )),
                CoreMessage::ToolResult(ToolResultMessage::interrupted(
                    MessageId(3),
                    tool_call.call_id,
                    "bash",
                )),
                CoreMessage::User(UserMessage {
                    id: MessageId(4),
                    text: "urgent".to_string(),
                }),
            ]
        );
    }

    #[test]
    fn stale_model_completion_is_ignored_after_an_interrupt() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_signal(LoopSignal::Mailbox(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        loop_state.on_signal(LoopSignal::Mailbox(MailboxCommand::Interrupt));

        let stale_assistant = text_assistant(&mut loop_state, "stale");
        let transition = loop_state.on_signal(LoopSignal::ModelCompleted {
            epoch: Epoch(1),
            assistant: stale_assistant,
        });

        assert_eq!(transition, CoreTransition::default());
        assert_eq!(loop_state.transcript.len(), 1);
        assert_eq!(loop_state.phase, Phase::Idle);
    }

    #[test]
    fn stale_tool_completion_is_ignored_once_the_active_call_changes() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_signal(LoopSignal::Mailbox(MailboxCommand::FollowUp(
            UserInput::from("hello"),
        )));
        let first = tool_call(&mut loop_state, "bash");
        let second = tool_call(&mut loop_state, "read");
        let assistant = assistant_message(
            loop_state.alloc_message_id(),
            vec![
                AssistantItem::ToolCall(first.clone()),
                AssistantItem::ToolCall(second.clone()),
            ],
        );
        loop_state.on_signal(LoopSignal::ModelCompleted {
            epoch: Epoch(1),
            assistant,
        });
        let first_result = successful_tool_result(&mut loop_state, first.call_id, "bash");
        loop_state.on_signal(LoopSignal::ToolCompleted {
            epoch: Epoch(1),
            result: first_result,
        });

        let stale_result = successful_tool_result(&mut loop_state, first.call_id, "bash");
        let transition = loop_state.on_signal(LoopSignal::ToolCompleted {
            epoch: Epoch(1),
            result: stale_result,
        });

        assert_eq!(transition, CoreTransition::default());
        assert_eq!(
            loop_state.phase,
            Phase::RunningTool {
                epoch: Epoch(1),
                call_id: second.call_id,
                tool_name: "read".to_string(),
            }
        );
    }
}
