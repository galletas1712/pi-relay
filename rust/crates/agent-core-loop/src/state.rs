use crate::event::TurnOutcome;
use crate::ids::TurnId;
use crate::mailbox::MailboxEvent;
use crate::message::ToolCall;

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
}
