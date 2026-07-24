use std::collections::HashMap;

use crate::action::AgentAction;
use crate::event::{AgentEvent, TurnInput};
use agent_vocab::{
    ActionId, AssistantMessage, DaemonToolObservation, ToolCall, ToolResultMessage, TranscriptItem,
    TurnId, TurnOutcome,
};

// Live control state only. Durable session history lives in TranscriptStore.
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
        tools: Vec<RunningTool>,
        tool_index_by_action_id: HashMap<ActionId, usize>,
        next_result_index: usize,
    },
    // Internal transition point after every tool in a batch has completed.
    ReadyToContinue {
        turn_id: TurnId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningTool {
    pub(crate) call: ToolCall,
    pub(crate) action_id: ActionId,
    pub(crate) result: Option<ToolResultMessage>,
}

impl AgentState {
    pub(crate) fn step(
        &mut self,
        event: AgentEvent,
        next_action_id: &mut ActionId,
    ) -> (Vec<TranscriptItem>, Vec<AgentAction>) {
        match event {
            AgentEvent::Interrupt => self.on_interrupt(),
            AgentEvent::StartTurn { turn_id, input } => {
                self.on_start_turn(turn_id, input, next_action_id)
            }
            AgentEvent::Steer { input } => self.on_steer(input, next_action_id),
            AgentEvent::StartDaemonObservationTurn {
                turn_id,
                observation,
            } => self.on_start_daemon_observation_turn(turn_id, observation, next_action_id),
            AgentEvent::DaemonObservation { observation } => {
                self.on_daemon_observation(observation, next_action_id)
            }
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
        input: TurnInput,
        next_action_id: &mut ActionId,
    ) -> (Vec<TranscriptItem>, Vec<AgentAction>) {
        match self {
            Self::Idle => {
                let action_id = ActionId::take_next(next_action_id);
                *self = Self::RunningModel { turn_id, action_id };
                (
                    vec![
                        TranscriptItem::TurnStarted { turn_id },
                        input.into_transcript_item(),
                    ],
                    vec![AgentAction::RequestModel { action_id, turn_id }],
                )
            }
            Self::RunningModel { .. }
            | Self::RunningTools { .. }
            | Self::ReadyToContinue { .. } => empty_transition(),
        }
    }

    fn on_steer(
        &mut self,
        input: TurnInput,
        next_action_id: &mut ActionId,
    ) -> (Vec<TranscriptItem>, Vec<AgentAction>) {
        let Self::ReadyToContinue { turn_id } = self else {
            return empty_transition();
        };
        let turn_id = *turn_id;
        let action_id = ActionId::take_next(next_action_id);

        *self = Self::RunningModel { turn_id, action_id };
        (
            vec![input.into_transcript_item()],
            vec![AgentAction::RequestModel { action_id, turn_id }],
        )
    }

    fn on_start_daemon_observation_turn(
        &mut self,
        turn_id: TurnId,
        observation: DaemonToolObservation,
        next_action_id: &mut ActionId,
    ) -> (Vec<TranscriptItem>, Vec<AgentAction>) {
        match self {
            Self::Idle => {
                let action_id = ActionId::take_next(next_action_id);
                *self = Self::RunningModel { turn_id, action_id };
                (
                    vec![
                        TranscriptItem::TurnStarted { turn_id },
                        TranscriptItem::DaemonToolObservation(observation),
                    ],
                    vec![AgentAction::RequestModel { action_id, turn_id }],
                )
            }
            Self::RunningModel { .. }
            | Self::RunningTools { .. }
            | Self::ReadyToContinue { .. } => empty_transition(),
        }
    }

    fn on_daemon_observation(
        &mut self,
        observation: DaemonToolObservation,
        next_action_id: &mut ActionId,
    ) -> (Vec<TranscriptItem>, Vec<AgentAction>) {
        let Self::ReadyToContinue { turn_id } = self else {
            return empty_transition();
        };
        let turn_id = *turn_id;
        let action_id = ActionId::take_next(next_action_id);

        *self = Self::RunningModel { turn_id, action_id };
        (
            vec![TranscriptItem::DaemonToolObservation(observation)],
            vec![AgentAction::RequestModel { action_id, turn_id }],
        )
    }

    fn on_model_completed(
        &mut self,
        action_id: ActionId,
        turn_id: TurnId,
        assistant: AssistantMessage,
        next_action_id: &mut ActionId,
    ) -> (Vec<TranscriptItem>, Vec<AgentAction>) {
        if !matches!(
            self,
            Self::RunningModel { turn_id: active_turn_id, action_id: active_action_id }
                if *active_turn_id == turn_id && *active_action_id == action_id
        ) {
            return empty_transition();
        }

        let mut items = vec![TranscriptItem::AssistantMessage(assistant.clone())];
        let mut actions = Vec::new();

        let tool_calls: Vec<ToolCall> = assistant.tool_calls().cloned().collect();
        if tool_calls.is_empty() {
            *self = Self::Idle;
            items.push(TranscriptItem::TurnFinished {
                turn_id,
                outcome: TurnOutcome::Graceful,
            });
            return (items, actions);
        }

        let mut tools = Vec::with_capacity(tool_calls.len());
        let mut tool_index_by_action_id = HashMap::with_capacity(tool_calls.len());
        for tool_call in tool_calls {
            let action_id = ActionId::take_next(next_action_id);
            items.push(TranscriptItem::ToolCallStarted {
                turn_id,
                tool_call: tool_call.clone(),
            });
            actions.push(AgentAction::RequestTool {
                action_id,
                turn_id,
                tool_call: tool_call.clone(),
            });
            tools.push(RunningTool {
                call: tool_call,
                action_id,
                result: None,
            });
            tool_index_by_action_id.insert(action_id, tools.len() - 1);
        }
        *self = Self::RunningTools {
            turn_id,
            tools,
            tool_index_by_action_id,
            next_result_index: 0,
        };
        (items, actions)
    }

    fn on_model_failed(
        &mut self,
        action_id: ActionId,
        turn_id: TurnId,
        _error: String,
    ) -> (Vec<TranscriptItem>, Vec<AgentAction>) {
        if !matches!(
            self,
            Self::RunningModel { turn_id: active_turn_id, action_id: active_action_id }
                if *active_turn_id == turn_id && *active_action_id == action_id
        ) {
            return empty_transition();
        }

        *self = Self::Idle;
        (
            vec![TranscriptItem::TurnFinished {
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
    ) -> (Vec<TranscriptItem>, Vec<AgentAction>) {
        let Self::RunningTools {
            turn_id: active_turn_id,
            tools,
            tool_index_by_action_id,
            next_result_index,
        } = self
        else {
            return empty_transition();
        };

        if *active_turn_id != turn_id {
            return empty_transition();
        }

        #[cfg(test)]
        count_tool_completion_operation();
        let Some(&result_index) = tool_index_by_action_id.get(&action_id) else {
            return empty_transition();
        };

        if tools[result_index].call.id != result.tool_call_id
            || tools[result_index].call.tool_name != result.tool_name
        {
            return empty_transition();
        }

        if result_index < *next_result_index || tools[result_index].result.is_some() {
            return empty_transition();
        }

        tools[result_index].result = Some(result);

        let mut items = Vec::new();
        while *next_result_index < tools.len() {
            #[cfg(test)]
            count_tool_completion_operation();
            let Some(result) = tools[*next_result_index].result.take() else {
                break;
            };
            items.push(TranscriptItem::ToolResult(result));
            *next_result_index += 1;
        }

        let finished = *next_result_index == tools.len();
        if finished {
            *self = Self::ReadyToContinue { turn_id };
        }

        (items, Vec::new())
    }

    fn on_continue_model(
        &mut self,
        next_action_id: &mut ActionId,
    ) -> (Vec<TranscriptItem>, Vec<AgentAction>) {
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

    fn on_interrupt(&mut self) -> (Vec<TranscriptItem>, Vec<AgentAction>) {
        match self.clone() {
            Self::Idle => empty_transition(),
            Self::ReadyToContinue { turn_id } => {
                // All external tool work has already completed in this state;
                // interrupt only closes the turn and has nothing to cancel.
                *self = Self::Idle;
                (
                    vec![TranscriptItem::TurnFinished {
                        turn_id,
                        outcome: TurnOutcome::Interrupted,
                    }],
                    Vec::new(),
                )
            }
            Self::RunningModel { turn_id, .. } => {
                *self = Self::Idle;
                (
                    vec![TranscriptItem::TurnFinished {
                        turn_id,
                        outcome: TurnOutcome::Interrupted,
                    }],
                    vec![AgentAction::CancelTurn { turn_id }],
                )
            }
            Self::RunningTools {
                turn_id,
                tools,
                next_result_index,
                ..
            } => {
                *self = Self::Idle;
                let mut items = Vec::new();
                for tool in tools.into_iter().skip(next_result_index) {
                    let result = tool.result.unwrap_or_else(|| {
                        ToolResultMessage::crashed(tool.call.id, tool.call.tool_name)
                    });
                    items.push(TranscriptItem::ToolResult(result));
                }
                items.push(TranscriptItem::TurnFinished {
                    turn_id,
                    outcome: TurnOutcome::Interrupted,
                });
                (items, vec![AgentAction::CancelTurn { turn_id }])
            }
        }
    }
}

fn empty_transition() -> (Vec<TranscriptItem>, Vec<AgentAction>) {
    (Vec::new(), Vec::new())
}

#[cfg(test)]
thread_local! {
    static TOOL_COMPLETION_OPERATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
fn count_tool_completion_operation() {
    TOOL_COMPLETION_OPERATIONS.set(TOOL_COMPLETION_OPERATIONS.get() + 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_vocab::{
        AssistantItem, AssistantMessage, ToolCallId, ToolResultMessage, ToolResultStatus,
        UserMessage,
    };

    fn tool_call(id: u64, name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::from_u64(id),
            tool_name: name.to_string(),
            args_json: "{}".to_string(),
        }
    }

    fn tool_result(id: u64, name: &str) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: ToolCallId::from_u64(id),
            tool_name: name.to_string(),
            output: "ok".to_string(),
            status: ToolResultStatus::Success,
        }
    }

    fn running_tool(action_id: u64, call: ToolCall) -> RunningTool {
        RunningTool {
            call,
            action_id: ActionId(action_id),
            result: None,
        }
    }

    fn completed_running_tool(
        action_id: u64,
        call: ToolCall,
        result: ToolResultMessage,
    ) -> RunningTool {
        RunningTool {
            call,
            action_id: ActionId(action_id),
            result: Some(result),
        }
    }

    fn transition_is_empty((items, actions): &(Vec<TranscriptItem>, Vec<AgentAction>)) -> bool {
        items.is_empty() && actions.is_empty()
    }

    fn tool_index_by_action_id(tools: &[RunningTool]) -> HashMap<ActionId, usize> {
        tools
            .iter()
            .enumerate()
            .map(|(index, tool)| (tool.action_id, index))
            .collect()
    }

    fn reset_tool_completion_operations() {
        TOOL_COMPLETION_OPERATIONS.set(0);
    }

    fn tool_completion_operations() -> usize {
        TOOL_COMPLETION_OPERATIONS.get()
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

        let (items, _) = state.step(matching, &mut next_action_id);
        assert_eq!(state, AgentState::Idle);
        assert_eq!(
            items,
            vec![
                TranscriptItem::AssistantMessage(AssistantMessage { items: Vec::new() }),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(2),
                    outcome: TurnOutcome::Graceful,
                }
            ]
        );
    }

    #[test]
    fn running_tools_correlate_duplicate_provider_identities_by_action_id() {
        let call = tool_call(1, "same");
        let tools = vec![running_tool(1, call.clone()), running_tool(2, call.clone())];
        let mut state = AgentState::RunningTools {
            turn_id: TurnId(1),
            tool_index_by_action_id: tool_index_by_action_id(&tools),
            tools,
            next_result_index: 0,
        };
        let mut next_action_id = ActionId(3);

        assert_eq!(
            state.step(
                AgentEvent::ToolCompleted {
                    action_id: ActionId(1),
                    turn_id: TurnId(1),
                    result: tool_result(1, "same"),
                },
                &mut next_action_id,
            ),
            (
                vec![TranscriptItem::ToolResult(tool_result(1, "same"))],
                Vec::new(),
            )
        );
        assert_eq!(
            state.step(
                AgentEvent::ToolCompleted {
                    action_id: ActionId(2),
                    turn_id: TurnId(1),
                    result: tool_result(1, "same"),
                },
                &mut next_action_id,
            ),
            (
                vec![TranscriptItem::ToolResult(tool_result(1, "same"))],
                Vec::new(),
            )
        );
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
    }

    #[test]
    fn running_tool_accepts_only_matching_tool_completion() {
        let mut state = AgentState::RunningTools {
            turn_id: TurnId(1),
            tools: vec![running_tool(1, tool_call(1, "bash"))],
            tool_index_by_action_id: HashMap::from([(ActionId(1), 0)]),
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

        let (items, _) = state.step(result, &mut next_action_id);
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
        assert_eq!(
            items,
            vec![TranscriptItem::ToolResult(tool_result(1, "bash"))]
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
                input: TurnInput(UserMessage::text("hello")),
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
                tools: vec![
                    running_tool(2, first_tool.clone()),
                    running_tool(3, second_tool.clone()),
                ],
                tool_index_by_action_id: HashMap::from([(ActionId(2), 0), (ActionId(3), 1),]),
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
                tools: vec![
                    running_tool(2, first_tool.clone()),
                    completed_running_tool(3, second_tool.clone(), tool_result(2, "read")),
                ],
                tool_index_by_action_id: HashMap::from([(ActionId(2), 0), (ActionId(3), 1),]),
                next_result_index: 0,
            }
        );

        let (items, _) = state.step(
            AgentEvent::ToolCompleted {
                action_id: ActionId(2),
                turn_id: TurnId(1),
                result: tool_result(1, "bash"),
            },
            &mut next_action_id,
        );
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
        assert_eq!(
            items,
            vec![
                TranscriptItem::ToolResult(tool_result(1, "bash")),
                TranscriptItem::ToolResult(tool_result(2, "read")),
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
            tools: vec![running_tool(1, tool_call(1, "bash"))],
            tool_index_by_action_id: HashMap::from([(ActionId(1), 0)]),
            next_result_index: 0,
        };
        let mut next_action_id = ActionId::first();

        let (items, actions) = state.step(AgentEvent::Interrupt, &mut next_action_id);

        assert_eq!(state, AgentState::Idle);
        assert_eq!(
            items,
            vec![
                TranscriptItem::ToolResult(ToolResultMessage::crashed(
                    ToolCallId::from_u64(1),
                    "bash"
                )),
                TranscriptItem::TurnFinished {
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

    #[test]
    fn tool_completion_matching_and_source_order_release_scale_linearly() {
        for tool_count in [1, 10, 100, 1_000] {
            let tools = (0..tool_count)
                .map(|index| running_tool(index as u64 + 1, tool_call(index as u64 + 1, "tool")))
                .collect::<Vec<_>>();
            let mut state = AgentState::RunningTools {
                turn_id: TurnId(1),
                tool_index_by_action_id: tool_index_by_action_id(&tools),
                tools,
                next_result_index: 0,
            };
            let mut next_action_id = ActionId(tool_count as u64 + 1);
            let mut items = Vec::new();
            reset_tool_completion_operations();

            for index in (0..tool_count).rev() {
                let (released, actions) = state.step(
                    AgentEvent::ToolCompleted {
                        action_id: ActionId(index as u64 + 1),
                        turn_id: TurnId(1),
                        result: tool_result(index as u64 + 1, "tool"),
                    },
                    &mut next_action_id,
                );
                items.extend(released);
                assert!(actions.is_empty());
            }

            let expected_items = (0..tool_count)
                .map(|index| TranscriptItem::ToolResult(tool_result(index as u64 + 1, "tool")))
                .collect::<Vec<_>>();
            assert_eq!(items, expected_items);
            assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });
            assert_eq!(
                tool_completion_operations(),
                3 * tool_count - 1,
                "one lookup per completion plus amortized source-front examinations"
            );
        }
    }
}
