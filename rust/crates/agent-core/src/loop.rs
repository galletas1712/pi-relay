use std::collections::VecDeque;

use crate::action::AgentAction;
use crate::context_item::ContextItem;
use crate::event::{AgentInput, AgentInputError};
use crate::ids::{ActionId, TurnId};
use crate::mailbox::Mailbox;
use crate::state::AgentState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCoreLoop {
    mailbox: Mailbox,
    state: AgentState,
    last_turn_id: TurnId,
    next_action_id: ActionId,
    action_outbox: VecDeque<AgentAction>,
    record_outbox: VecDeque<ContextItem>,
}

impl Default for AgentCoreLoop {
    fn default() -> Self {
        Self {
            mailbox: Mailbox::default(),
            state: AgentState::Idle,
            last_turn_id: TurnId::default(),
            next_action_id: ActionId::first(),
            action_outbox: VecDeque::new(),
            record_outbox: VecDeque::new(),
        }
    }
}

impl AgentCoreLoop {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resume a fresh idle core at the given turn boundary.
    ///
    /// Callers own durable history; the core itself no longer buffers records.
    /// The session derives `last_turn_id` from its log before calling this.
    pub fn resume_at_boundary(last_turn_id: TurnId) -> Self {
        Self::resume_at_boundary_with_next_action_id(last_turn_id, ActionId::first())
    }

    pub fn resume_at_boundary_with_next_action_id(
        last_turn_id: TurnId,
        next_action_id: ActionId,
    ) -> Self {
        Self {
            mailbox: Mailbox::default(),
            state: AgentState::Idle,
            last_turn_id,
            next_action_id,
            action_outbox: VecDeque::new(),
            record_outbox: VecDeque::new(),
        }
    }

    pub fn enqueue_input(&mut self, input: AgentInput) -> Result<(), AgentInputError> {
        self.mailbox.push_input(input)
    }

    /// True when the core is between turns and has no in-flight model/tool work.
    ///
    /// Exposed so callers can observe liveness without reaching into the
    /// underlying `AgentState` enum, which is a private implementation detail.
    pub fn is_idle(&self) -> bool {
        self.state == AgentState::Idle
    }

    /// True when the mailbox still has queued inputs waiting to be processed.
    ///
    /// Exposed so callers can observe liveness without reaching into the
    /// underlying `Mailbox`, which is a private implementation detail.
    pub fn has_pending_work(&self) -> bool {
        self.mailbox.total_len() > 0
    }

    /// The most recent turn id observed by the core.
    pub fn last_turn_id(&self) -> TurnId {
        self.last_turn_id
    }

    pub fn next_action_id(&self) -> ActionId {
        self.next_action_id
    }

    pub fn drain_actions(&mut self) -> Vec<AgentAction> {
        self.action_outbox.drain(..).collect()
    }

    pub fn drain_records(&mut self) -> Vec<ContextItem> {
        self.record_outbox.drain(..).collect()
    }

    /// Drain every queued user input (Steer then FollowUp) from the mailbox
    /// without advancing the FSM. Preserves the `from` and `kind` tags each
    /// input was enqueued with. Notifications and the interrupt flag are
    /// untouched.
    ///
    /// Primarily intended for tests and for orchestrator introspection.
    pub fn drain_pending_inputs(&mut self) -> Vec<AgentInput> {
        self.mailbox.drain_pending_inputs()
    }

    pub fn drive(&mut self) {
        loop {
            let next_turn_id = self.last_turn_id.next();
            let Some(event) = self.mailbox.next_event(&self.state, next_turn_id) else {
                return;
            };

            let (records, actions) = self.state.step(event, &mut self.next_action_id);
            if records.is_empty() && actions.is_empty() {
                continue;
            }

            if let Some(turn_id) = started_turn_id(&records) {
                self.last_turn_id = turn_id;
            }
            self.record_outbox.extend(records);
            self.action_outbox.extend(actions);
        }
    }
}

