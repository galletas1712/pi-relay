use std::collections::BTreeMap;

use crate::action::AgentAction;
use crate::event::{AgentEvent, TurnOrigin};
use crate::ids::{ActionId, TurnId};
use crate::message::{AssistantMessage, ToolCall, ToolResultMessage};
use crate::record::{InjectedMessage, TranscriptRecord, TurnOutcome};

// Live control state only. Durable session history lives in Transcript.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentState {
    #[default]
    Idle,
    RunningModel {
        turn_id: TurnId,
        action_id: ActionId,
    },
    RunningTools {
        turn_id: TurnId,
        tool_calls: Vec<ToolCall>,
        tool_action_ids: Vec<ActionId>,
        completed_results: Vec<Option<ToolResultMessage>>,
        next_result_index: usize,
    },
    // Internal transition point after every tool in a batch has completed.
    ReadyToContinue {
        turn_id: TurnId,
    },
}

impl AgentState {
    pub(crate) fn step(
        &mut self,
        event: AgentEvent,
        next_action_id: &mut ActionId,
    ) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
        match event {
            AgentEvent::Interrupt => self.on_interrupt(),
            AgentEvent::StartTurn {
                turn_id,
                input,
                origin,
            } => self.on_start_turn(turn_id, input, origin, next_action_id),
            AgentEvent::ModelCompleted {
                action_id,
                turn_id,
                assistant,
            } => self.on_model_completed(action_id, turn_id, assistant, next_action_id),
            AgentEvent::ModelFailed {
                action_id,
                turn_id,
                error,
            } => self.on_model_failed(action_id, turn_id, error),
            AgentEvent::ToolCompleted {
                action_id,
                turn_id,
                result,
            } => self.on_tool_completed(action_id, turn_id, result),
            AgentEvent::ContinueModel => self.on_continue_model(next_action_id),
        }
    }

    fn on_start_turn(
        &mut self,
        turn_id: TurnId,
        input: String,
        origin: Option<TurnOrigin>,
        next_action_id: &mut ActionId,
    ) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
        match self {
            Self::Idle => {
                let action_id = ActionId::take_next(next_action_id);
                *self = Self::RunningModel { turn_id, action_id };
                let first_record = match origin {
                    None => TranscriptRecord::UserMessage(input),
                    Some(TurnOrigin { from, kind }) => {
                        let mut metadata = BTreeMap::new();
                        metadata.insert("from".to_string(), from);
                        TranscriptRecord::Injected(InjectedMessage {
                            kind,
                            content: input,
                            metadata,
                        })
                    }
                };
                (
                    vec![TranscriptRecord::TurnStarted { turn_id }, first_record],
                    vec![AgentAction::RequestModel { action_id, turn_id }],
                )
            }
            Self::RunningModel { .. }
            | Self::RunningTools { .. }
            | Self::ReadyToContinue { .. } => empty_transition(),
        }
    }

    fn on_model_completed(
        &mut self,
        action_id: ActionId,
        turn_id: TurnId,
        assistant: AssistantMessage,
        next_action_id: &mut ActionId,
    ) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
        if !matches!(
            self,
            Self::RunningModel { turn_id: active_turn_id, action_id: active_action_id }
                if *active_turn_id == turn_id && *active_action_id == action_id
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
            tool_action_ids: Vec::with_capacity(tool_calls.len()),
            completed_results: vec![None; tool_calls.len()],
            next_result_index: 0,
        };
        let Self::RunningTools {
            tool_action_ids, ..
        } = self
        else {
            unreachable!("state just entered RunningTools");
        };
        for tool_call in tool_calls {
            let action_id = ActionId::take_next(next_action_id);
            tool_action_ids.push(action_id);
            records.push(TranscriptRecord::ToolCallStarted {
                turn_id,
                tool_call: tool_call.clone(),
            });
            actions.push(AgentAction::RequestTool {
                action_id,
                turn_id,
                tool_call,
            });
        }
        (records, actions)
    }

    fn on_model_failed(
        &mut self,
        action_id: ActionId,
        turn_id: TurnId,
        _error: String,
    ) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
        if !matches!(
            self,
            Self::RunningModel { turn_id: active_turn_id, action_id: active_action_id }
                if *active_turn_id == turn_id && *active_action_id == action_id
        ) {
            return empty_transition();
        }

        *self = Self::Idle;
        (
            vec![TranscriptRecord::TurnFinished {
                turn_id,
                outcome: TurnOutcome::Crashed,
            }],
            Vec::new(),
        )
    }

    fn on_tool_completed(
        &mut self,
        action_id: ActionId,
        turn_id: TurnId,
        result: ToolResultMessage,
    ) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
        let Self::RunningTools {
            turn_id: active_turn_id,
            tool_calls,
            tool_action_ids,
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

        if tool_action_ids.get(result_index) != Some(&action_id) {
            return empty_transition();
        }

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

    fn on_continue_model(
        &mut self,
        next_action_id: &mut ActionId,
    ) -> (Vec<TranscriptRecord>, Vec<AgentAction>) {
        let Self::ReadyToContinue { turn_id } = self else {
            return empty_transition();
        };
        let turn_id = *turn_id;
        let action_id = ActionId::take_next(next_action_id);

        *self = Self::RunningModel { turn_id, action_id };
        (
            Vec::new(),
            vec![AgentAction::RequestModel { action_id, turn_id }],
        )
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
            Self::RunningModel { turn_id, .. } => {
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
                tool_action_ids: _,
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
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        };

        let mut idle = AgentState::Idle;
        let mut next_action_id = ActionId::first();

        assert!(transition_is_empty(&idle.step(event, &mut next_action_id)));
        assert_eq!(idle, AgentState::Idle);
    }

    #[test]
    fn running_model_accepts_only_matching_model_completion() {
        let mut state = AgentState::RunningModel {
            turn_id: TurnId(2),
            action_id: ActionId(1),
        };
        let assistant = AssistantMessage { items: Vec::new() };
        let matching = AgentEvent::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(2),
            assistant: assistant.clone(),
        };
        let stale = AgentEvent::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant,
        };

        let mut stale_state = AgentState::RunningModel {
            turn_id: TurnId(2),
            action_id: ActionId(1),
        };
        let mut next_action_id = ActionId::first();
        assert!(transition_is_empty(
            &stale_state.step(stale, &mut next_action_id)
        ));

        let (records, _) = state.step(matching, &mut next_action_id);
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
            tool_action_ids: vec![ActionId(1)],
            completed_results: vec![None],
            next_result_index: 0,
        };
        let result = AgentEvent::ToolCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            result: tool_result(1, "bash"),
        };
        let wrong_tool = AgentEvent::ToolCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            result: tool_result(1, "read"),
        };
        let mut next_action_id = ActionId::first();

        assert!(transition_is_empty(
            &state.clone().step(wrong_tool, &mut next_action_id)
        ));

        let (records, _) = state.step(result, &mut next_action_id);
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
        assert_eq!(
            records,
            vec![TranscriptRecord::ToolResult(tool_result(1, "bash"))]
        );
    }

    #[test]
    fn step_moves_through_turn_tool_and_resume_states() {
        let mut state = AgentState::Idle;
        let mut next_action_id = ActionId::first();
        let first_tool = tool_call(1, "bash");
        let second_tool = tool_call(2, "read");

        let (_, actions) = state.step(
            AgentEvent::StartTurn {
                turn_id: TurnId(1),
                input: "hello".to_string(),
                origin: None,
            },
            &mut next_action_id,
        );
        assert_eq!(
            state,
            AgentState::RunningModel {
                turn_id: TurnId(1),
                action_id: ActionId(1),
            }
        );
        assert_eq!(
            actions,
            vec![AgentAction::RequestModel {
                action_id: ActionId(1),
                turn_id: TurnId(1),
            }]
        );

        let (_, actions) = state.step(
            AgentEvent::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant: AssistantMessage {
                    items: vec![
                        AssistantItem::ToolCall(first_tool.clone()),
                        AssistantItem::ToolCall(second_tool.clone()),
                    ],
                },
            },
            &mut next_action_id,
        );
        assert_eq!(
            state,
            AgentState::RunningTools {
                turn_id: TurnId(1),
                tool_calls: vec![first_tool.clone(), second_tool.clone()],
                tool_action_ids: vec![ActionId(2), ActionId(3)],
                completed_results: vec![None, None],
                next_result_index: 0,
            }
        );
        assert_eq!(
            actions,
            vec![
                AgentAction::RequestTool {
                    action_id: ActionId(2),
                    turn_id: TurnId(1),
                    tool_call: first_tool.clone(),
                },
                AgentAction::RequestTool {
                    action_id: ActionId(3),
                    turn_id: TurnId(1),
                    tool_call: second_tool.clone(),
                },
            ]
        );

        let second_tool_first = state.step(
            AgentEvent::ToolCompleted {
                action_id: ActionId(3),
                turn_id: TurnId(1),
                result: tool_result(2, "read"),
            },
            &mut next_action_id,
        );
        assert!(transition_is_empty(&second_tool_first));
        assert_eq!(
            state,
            AgentState::RunningTools {
                turn_id: TurnId(1),
                tool_calls: vec![first_tool.clone(), second_tool.clone()],
                tool_action_ids: vec![ActionId(2), ActionId(3)],
                completed_results: vec![None, Some(tool_result(2, "read"))],
                next_result_index: 0,
            }
        );

        let (records, _) = state.step(
            AgentEvent::ToolCompleted {
                action_id: ActionId(2),
                turn_id: TurnId(1),
                result: tool_result(1, "bash"),
            },
            &mut next_action_id,
        );
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
        assert_eq!(
            records,
            vec![
                TranscriptRecord::ToolResult(tool_result(1, "bash")),
                TranscriptRecord::ToolResult(tool_result(2, "read")),
            ]
        );

        let (_, actions) = state.step(AgentEvent::ContinueModel, &mut next_action_id);
        assert_eq!(
            state,
            AgentState::RunningModel {
                turn_id: TurnId(1),
                action_id: ActionId(4),
            }
        );
        assert_eq!(
            actions,
            vec![AgentAction::RequestModel {
                action_id: ActionId(4),
                turn_id: TurnId(1),
            }]
        );
    }

    #[test]
    fn interrupt_transitions_running_state_and_reports_cleanup_work() {
        let mut state = AgentState::RunningTools {
            turn_id: TurnId(3),
            tool_calls: vec![tool_call(1, "bash")],
            tool_action_ids: vec![ActionId(1)],
            completed_results: vec![None],
            next_result_index: 0,
        };
        let mut next_action_id = ActionId::first();

        let (records, actions) = state.step(AgentEvent::Interrupt, &mut next_action_id);

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
