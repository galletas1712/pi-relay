use crate::event::TurnOutcome;
use crate::ids::TurnId;
use crate::mailbox::MailboxEvent;
use crate::message::{ToolCall, ToolResultMessage};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentStateStep {
    ConsumeEvent,
    DropEvent,
    Wait,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterruptedTurn {
    pub(crate) turn_id: TurnId,
    pub(crate) tool_call: Option<ToolCall>,
    pub(crate) cancel_active: bool,
}

impl AgentState {
    pub(crate) fn from_tail_outcome(outcome: Option<TurnOutcome>) -> Self {
        match outcome {
            Some(TurnOutcome::Interrupted) => Self::Interrupted,
            Some(TurnOutcome::Crashed) => Self::Crashed,
            Some(TurnOutcome::Graceful) | None => Self::Idle,
        }
    }

    pub(crate) fn step(&self, event: &MailboxEvent) -> AgentStateStep {
        let Some(active_turn_id) = self.active_turn_id() else {
            return AgentStateStep::DropEvent;
        };

        if event.turn_id() != active_turn_id {
            return AgentStateStep::DropEvent;
        }

        match (self, event) {
            (Self::RunningModel { .. }, MailboxEvent::AssistantMessage { .. }) => {
                AgentStateStep::ConsumeEvent
            }
            (Self::RunningTool { tool_call, .. }, MailboxEvent::ToolResult { result, .. })
                if tool_call.id == result.tool_call_id
                    && tool_call.tool_name == result.tool_name =>
            {
                AgentStateStep::ConsumeEvent
            }
            (Self::ReadyToContinue { .. }, MailboxEvent::ToolCallReady { .. }) => {
                AgentStateStep::ConsumeEvent
            }
            (Self::RunningTool { .. }, MailboxEvent::ToolCallReady { .. }) => AgentStateStep::Wait,
            _ => AgentStateStep::DropEvent,
        }
    }

    pub(crate) fn start_turn(&mut self, turn_id: TurnId) -> bool {
        match self {
            Self::Idle | Self::Interrupted | Self::Crashed => {
                *self = Self::RunningModel { turn_id };
                true
            }
            Self::RunningModel { .. } | Self::RunningTool { .. } | Self::ReadyToContinue { .. } => {
                false
            }
        }
    }

    pub(crate) fn resume_model(&mut self) -> Option<TurnId> {
        let Self::ReadyToContinue { turn_id } = self else {
            return None;
        };

        let turn_id = *turn_id;
        *self = Self::RunningModel { turn_id };
        Some(turn_id)
    }

    pub(crate) fn finish_model_turn(&mut self, turn_id: TurnId) -> bool {
        match self {
            Self::RunningModel {
                turn_id: active_turn_id,
            } if *active_turn_id == turn_id => {
                *self = Self::Idle;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn start_tool(&mut self, turn_id: TurnId, tool_call: ToolCall) -> bool {
        match self {
            Self::RunningModel {
                turn_id: active_turn_id,
            }
            | Self::ReadyToContinue {
                turn_id: active_turn_id,
            } if *active_turn_id == turn_id => {
                *self = Self::RunningTool { turn_id, tool_call };
                true
            }
            _ => false,
        }
    }

    pub(crate) fn finish_tool(&mut self, turn_id: TurnId, result: &ToolResultMessage) -> bool {
        match self {
            Self::RunningTool {
                turn_id: active_turn_id,
                tool_call,
            } if *active_turn_id == turn_id
                && tool_call.id == result.tool_call_id
                && tool_call.tool_name == result.tool_name =>
            {
                *self = Self::ReadyToContinue { turn_id };
                true
            }
            _ => false,
        }
    }

    pub(crate) fn interrupt(&mut self) -> Option<InterruptedTurn> {
        match self.clone() {
            Self::Idle | Self::Interrupted | Self::Crashed => None,
            Self::ReadyToContinue { turn_id } => {
                *self = Self::Interrupted;
                Some(InterruptedTurn {
                    turn_id,
                    tool_call: None,
                    cancel_active: false,
                })
            }
            Self::RunningModel { turn_id } => {
                *self = Self::Interrupted;
                Some(InterruptedTurn {
                    turn_id,
                    tool_call: None,
                    cancel_active: true,
                })
            }
            Self::RunningTool { turn_id, tool_call } => {
                *self = Self::Interrupted;
                Some(InterruptedTurn {
                    turn_id,
                    tool_call: Some(tool_call),
                    cancel_active: true,
                })
            }
        }
    }

    fn active_turn_id(&self) -> Option<TurnId> {
        match self {
            Self::RunningModel { turn_id }
            | Self::RunningTool { turn_id, .. }
            | Self::ReadyToContinue { turn_id } => Some(*turn_id),
            Self::Idle | Self::Interrupted | Self::Crashed => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ToolCallId;
    use crate::message::{AssistantMessage, ToolResultMessage, ToolResultStatus};

    fn tool_call(name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId(1),
            tool_name: name.to_string(),
            args_json: "{}".to_string(),
        }
    }

    fn tool_result(name: &str) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: ToolCallId(1),
            tool_name: name.to_string(),
            output: "ok".to_string(),
            status: ToolResultStatus::Success,
        }
    }

    #[test]
    fn terminal_states_drop_late_events() {
        let event = MailboxEvent::AssistantMessage {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        };

        assert_eq!(AgentState::Idle.step(&event), AgentStateStep::DropEvent);
        assert_eq!(
            AgentState::Interrupted.step(&event),
            AgentStateStep::DropEvent
        );
        assert_eq!(AgentState::Crashed.step(&event), AgentStateStep::DropEvent);
    }

    #[test]
    fn running_model_consumes_only_matching_assistant_events() {
        let state = AgentState::RunningModel { turn_id: TurnId(2) };
        let matching = MailboxEvent::AssistantMessage {
            turn_id: TurnId(2),
            assistant: AssistantMessage { items: Vec::new() },
        };
        let stale = MailboxEvent::AssistantMessage {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        };

        assert_eq!(state.step(&matching), AgentStateStep::ConsumeEvent);
        assert_eq!(state.step(&stale), AgentStateStep::DropEvent);
    }

    #[test]
    fn running_tool_consumes_matching_result_and_waits_on_future_tool_calls() {
        let state = AgentState::RunningTool {
            turn_id: TurnId(1),
            tool_call: tool_call("bash"),
        };
        let result = MailboxEvent::ToolResult {
            turn_id: TurnId(1),
            result: tool_result("bash"),
        };
        let future_tool = MailboxEvent::ToolCallReady {
            turn_id: TurnId(1),
            tool_call: tool_call("read"),
        };

        assert_eq!(state.step(&result), AgentStateStep::ConsumeEvent);
        assert_eq!(state.step(&future_tool), AgentStateStep::Wait);
    }

    #[test]
    fn turn_and_tool_transitions_move_between_live_states() {
        let mut state = AgentState::Idle;
        let tool_call = tool_call("bash");
        let result = tool_result("bash");

        assert!(state.start_turn(TurnId(1)));
        assert_eq!(state, AgentState::RunningModel { turn_id: TurnId(1) });

        assert!(state.start_tool(TurnId(1), tool_call.clone()));
        assert_eq!(
            state,
            AgentState::RunningTool {
                turn_id: TurnId(1),
                tool_call
            }
        );

        assert!(state.finish_tool(TurnId(1), &result));
        assert_eq!(state, AgentState::ReadyToContinue { turn_id: TurnId(1) });

        assert_eq!(state.resume_model(), Some(TurnId(1)));
        assert_eq!(state, AgentState::RunningModel { turn_id: TurnId(1) });

        assert!(state.finish_model_turn(TurnId(1)));
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn interrupt_transitions_running_state_and_reports_cleanup_work() {
        let mut state = AgentState::RunningTool {
            turn_id: TurnId(3),
            tool_call: tool_call("bash"),
        };

        let interrupted = state
            .interrupt()
            .expect("running tool should be interruptible");

        assert_eq!(state, AgentState::Interrupted);
        assert_eq!(interrupted.turn_id, TurnId(3));
        assert_eq!(interrupted.tool_call, Some(tool_call("bash")));
        assert!(interrupted.cancel_active);
    }
}
