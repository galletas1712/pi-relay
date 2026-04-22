use crate::action::AgentAction;
use crate::event::AgentEvent;
use crate::ids::TurnId;
use crate::message::{ToolCall, ToolResultMessage, UserMessage};
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
    RunningTools {
        turn_id: TurnId,
        tool_calls: Vec<ToolCall>,
        completed_results: Vec<Option<ToolResultMessage>>,
        next_result_index: usize,
    },
    // Internal transition point after every tool in a batch has completed.
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
}

impl AgentTransition {
    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty() && self.actions.is_empty()
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
                }
            }
            Self::RunningModel { .. }
            | Self::RunningTools { .. }
            | Self::ReadyToContinue { .. } => AgentTransition::default(),
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

        let tool_calls: Vec<ToolCall> = assistant.tool_calls().cloned().collect();
        if tool_calls.is_empty() {
            *self = Self::Idle;
            transition.records.push(TranscriptRecord::TurnFinished {
                turn_id,
                outcome: TurnOutcome::Graceful,
            });
            return transition;
        }

        *self = Self::RunningTools {
            turn_id,
            tool_calls: tool_calls.clone(),
            completed_results: vec![None; tool_calls.len()],
            next_result_index: 0,
        };
        for tool_call in tool_calls {
            transition.records.push(TranscriptRecord::ToolCallStarted {
                turn_id,
                tool_call: tool_call.clone(),
            });
            transition
                .actions
                .push(AgentAction::RequestTool { turn_id, tool_call });
        }
        transition
    }

    fn on_tool_completed(&mut self, turn_id: TurnId, result: ToolResultMessage) -> AgentTransition {
        let Self::RunningTools {
            turn_id: active_turn_id,
            tool_calls,
            completed_results,
            next_result_index,
        } = self
        else {
            return AgentTransition::default();
        };

        if *active_turn_id != turn_id {
            return AgentTransition::default();
        }

        let Some(result_index) = tool_calls.iter().position(|tool_call| {
            tool_call.id == result.tool_call_id && tool_call.tool_name == result.tool_name
        }) else {
            return AgentTransition::default();
        };

        if result_index < *next_result_index || completed_results[result_index].is_some() {
            return AgentTransition::default();
        }

        completed_results[result_index] = Some(result);

        let mut records = Vec::new();
        while *next_result_index < completed_results.len() {
            let Some(result) = completed_results[*next_result_index].take() else {
                break;
            };
            records.push(TranscriptRecord::ToolResult(result));
            *next_result_index += 1;
        }

        let finished = *next_result_index == tool_calls.len();
        if finished {
            *self = Self::ReadyToContinue { turn_id };
        }

        AgentTransition {
            records,
            actions: Vec::new(),
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
                    actions: Vec::new(),
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
                }
            }
            Self::RunningTools {
                turn_id,
                tool_calls,
                completed_results,
                next_result_index,
            } => {
                *self = Self::Interrupted;
                let mut records = Vec::new();
                for (index, tool_call) in tool_calls.into_iter().enumerate().skip(next_result_index)
                {
                    let result = completed_results[index].clone().unwrap_or_else(|| {
                        ToolResultMessage::interrupted(tool_call.id, tool_call.tool_name)
                    });
                    records.push(TranscriptRecord::ToolResult(result));
                }
                records.push(TranscriptRecord::TurnFinished {
                    turn_id,
                    outcome: TurnOutcome::Interrupted,
                });
                AgentTransition {
                    records,
                    actions: vec![AgentAction::CancelActive { turn_id }],
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
        let mut state = AgentState::RunningTools {
            turn_id: TurnId(1),
            tool_calls: vec![tool_call(1, "bash")],
            completed_results: vec![None],
            next_result_index: 0,
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
            AgentState::RunningTools {
                turn_id: TurnId(1),
                tool_calls: vec![first_tool.clone(), second_tool.clone()],
                completed_results: vec![None, None],
                next_result_index: 0,
            }
        );
        assert_eq!(
            model.actions,
            vec![
                AgentAction::RequestTool {
                    turn_id: TurnId(1),
                    tool_call: first_tool.clone(),
                },
                AgentAction::RequestTool {
                    turn_id: TurnId(1),
                    tool_call: second_tool.clone(),
                },
            ]
        );

        let second_tool_first = state.step(AgentEvent::ToolCompleted {
            turn_id: TurnId(1),
            result: tool_result(2, "read"),
        });
        assert!(second_tool_first.is_empty());
        assert_eq!(
            state,
            AgentState::RunningTools {
                turn_id: TurnId(1),
                tool_calls: vec![first_tool.clone(), second_tool.clone()],
                completed_results: vec![None, Some(tool_result(2, "read"))],
                next_result_index: 0,
            }
        );

        let first_tool_second = state.step(AgentEvent::ToolCompleted {
            turn_id: TurnId(1),
            result: tool_result(1, "bash"),
        });
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
        assert_eq!(
            first_tool_second.records,
            vec![
                TranscriptRecord::ToolResult(tool_result(1, "bash")),
                TranscriptRecord::ToolResult(tool_result(2, "read")),
            ]
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
        let mut state = AgentState::RunningTools {
            turn_id: TurnId(3),
            tool_calls: vec![tool_call(1, "bash")],
            completed_results: vec![None],
            next_result_index: 0,
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
    }
}
