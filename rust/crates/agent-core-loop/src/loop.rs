use std::collections::VecDeque;

use crate::event::{AgentAction, AgentEvent, TurnOutcome};
use crate::ids::TurnId;
use crate::mailbox::{Mailbox, MailboxEvent};
use crate::message::{
    AssistantMessage, CompactMessage, ToolCall, ToolResultMessage, UserInput, UserMessage,
};
use crate::transcript::Transcript;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentStateStep {
    ConsumeEvent,
    DropEvent,
    Wait,
}

impl AgentState {
    fn step(&self, event: &MailboxEvent) -> AgentStateStep {
        let Some(active_turn_id) = self.active_turn_id() else {
            return AgentStateStep::DropEvent;
        };

        if event.turn_id() != active_turn_id {
            return AgentStateStep::DropEvent;
        }

        match (self, event) {
            (AgentState::RunningModel { .. }, MailboxEvent::AssistantMessage { .. }) => {
                AgentStateStep::ConsumeEvent
            }
            (
                AgentState::RunningTool { tool_call, .. },
                MailboxEvent::ToolResult { result, .. },
            ) if tool_call.id == result.tool_call_id && tool_call.tool_name == result.tool_name => {
                AgentStateStep::ConsumeEvent
            }
            (AgentState::ReadyToContinue { .. }, MailboxEvent::ToolCallReady { .. }) => {
                AgentStateStep::ConsumeEvent
            }
            (AgentState::RunningTool { .. }, MailboxEvent::ToolCallReady { .. }) => {
                AgentStateStep::Wait
            }
            _ => AgentStateStep::DropEvent,
        }
    }

