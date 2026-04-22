use crate::action::AgentAction;
use crate::event::AgentEvent;
use crate::ids::TurnId;
use crate::message::{ToolCall, UserMessage};
use crate::transcript_record::{TranscriptRecord, TurnOutcome};

// Live control state only. Durable session history lives in Transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
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
    // Internal transition point after a tool result. The next step is either
    // another queued tool call or a model request.
    ReadyToContinue {
        turn_id: TurnId,
    },
}

impl Default for AgentState {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct AgentTransition {
    pub(crate) records: Vec<TranscriptRecord>,
    pub(crate) actions: Vec<AgentAction>,
    pub(crate) queued_tool_calls: Vec<ToolCall>,
    pub(crate) clear_tool_calls: bool,
}

impl AgentTransition {
    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty()
            && self.actions.is_empty()
            && self.queued_tool_calls.is_empty()
            && !self.clear_tool_calls
    }
}

impl AgentState {
    pub(crate) fn from_tail_outcome(outcome: Option<TurnOutcome>) -> Self {
        match outcome {
            Some(TurnOutcome::Interrupted) => Self::Interrupted,
            Some(TurnOutcome::Crashed) => Self::Crashed,
            Some(TurnOutcome::Graceful) | None => Self::Idle,
        }
    }

    pub(crate) fn step(&mut self, event: AgentEvent) -> AgentTransition {
        match event {
            AgentEvent::Interrupt => self.on_interrupt(),
            AgentEvent::StartTurn { turn_id, input } => self.on_start_turn(turn_id, input),
            AgentEvent::ModelCompleted { turn_id, assistant } => {
                self.on_model_completed(turn_id, assistant)
            }
            AgentEvent::ToolReady(tool_call) => self.on_tool_ready(tool_call),
            AgentEvent::ToolCompleted { turn_id, result } => {
                self.on_tool_completed(turn_id, result)
            }
            AgentEvent::ContinueModel => self.on_continue_model(),
        }
    }

    fn on_start_turn(
        &mut self,
        turn_id: TurnId,
        input: crate::message::UserInput,
    ) -> AgentTransition {
        match self {
            Self::Idle | Self::Interrupted | Self::Crashed => {
                *self = Self::RunningModel { turn_id };
                AgentTransition {
                    records: vec![
                        TranscriptRecord::TurnStarted { turn_id },
                        TranscriptRecord::UserMessage(UserMessage { text: input.text }),
                    ],
                    actions: vec![AgentAction::RequestModel { turn_id }],
                    clear_tool_calls: true,
                    ..AgentTransition::default()
                }
            }
            Self::RunningModel { .. } | Self::RunningTool { .. } | Self::ReadyToContinue { .. } => {
                AgentTransition::default()
            }
        }
    }

    fn on_model_completed(
        &mut self,
        turn_id: TurnId,
        assistant: crate::message::AssistantMessage,
    ) -> AgentTransition {
        if !matches!(
            self,
            Self::RunningModel { turn_id: active_turn_id } if *active_turn_id == turn_id
        ) {
            return AgentTransition::default();
        }

        let mut transition = AgentTransition {
            records: vec![TranscriptRecord::AssistantMessage(assistant.clone())],
            ..AgentTransition::default()
        };

        let mut tool_calls = assistant.tool_calls().cloned();
        let Some(first_tool_call) = tool_calls.next() else {
            *self = Self::Idle;
            transition.clear_tool_calls = true;
            transition.records.push(TranscriptRecord::TurnFinished {
                turn_id,
                outcome: TurnOutcome::Graceful,
            });
            return transition;
        };

        *self = Self::RunningTool {
            turn_id,
            tool_call: first_tool_call.clone(),
        };
        transition.records.push(TranscriptRecord::ToolCallStarted {
            turn_id,
            tool_call: first_tool_call.clone(),
        });
        transition.actions.push(AgentAction::RequestTool {
            turn_id,
            tool_call: first_tool_call,
        });
        transition.queued_tool_calls.extend(tool_calls);
        transition
    }