fn started_turn_id(records: &[ContextItem]) -> Option<TurnId> {
    records.iter().find_map(|record| match record {
        ContextItem::TurnStarted { turn_id } => Some(*turn_id),
        ContextItem::UserMessage(_)
        | ContextItem::AssistantMessage(_)
        | ContextItem::ToolCallStarted { .. }
        | ContextItem::ToolResult(_)
        | ContextItem::TurnFinished { .. }
        | ContextItem::Injected(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::action::AgentAction;
    use crate::context_item::{InjectedMessage, TurnOutcome};
    use crate::ids::{ActionId, ToolCallId};
    use crate::message::{
        AssistantItem, AssistantMessage, ToolCall, ToolResultMessage, ToolResultStatus,
    };

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
            status: ToolResultStatus::Success,
        }
    }

    /// Drive a single input end-to-end and collect every record emitted during
    /// that cycle. Tests accumulate these into a running context-item list so
    /// they can assert the shape the session stores durably.
    fn drive_collect(loop_state: &mut AgentCoreLoop, input: AgentInput) -> Vec<ContextItem> {
        loop_state
            .enqueue_input(input)
            .expect("test inputs should be valid");
        loop_state.drive();
        loop_state.drain_records()
    }

    #[test]
    fn starting_a_turn_appends_boundary_events_and_requests_the_model() {
        let mut loop_state = AgentCoreLoop::new();

        let records = drive_collect(&mut loop_state, AgentInput::follow_up("hello"));

        assert_eq!(
            records,
            vec![
                ContextItem::TurnStarted { turn_id: TurnId(1) },
                ContextItem::UserMessage("hello".to_string()),
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestModel {
                action_id: ActionId(1),
                turn_id: TurnId(1)
            }]
        );
        assert_eq!(
            loop_state.state,
            AgentState::RunningModel {
                action_id: ActionId(1),
                turn_id: TurnId(1)
            }
        );
    }

    #[test]
    fn model_completion_with_a_tool_call_appends_assistant_and_starts_the_tool() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        let mut records = drive_collect(&mut loop_state, AgentInput::follow_up("hello"));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![
            AssistantItem::Text("Let me inspect that.".to_string()),
            AssistantItem::ToolCall(tool_call.clone()),
        ]);

        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            },
        ));

        assert_eq!(
            records,
            vec![
                ContextItem::TurnStarted { turn_id: TurnId(1) },
                ContextItem::UserMessage("hello".to_string()),
                ContextItem::AssistantMessage(assistant.clone()),
                ContextItem::ToolCallStarted {
                    turn_id: TurnId(1),
                    tool_call: tool_call.clone(),
                },
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestTool {
                action_id: ActionId(2),
                turn_id: TurnId(1),
                tool_call: tool_call.clone(),
            }]
        );
        assert_eq!(
            loop_state.state,
            AgentState::RunningTools {
                turn_id: TurnId(1),
                tool_calls: vec![tool_call],
                tool_action_ids: vec![ActionId(2)],
                completed_results: vec![None],
                next_result_index: 0,
            }
        );
    }

    #[test]
    fn model_failure_finishes_the_turn_as_crashed() {
        let mut loop_state = AgentCoreLoop::new();
        let mut records = drive_collect(&mut loop_state, AgentInput::follow_up("hello"));
        loop_state.drain_actions();

        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ModelFailed {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                error: "provider failed".to_string(),
            },
        ));

        assert_eq!(
            records,
            vec![
                ContextItem::TurnStarted { turn_id: TurnId(1) },
                ContextItem::UserMessage("hello".to_string()),
                ContextItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Crashed,
                },
            ]
        );
        assert!(loop_state.drain_actions().is_empty());
        assert!(loop_state.is_idle());
    }

    #[test]
    fn tool_completion_appends_a_result_and_resumes_the_model() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        drive_collect(&mut loop_state, AgentInput::follow_up("hello"));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![AssistantItem::ToolCall(tool_call.clone())]);
        drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant,
            },
        );
        loop_state.drain_actions();

        let result = successful_tool_result(tool_call.id, "bash");
        let records = drive_collect(
            &mut loop_state,
            AgentInput::ToolCompleted {
                action_id: ActionId(2),
                turn_id: TurnId(1),
                result: result.clone(),
            },
        );

        assert_eq!(records.last(), Some(&ContextItem::ToolResult(result)));
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestModel {
                action_id: ActionId(3),
                turn_id: TurnId(1)
            }]
        );
        assert_eq!(
            loop_state.state,
            AgentState::RunningModel {
                action_id: ActionId(3),
                turn_id: TurnId(1)
            }
        );
    }

    #[test]
    fn queued_completions_preserve_arrival_order() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        drive_collect(&mut loop_state, AgentInput::follow_up("hello"));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![AssistantItem::ToolCall(tool_call.clone())]);
        let result = successful_tool_result(tool_call.id, "bash");

        loop_state
            .enqueue_input(AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant,
            })
            .expect("matching model completion is valid");
        loop_state
            .enqueue_input(AgentInput::ToolCompleted {
                action_id: ActionId(2),
                turn_id: TurnId(1),
                result: result.clone(),
            })
            .expect("matching tool completion is valid");
        loop_state.drive();

        let records = loop_state.drain_records();
        let expected_result_record = ContextItem::ToolResult(result);
        assert!(records
            .iter()
            .any(|record| record == &expected_result_record));
        assert_eq!(
            loop_state.drain_actions(),
            vec![
                AgentAction::RequestTool {
                    action_id: ActionId(2),
                    turn_id: TurnId(1),
                    tool_call,
                },
                AgentAction::RequestModel {
                    action_id: ActionId(3),
                    turn_id: TurnId(1)
                },
            ]
        );
    }

    #[test]
    fn multiple_tool_calls_run_in_parallel_and_results_are_recorded_in_source_order() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        let mut records = drive_collect(&mut loop_state, AgentInput::follow_up("hello"));
        loop_state.drain_actions();

        let first = tool_call(&mut next_tool_call_id, "bash");
        let second = tool_call(&mut next_tool_call_id, "read");
        let assistant = assistant_message(vec![
            AssistantItem::ToolCall(first.clone()),
            AssistantItem::ToolCall(second.clone()),
        ]);
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant,
            },
        ));

        assert_eq!(
            loop_state.drain_actions(),
            vec![
                AgentAction::RequestTool {
                    action_id: ActionId(2),
                    turn_id: TurnId(1),
                    tool_call: first.clone(),
                },
                AgentAction::RequestTool {
                    action_id: ActionId(3),
                    turn_id: TurnId(1),
                    tool_call: second.clone(),
                },
            ]
        );
        assert_eq!(
            records.last(),
            Some(&ContextItem::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: second.clone(),
            })
        );

        let second_result = successful_tool_result(second.id, "read");
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ToolCompleted {
                action_id: ActionId(3),
                turn_id: TurnId(1),
                result: second_result.clone(),
            },
        ));
        records.extend(drive_collect(&mut loop_state, AgentInput::steer("urgent")));

        assert_eq!(
            records.last(),
            Some(&ContextItem::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: second.clone(),
            })
        );
        assert!(loop_state.drain_actions().is_empty());
        assert!(matches!(
            loop_state.state,
            AgentState::RunningTools {
                next_result_index: 0,
                ..
            }
        ));

        let first_result = successful_tool_result(first.id, "bash");
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ToolCompleted {
                action_id: ActionId(2),
                turn_id: TurnId(1),
                result: first_result.clone(),
            },
        ));

        assert_eq!(
            records.last(),
            Some(&ContextItem::ToolResult(second_result.clone()))
        );
        assert_eq!(records[5], ContextItem::ToolResult(first_result));
        assert_eq!(records[6], ContextItem::ToolResult(second_result));
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestModel {
                action_id: ActionId(4),
                turn_id: TurnId(1)
            }]
        );
        assert_eq!(loop_state.mailbox.steer_len(), 1);
        assert_eq!(
            loop_state.state,
            AgentState::RunningModel {
                action_id: ActionId(4),
                turn_id: TurnId(1)
            }
        );
    }

    #[test]
    fn interrupting_a_running_tool_closes_the_turn_and_starts_queued_steer_work() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        let mut records = drive_collect(&mut loop_state, AgentInput::follow_up("initial"));
        loop_state.drain_actions();

        let tool_call = tool_call(&mut next_tool_call_id, "bash");
        let assistant = assistant_message(vec![AssistantItem::ToolCall(tool_call.clone())]);
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant,
            },
        ));
        loop_state.drain_actions();

        records.extend(drive_collect(&mut loop_state, AgentInput::steer("urgent")));

        assert!(loop_state.drain_actions().is_empty());

        records.extend(drive_collect(&mut loop_state, AgentInput::Interrupt));

        assert_eq!(
            records,
            vec![
                ContextItem::TurnStarted { turn_id: TurnId(1) },
                ContextItem::UserMessage("initial".to_string()),
                ContextItem::AssistantMessage(assistant_message(vec![AssistantItem::ToolCall(
                    tool_call.clone(),
                )])),
                ContextItem::ToolCallStarted {
                    turn_id: TurnId(1),
                    tool_call: tool_call.clone(),
                },
                ContextItem::ToolResult(ToolResultMessage::interrupted(tool_call.id, "bash")),
                ContextItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Interrupted,
                },
                ContextItem::TurnStarted { turn_id: TurnId(2) },
                ContextItem::UserMessage("urgent".to_string()),
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![
                AgentAction::CancelTurn { turn_id: TurnId(1) },
                AgentAction::RequestModel {
                    action_id: ActionId(3),
                    turn_id: TurnId(2)
                },
            ]
        );
        assert_eq!(
            loop_state.state,
            AgentState::RunningModel {
                action_id: ActionId(3),
                turn_id: TurnId(2)
            }
        );
    }

    #[test]
    fn interrupting_parallel_tools_cancels_the_turn_and_records_unfinished_tools() {
        let mut loop_state = AgentCoreLoop::new();
        let mut next_tool_call_id = ToolCallId::first();
        let mut records = drive_collect(&mut loop_state, AgentInput::follow_up("initial"));
        loop_state.drain_actions();

        let first = tool_call(&mut next_tool_call_id, "bash");
        let second = tool_call(&mut next_tool_call_id, "read");
        let assistant = assistant_message(vec![
            AssistantItem::ToolCall(first.clone()),
            AssistantItem::ToolCall(second.clone()),
        ]);
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant,
            },
        ));
        loop_state.drain_actions();

        let second_result = successful_tool_result(second.id, "read");
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ToolCompleted {
                action_id: ActionId(3),
                turn_id: TurnId(1),
                result: second_result.clone(),
            },
        ));
        records.extend(drive_collect(&mut loop_state, AgentInput::Interrupt));

        assert_eq!(
            records.last(),
            Some(&ContextItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Interrupted,
            })
        );
        assert_eq!(
            records[5],
            ContextItem::ToolResult(ToolResultMessage::interrupted(first.id, "bash"))
        );
        assert_eq!(records[6], ContextItem::ToolResult(second_result));
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::CancelTurn { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.state, AgentState::Idle);
    }

    #[test]
    fn interrupting_a_running_model_without_queued_work_finishes_interrupted() {
        let mut loop_state = AgentCoreLoop::new();
        let mut records = drive_collect(&mut loop_state, AgentInput::follow_up("hello"));
        loop_state.drain_actions();

        records.extend(drive_collect(&mut loop_state, AgentInput::Interrupt));

        assert_eq!(
            records,
            vec![
                ContextItem::TurnStarted { turn_id: TurnId(1) },
                ContextItem::UserMessage("hello".to_string()),
                ContextItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Interrupted,
                },
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::CancelTurn { turn_id: TurnId(1) }]
        );
        assert_eq!(loop_state.state, AgentState::Idle);
    }

    #[test]
    fn idle_turn_from_tagged_input_produces_injected_transcript_entry() {
        let mut loop_state = AgentCoreLoop::new();

        let records = drive_collect(
            &mut loop_state,
            AgentInput::steer_tagged("parent-session", "agent_directive", "please do X"),
        );

        let mut expected_metadata = BTreeMap::new();
        expected_metadata.insert("from".to_string(), "parent-session".to_string());

        assert_eq!(
            records,
            vec![
                ContextItem::TurnStarted { turn_id: TurnId(1) },
                ContextItem::Injected(InjectedMessage {
                    kind: "agent_directive".to_string(),
                    content: "please do X".to_string(),
                    metadata: expected_metadata,
                }),
            ]
        );
        assert_eq!(
            loop_state.drain_actions(),
            vec![AgentAction::RequestModel {
                action_id: ActionId(1),
                turn_id: TurnId(1)
            }]
        );
    }

    #[test]
    fn idle_turn_from_untagged_input_produces_user_message() {
        let mut loop_state = AgentCoreLoop::new();

        let records = drive_collect(&mut loop_state, AgentInput::steer("human steer"));

        assert_eq!(
            records,
            vec![
                ContextItem::TurnStarted { turn_id: TurnId(1) },
                ContextItem::UserMessage("human steer".to_string()),
            ]
        );
    }

    #[test]
    fn stale_completions_are_ignored_after_an_interrupt() {
        let mut loop_state = AgentCoreLoop::new();
        let mut records = drive_collect(&mut loop_state, AgentInput::follow_up("hello"));
        loop_state.drain_actions();
        records.extend(drive_collect(&mut loop_state, AgentInput::Interrupt));
        loop_state.drain_actions();

        let stale_assistant = assistant_message(vec![AssistantItem::Text("stale".to_string())]);
        records.extend(drive_collect(
            &mut loop_state,
            AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant: stale_assistant,
            },
        ));

        assert_eq!(records.len(), 3);
        assert!(loop_state.drain_actions().is_empty());
        assert_eq!(loop_state.state, AgentState::Idle);
    }
}