    fn active_turn_id(&self) -> Option<TurnId> {
        match self {
            AgentState::RunningModel { turn_id }
            | AgentState::RunningTool { turn_id, .. }
            | AgentState::ReadyToContinue { turn_id } => Some(*turn_id),
            AgentState::Idle | AgentState::Interrupted | AgentState::Crashed => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentInput {
    Interrupt,
    Steer(UserInput),
    FollowUp(UserInput),
    Event(MailboxEvent),
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

    pub fn from_events(events: Vec<AgentEvent>) -> Self {
        Self::from_transcript(Transcript::from_events(events))
    }

    pub fn from_transcript(transcript: Transcript) -> Self {
        let last_turn_id = transcript.last_turn_id();
        let state = Self::state_from_transcript(&transcript);

        Self {
            mailbox: Mailbox::default(),
            transcript,
            state,
            last_turn_id,
            action_outbox: VecDeque::new(),
            interrupt_requested: false,
        }
    }

    fn state_from_transcript(transcript: &Transcript) -> AgentState {
        match transcript.tail_outcome() {
            Some(TurnOutcome::Interrupted) => AgentState::Interrupted,
            Some(TurnOutcome::Crashed) => AgentState::Crashed,
            Some(TurnOutcome::Graceful) | None => AgentState::Idle,
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
            AgentInput::Event(event) => {
                // External completions should preempt queued future work for the current turn.
                self.mailbox.push_event_front(event);
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

            if self.consume_ready_event() {
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

    fn consume_ready_event(&mut self) -> bool {
        let Some(event) = self.mailbox.front_event().cloned() else {
            return false;
        };

        match self.state.step(&event) {
            AgentStateStep::ConsumeEvent => {
                let event = self.pop_event();
                self.handle_mailbox_event(event);
                true
            }
            AgentStateStep::DropEvent => {
                let _ = self.pop_event();
                true
            }
            AgentStateStep::Wait => false,
        }
    }

    fn resume_model_if_ready(&mut self) -> bool {
        let turn_id = match &self.state {
            AgentState::ReadyToContinue { turn_id } => *turn_id,
            _ => return false,
        };

        self.state = AgentState::RunningModel { turn_id };
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

    fn pop_event(&mut self) -> MailboxEvent {
        self.mailbox
            .pop_event()
            .expect("front event disappeared before it could be consumed")
    }

    fn start_turn(&mut self, input: UserInput) {
        self.last_turn_id = self.last_turn_id.next();
        let turn_id = self.last_turn_id;
        let user_message = self.create_user_message(input);
        self.state = AgentState::RunningModel { turn_id };

        self.append_event(AgentEvent::TurnStarted { turn_id });
        self.append_event(AgentEvent::UserMessage(user_message));
        self.enqueue_action(AgentAction::RequestModel { turn_id });
    }

    fn handle_mailbox_event(&mut self, event: MailboxEvent) {
        match event {
            MailboxEvent::AssistantMessage { turn_id, assistant } => {
                self.on_assistant_message(turn_id, assistant);
            }
            MailboxEvent::ToolCallReady { turn_id, tool_call } => {
                self.start_tool_call(turn_id, tool_call);
            }
            MailboxEvent::ToolResult { turn_id, result } => {
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

        self.append_event(AgentEvent::AssistantMessage(assistant.clone()));

        let mut tool_calls = assistant.tool_calls().cloned();
        let Some(first_tool_call) = tool_calls.next() else {
            self.state = AgentState::Idle;
            self.append_event(AgentEvent::TurnFinished {
                turn_id,
                outcome: TurnOutcome::Graceful,
            });
            return;
        };

        for tool_call in tool_calls {
            self.mailbox
                .push_event_back(MailboxEvent::ToolCallReady { turn_id, tool_call });
        }

        self.start_tool_call(turn_id, first_tool_call);
    }

    fn start_tool_call(&mut self, turn_id: TurnId, tool_call: ToolCall) {
        self.state = AgentState::RunningTool {
            turn_id,
            tool_call: tool_call.clone(),
        };
        self.append_event(AgentEvent::ToolCallStarted {
            turn_id,
            tool_call: tool_call.clone(),
        });
        self.enqueue_action(AgentAction::RequestTool { turn_id, tool_call });
    }

    fn on_tool_result(&mut self, turn_id: TurnId, result: ToolResultMessage) {
        let running_tool_call = match &self.state {
            AgentState::RunningTool {
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

        self.append_event(AgentEvent::ToolResult(result));
        self.state = AgentState::ReadyToContinue { turn_id };
    }

    fn handle_interrupt(&mut self) -> bool {
        if !self.interrupt_requested {
            return false;
        }

        self.interrupt_requested = false;

        match self.state.clone() {
            AgentState::Idle | AgentState::Interrupted | AgentState::Crashed => false,
            AgentState::ReadyToContinue { turn_id } => {
                self.state = AgentState::Interrupted;
                self.append_event(AgentEvent::TurnFinished {
                    turn_id,
                    outcome: TurnOutcome::Interrupted,
                });
                true
            }
            AgentState::RunningModel { turn_id } => {
                self.state = AgentState::Interrupted;

                self.append_event(AgentEvent::TurnFinished {
                    turn_id,
                    outcome: TurnOutcome::Interrupted,
                });
                self.enqueue_action(AgentAction::CancelActive { turn_id });
                true
            }
            AgentState::RunningTool { turn_id, tool_call } => {
                self.state = AgentState::Interrupted;

                let interrupted =
                    ToolResultMessage::interrupted(tool_call.id, tool_call.tool_name.clone());
                self.append_event(AgentEvent::ToolResult(interrupted));
                self.append_event(AgentEvent::TurnFinished {
                    turn_id,
                    outcome: TurnOutcome::Interrupted,
                });
                self.enqueue_action(AgentAction::CancelActive { turn_id });
                true
            }
        }
    }

    fn create_user_message(&mut self, input: UserInput) -> UserMessage {
        UserMessage { text: input.text }
    }

    fn append_event(&mut self, event: AgentEvent) {
        self.transcript.append(event);
    }

    fn enqueue_action(&mut self, action: AgentAction) {
        self.action_outbox.push_back(action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentAction, AgentEvent, TurnOutcome};
    use crate::ids::ToolCallId;
    use crate::message::AssistantItem;

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
            loop_state.transcript.events(),
            vec![
                AgentEvent::TurnStarted { turn_id: TurnId(1) },
                AgentEvent::UserMessage(UserMessage {
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

        loop_state.on_input(AgentInput::Event(MailboxEvent::AssistantMessage {
            turn_id: TurnId(1),
            assistant: assistant.clone(),
        }));

        assert_eq!(
            loop_state.transcript.events(),
            vec![
                AgentEvent::TurnStarted { turn_id: TurnId(1) },
                AgentEvent::UserMessage(UserMessage {
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
        loop_state.on_input(AgentInput::Event(MailboxEvent::AssistantMessage {
            turn_id: TurnId(1),
            assistant,
        }));
        loop_state.drain_actions();

        let result = successful_tool_result(tool_call.id, "bash");
        loop_state.on_input(AgentInput::Event(MailboxEvent::ToolResult {
            turn_id: TurnId(1),
            result: result.clone(),
        }));

        assert_eq!(
            loop_state.transcript.events().last(),
            Some(&AgentEvent::ToolResult(result))
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
        loop_state.on_input(AgentInput::Event(MailboxEvent::AssistantMessage {
            turn_id: TurnId(1),
            assistant,
        }));
        loop_state.drain_actions();

        let first_result = successful_tool_result(first.id, "bash");
        loop_state.on_input(AgentInput::Event(MailboxEvent::ToolResult {
            turn_id: TurnId(1),
            result: first_result.clone(),
        }));

        assert_eq!(
            loop_state.transcript.events().last(),
            Some(&AgentEvent::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: second.clone(),
            })
        );
        assert_eq!(
            loop_state.transcript.events()[4],
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
        loop_state.on_input(AgentInput::Event(MailboxEvent::AssistantMessage {
            turn_id: TurnId(1),
            assistant,
        }));
        loop_state.drain_actions();

        loop_state.on_input(AgentInput::Steer(UserInput::from("urgent")));

        assert!(loop_state.drain_actions().is_empty());

        loop_state.on_input(AgentInput::Interrupt);

        assert_eq!(
            loop_state.transcript.events(),
            vec![
                AgentEvent::TurnStarted { turn_id: TurnId(1) },
                AgentEvent::UserMessage(UserMessage {
                    text: "initial".to_string(),
                }),
                AgentEvent::AssistantMessage(assistant_message(vec![AssistantItem::ToolCall(
                    tool_call.clone(),
                )])),
                AgentEvent::ToolCallStarted {
                    turn_id: TurnId(1),
                    tool_call: tool_call.clone(),
                },
                AgentEvent::ToolResult(ToolResultMessage::interrupted(tool_call.id, "bash")),
                AgentEvent::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Interrupted,
                },
                AgentEvent::TurnStarted { turn_id: TurnId(2) },
                AgentEvent::UserMessage(UserMessage {
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
            loop_state.transcript.events(),
            vec![
                AgentEvent::TurnStarted { turn_id: TurnId(1) },
                AgentEvent::UserMessage(UserMessage {
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
        loop_state.on_input(AgentInput::Event(MailboxEvent::AssistantMessage {
            turn_id: TurnId(1),
            assistant: stale_assistant,
        }));

        assert_eq!(loop_state.transcript.events().len(), 3);
        assert!(loop_state.drain_actions().is_empty());
        assert_eq!(loop_state.state, AgentState::Interrupted);
    }

    #[test]
    fn compact_transcript_filters_to_user_and_assistant_messages() {
        let mut loop_state = AgentCoreLoop::new();
        loop_state.on_input(AgentInput::FollowUp(UserInput::from("hello")));
        loop_state.drain_actions();

        let assistant = assistant_message(vec![AssistantItem::Text("hi".to_string())]);
        loop_state.on_input(AgentInput::Event(MailboxEvent::AssistantMessage {
            turn_id: TurnId(1),
            assistant: assistant.clone(),
        }));

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
            AgentEvent::TurnStarted { turn_id: TurnId(7) },
            AgentEvent::UserMessage(UserMessage {
                text: "hello".to_string(),
            }),
        ];

        let loop_state = AgentCoreLoop::from_events(transcript);

        assert_eq!(
            loop_state.transcript.events(),
            vec![
                AgentEvent::TurnStarted { turn_id: TurnId(7) },
                AgentEvent::UserMessage(UserMessage {
                    text: "hello".to_string(),
                }),
                AgentEvent::TurnFinished {
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
            AgentEvent::TurnStarted { turn_id: TurnId(2) },
            AgentEvent::UserMessage(UserMessage {
                text: "hello".to_string(),
            }),
            AgentEvent::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ];

        let loop_state = AgentCoreLoop::from_events(transcript.clone());

        assert_eq!(loop_state.transcript.events(), transcript.as_slice());
        assert_eq!(loop_state.state, AgentState::Idle);
        assert_eq!(loop_state.last_turn_id, TurnId(2));
    }
}
