use crate::action::AgentAction;
use crate::event::AgentEvent;
use crate::ids::TurnId;
use crate::message::{AssistantMessage, ToolCall, ToolResultMessage};
use crate::transcript::{TranscriptRecord, TurnOutcome};

// Live control state only. Durable session history lives in Transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Idle,
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

impl AgentState {
    pub(crate) fn step(&mut self, event: AgentEvent) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
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
        input: String,
    ) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
        match self {
            Self::Idle => {
                *self = Self::RunningModel { turn_id };
                (
                    vec![
                        TranscriptRecord::TurnStarted { turn_id },
                        TranscriptRecord::UserMessage(input),
                    ],
                    vec![AgentAction::RequestModel { turn_id }],
                )
            }
            Self::RunningModel { .. }
            | Self::RunningTools { .. }
            | Self::ReadyToContinue { .. } => empty_transition(),
        }
    }

    fn on_model_completed(
        &mut self,
        turn_id: TurnId,
        assistant: AssistantMessage,
    ) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
        if !matches!(
            self,
            Self::RunningModel { turn_id: active_turn_id } if *active_turn_id == turn_id
        ) {
            return empty_transition();
        }

        let mut records = vec![TranscriptRecord::AssistantMessage(assistant.clone())];
        let mut actions = Vec::new();

        let tool_calls: Vec<ToolCall> = assistant.tool_calls().cloned().collect();
        if tool_calls.is_empty() {
            *self = Self::Idle;
            records.push(TranscriptRecord::TurnFinished {
                turn_id,
                outcome: TurnOutcome::Graceful,
            });
            return (records, actions);
        }

        *self = Self::RunningTools {
            turn_id,
            tool_calls: tool_calls.clone(),
            completed_results: vec![None; tool_calls.len()],
            next_result_index: 0,
        };
        for tool_call in tool_calls {
            records.push(TranscriptRecord::ToolCallStarted {
                turn_id,
                tool_call: tool_call.clone(),
            });
            actions.push(AgentAction::RequestTool { turn_id, tool_call });
        }
        (records, actions)
    }

    fn on_tool_completed(
        &mut self,
        turn_id: TurnId,
        result: ToolResultMessage,
    ) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
        let Self::RunningTools {
            turn_id: active_turn_id,
            tool_calls,
            completed_results,
            next_result_index,
        } = self
        else {
            return empty_transition();
        };

        if *active_turn_id != turn_id {
            return empty_transition();
        }

        let Some(result_index) = tool_calls.iter().position(|tool_call| {
            tool_call.id == result.tool_call_id && tool_call.tool_name == result.tool_name
        }) else {
            return empty_transition();
        };

        if result_index < *next_result_index || completed_results[result_index].is_some() {
            return empty_transition();
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

        (records, Vec::new())
    }

    fn on_continue_model(&mut self) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
        let Self::ReadyToContinue { turn_id } = self else {
            return empty_transition();
        };
        let turn_id = *turn_id;

        *self = Self::RunningModel { turn_id };
        (Vec::new(), vec![AgentAction::RequestModel { turn_id }])
    }

    fn on_interrupt(&mut self) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
        match self.clone() {
            Self::Idle => empty_transition(),
            Self::ReadyToContinue { turn_id } => {
                *self = Self::Idle;
                (
                    vec![TranscriptRecord::TurnFinished {
                        turn_id,
                        outcome: TurnOutcome::Interrupted,
                    }],
                    Vec::new(),
                )
            }
            Self::RunningModel { turn_id } => {
                *self = Self::Idle;
                (
                    vec![TranscriptRecord::TurnFinished {
                        turn_id,
                        outcome: TurnOutcome::Interrupted,
                    }],
                    vec![AgentAction::CancelTurn { turn_id }],
                )
            }
            Self::RunningTools {
                turn_id,
                tool_calls,
                completed_results,
                next_result_index,
            } => {
                *self = Self::Idle;
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
                (records, vec![AgentAction::CancelTurn { turn_id }])
            }
        }
    }
}

fn empty_transition() -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
    (Vec::new(), Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ToolCallId;
    use crate::message::{AssistantItem, AssistantMessage, ToolResultMessage, ToolResultStatus};

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

    fn transition_is_empty((records, actions): &(Vec<TranscriptRecord>, Vec<AgentAction>)) -> bool {
        records.is_empty() && actions.is_empty()
    }

    #[test]
    fn idle_ignores_late_model_completions() {
        let event = AgentEvent::ModelCompleted {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        };

        let mut idle = AgentState::Idle;

        assert!(transition_is_empty(&idle.step(event)));
        assert_eq!(idle, AgentState::Idle);
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

        let mut stale_state = AgentState::RunningModel { turn_id: TurnId(2) };
        assert!(transition_is_empty(&stale_state.step(stale)));

        let (records, _) = state.step(matching);
        assert_eq!(state, AgentState::Idle);
        assert_eq!(
            records,
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

        assert!(transition_is_empty(&state.clone().step(wrong_tool)));

        let (records, _) = state.step(result);
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
        assert_eq!(
            records,
            vec![TranscriptRecord::ToolResult(tool_result(1, "bash"))]
        );
    }

    #[test]
    fn step_moves_through_turn_tool_and_resume_states() {
        let mut state = AgentState::Idle;
        let first_tool = tool_call(1, "bash");
        let second_tool = tool_call(2, "read");

        let (_, actions) = state.step(AgentEvent::StartTurn {
            turn_id: TurnId(1),
            input: "hello".to_string(),
        });
        assert_eq!(state, AgentState::RunningModel { turn_id: TurnId(1) });
        assert_eq!(
            actions,
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );

        let (_, actions) = state.step(AgentEvent::ModelCompleted {
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
            actions,
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
        assert!(transition_is_empty(&second_tool_first));
        assert_eq!(
            state,
            AgentState::RunningTools {
                turn_id: TurnId(1),
                tool_calls: vec![first_tool.clone(), second_tool.clone()],
                completed_results: vec![None, Some(tool_result(2, "read"))],
                next_result_index: 0,
            }
        );

        let (records, _) = state.step(AgentEvent::ToolCompleted {
            turn_id: TurnId(1),
            result: tool_result(1, "bash"),
        });
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
        assert_eq!(
            records,
            vec![
                TranscriptRecord::ToolResult(tool_result(1, "bash")),
                TranscriptRecord::ToolResult(tool_result(2, "read")),
            ]
        );

        let (_, actions) = state.step(AgentEvent::ContinueModel);
        assert_eq!(state, AgentState::RunningModel { turn_id: TurnId(1) });
        assert_eq!(
            actions,
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

        let (records, actions) = state.step(AgentEvent::Interrupt);

        assert_eq!(state, AgentState::Idle);
        assert_eq!(
            records,
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
            actions,
            vec![AgentAction::CancelTurn { turn_id: TurnId(3) }]
        );
    }
}