    fn on_tool_ready(&mut self, tool_call: ToolCall) -> AgentTransition {
        let Self::ReadyToContinue { turn_id } = self else {
            return AgentTransition::default();
        };
        let turn_id = *turn_id;

        *self = Self::RunningTool {
            turn_id,
            tool_call: tool_call.clone(),
        };
        AgentTransition {
            records: vec![TranscriptRecord::ToolCallStarted {
                turn_id,
                tool_call: tool_call.clone(),
            }],
            actions: vec![AgentAction::RequestTool { turn_id, tool_call }],
            ..AgentTransition::default()
        }
    }

    fn on_tool_completed(
        &mut self,
        turn_id: TurnId,
        result: crate::message::ToolResultMessage,
    ) -> AgentTransition {
        match self {
            Self::RunningTool {
                turn_id: active_turn_id,
                tool_call,
            } if *active_turn_id == turn_id
                && tool_call.id == result.tool_call_id
                && tool_call.tool_name == result.tool_name =>
            {
                *self = Self::ReadyToContinue { turn_id };
                AgentTransition {
                    records: vec![TranscriptRecord::ToolResult(result)],
                    ..AgentTransition::default()
                }
            }
            _ => AgentTransition::default(),
        }
    }

    fn on_continue_model(&mut self) -> AgentTransition {
        let Self::ReadyToContinue { turn_id } = self else {
            return AgentTransition::default();
        };
        let turn_id = *turn_id;

        *self = Self::RunningModel { turn_id };
        AgentTransition {
            actions: vec![AgentAction::RequestModel { turn_id }],
            ..AgentTransition::default()
        }
    }

    fn on_interrupt(&mut self) -> AgentTransition {
        match self.clone() {
            Self::Idle | Self::Interrupted | Self::Crashed => AgentTransition::default(),
            Self::ReadyToContinue { turn_id } => {
                *self = Self::Interrupted;
                AgentTransition {
                    records: vec![TranscriptRecord::TurnFinished {
                        turn_id,
                        outcome: TurnOutcome::Interrupted,
                    }],
                    clear_tool_calls: true,
                    ..AgentTransition::default()
                }
            }
            Self::RunningModel { turn_id } => {
                *self = Self::Interrupted;
                AgentTransition {
                    records: vec![TranscriptRecord::TurnFinished {
                        turn_id,
                        outcome: TurnOutcome::Interrupted,
                    }],
                    actions: vec![AgentAction::CancelActive { turn_id }],
                    clear_tool_calls: true,
                    ..AgentTransition::default()
                }
            }
            Self::RunningTool { turn_id, tool_call } => {
                *self = Self::Interrupted;
                let interrupted_tool_result = crate::message::ToolResultMessage::interrupted(
                    tool_call.id,
                    tool_call.tool_name,
                );
                AgentTransition {
                    records: vec![
                        TranscriptRecord::ToolResult(interrupted_tool_result),
                        TranscriptRecord::TurnFinished {
                            turn_id,
                            outcome: TurnOutcome::Interrupted,
                        },
                    ],
                    actions: vec![AgentAction::CancelActive { turn_id }],
                    clear_tool_calls: true,
                    ..AgentTransition::default()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ToolCallId;
    use crate::message::{
        AssistantItem, AssistantMessage, ToolResultMessage, ToolResultStatus, UserInput,
    };

    fn tool_call(id: u64, name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId(id),
            tool_name: name.to_string(),
            args_json: "{}".to_string(),
        }
    }

    fn tool_result(id: u64, name: &str) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: ToolCallId(id),
            tool_name: name.to_string(),
            output: "ok".to_string(),
            status: ToolResultStatus::Success,
        }
    }

    #[test]
    fn terminal_states_ignore_late_model_completions() {
        let event = AgentEvent::ModelCompleted {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        };

        let mut idle = AgentState::Idle;
        let mut interrupted = AgentState::Interrupted;
        let mut crashed = AgentState::Crashed;

        assert!(idle.step(event.clone()).is_empty());
        assert!(interrupted.step(event.clone()).is_empty());
        assert!(crashed.step(event).is_empty());
        assert_eq!(idle, AgentState::Idle);
        assert_eq!(interrupted, AgentState::Interrupted);
        assert_eq!(crashed, AgentState::Crashed);
    }

    #[test]
    fn running_model_accepts_only_matching_model_completion() {
        let mut state = AgentState::RunningModel { turn_id: TurnId(2) };
        let assistant = AssistantMessage { items: Vec::new() };
        let matching = AgentEvent::ModelCompleted {
            turn_id: TurnId(2),
            assistant: assistant.clone(),
        };
        let stale = AgentEvent::ModelCompleted {
            turn_id: TurnId(1),
            assistant,
        };

        assert!(AgentState::RunningModel { turn_id: TurnId(2) }
            .step(stale)
            .is_empty());

        let transition = state.step(matching);
        assert_eq!(state, AgentState::Idle);
        assert_eq!(
            transition.records,
            vec![
                TranscriptRecord::AssistantMessage(AssistantMessage { items: Vec::new() }),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(2),
                    outcome: TurnOutcome::Graceful,
                }
            ]
        );
    }

    #[test]
    fn running_tool_accepts_only_matching_tool_completion() {
        let mut state = AgentState::RunningTool {
            turn_id: TurnId(1),
            tool_call: tool_call(1, "bash"),
        };
        let result = AgentEvent::ToolCompleted {
            turn_id: TurnId(1),
            result: tool_result(1, "bash"),
        };
        let wrong_tool = AgentEvent::ToolCompleted {
            turn_id: TurnId(1),
            result: tool_result(1, "read"),
        };

        assert!(state.clone().step(wrong_tool).is_empty());

        let transition = state.step(result);
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
        assert_eq!(
            transition.records,
            vec![TranscriptRecord::ToolResult(tool_result(1, "bash"))]
        );
    }

    #[test]
    fn step_moves_through_turn_tool_and_resume_states() {
        let mut state = AgentState::Idle;
        let first_tool = tool_call(1, "bash");
        let second_tool = tool_call(2, "read");

        let start = state.step(AgentEvent::StartTurn {
            turn_id: TurnId(1),
            input: UserInput::from("hello"),
        });
        assert_eq!(state, AgentState::RunningModel { turn_id: TurnId(1) });
        assert_eq!(
            start.actions,
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );

        let model = state.step(AgentEvent::ModelCompleted {
            turn_id: TurnId(1),
            assistant: AssistantMessage {
                items: vec![
                    AssistantItem::ToolCall(first_tool.clone()),
                    AssistantItem::ToolCall(second_tool.clone()),
                ],
            },
        });
        assert_eq!(
            state,
            AgentState::RunningTool {
                turn_id: TurnId(1),
                tool_call: first_tool.clone()
            }
        );
        assert_eq!(model.queued_tool_calls, vec![second_tool.clone()]);

        let tool = state.step(AgentEvent::ToolCompleted {
            turn_id: TurnId(1),
            result: tool_result(1, "bash"),
        });
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
        assert_eq!(
            tool.records,
            vec![TranscriptRecord::ToolResult(tool_result(1, "bash"))]
        );

        let next_tool = state.step(AgentEvent::ToolReady(second_tool.clone()));
        assert_eq!(
            state,
            AgentState::RunningTool {
                turn_id: TurnId(1),
                tool_call: second_tool
            }
        );
        assert!(matches!(
            next_tool.actions.as_slice(),
            [AgentAction::RequestTool {
                turn_id: TurnId(1),
                ..
            }]
        ));

        let second_result = state.step(AgentEvent::ToolCompleted {
            turn_id: TurnId(1),
            result: tool_result(2, "read"),
        });
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
        assert_eq!(
            second_result.records,
            vec![TranscriptRecord::ToolResult(tool_result(2, "read"))]
        );

        let resume = state.step(AgentEvent::ContinueModel);
        assert_eq!(state, AgentState::RunningModel { turn_id: TurnId(1) });
        assert_eq!(
            resume.actions,
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );
    }

    #[test]
    fn interrupt_transitions_running_state_and_reports_cleanup_work() {
        let mut state = AgentState::RunningTool {
            turn_id: TurnId(3),
            tool_call: tool_call(1, "bash"),
        };

        let transition = state.step(AgentEvent::Interrupt);

        assert_eq!(state, AgentState::Interrupted);
        assert_eq!(
            transition.records,
            vec![
                TranscriptRecord::ToolResult(crate::message::ToolResultMessage::interrupted(
                    ToolCallId(1),
                    "bash"
                )),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(3),
                    outcome: TurnOutcome::Interrupted,
                }
            ]
        );
        assert_eq!(
            transition.actions,
            vec![AgentAction::CancelActive { turn_id: TurnId(3) }]
        );
        assert!(transition.clear_tool_calls);
    }
}
