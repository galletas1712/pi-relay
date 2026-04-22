use std::collections::VecDeque;

use crate::event::AgentEvent;
use crate::ids::TurnId;
use crate::state::AgentState;

/// Volatile prioritized queues feeding the live agent FSM.
///
/// Mailbox contents are intentionally not durable: if the process dies, session
/// recovery is driven from TranscriptRecords instead.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Mailbox {
    notifications: VecDeque<AgentEvent>,
    steer: VecDeque<String>,
    follow_up: VecDeque<String>,
    interrupt_requested: bool,
}

impl Mailbox {
    pub(crate) fn push_notification_front(&mut self, notification: AgentEvent) {
        self.notifications.push_front(notification);
    }

    #[cfg(test)]
    fn push_notification_back(&mut self, notification: AgentEvent) {
        self.notifications.push_back(notification);
    }

    pub(crate) fn request_interrupt(&mut self) {
        self.interrupt_requested = true;
    }

    pub fn push_steer(&mut self, input: impl Into<String>) {
        self.steer.push_back(input.into());
    }

    pub fn push_follow_up(&mut self, input: impl Into<String>) {
        self.follow_up.push_back(input.into());
    }

    pub fn pop_user_input(&mut self) -> Option<String> {
        self.steer
            .pop_front()
            .or_else(|| self.follow_up.pop_front())
    }

    pub(crate) fn next_event(
        &mut self,
        state: &AgentState,
        next_turn_id: TurnId,
    ) -> Option<AgentEvent> {
        if self.interrupt_requested {
            self.interrupt_requested = false;
            if matches!(
                state,
                AgentState::RunningModel { .. }
                    | AgentState::RunningTools { .. }
                    | AgentState::ReadyToContinue { .. }
            ) {
                return Some(AgentEvent::Interrupt);
            }
        }

        if let Some(notification) = self.notifications.pop_front() {
            return Some(notification);
        }

        match state {
            AgentState::ReadyToContinue { .. } => Some(AgentEvent::ContinueModel),
            AgentState::Idle | AgentState::Interrupted | AgentState::Crashed => {
                self.pop_user_input().map(|input| AgentEvent::StartTurn {
                    turn_id: next_turn_id,
                    input,
                })
            }
            AgentState::RunningModel { .. } | AgentState::RunningTools { .. } => None,
        }
    }

    pub fn notification_len(&self) -> usize {
        self.notifications.len()
    }

    pub fn steer_len(&self) -> usize {
        self.steer.len()
    }

    pub fn follow_up_len(&self) -> usize {
        self.follow_up.len()
    }

    pub fn total_len(&self) -> usize {
        self.notifications.len() + self.steer.len() + self.follow_up.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentEvent;
    use crate::ids::ToolCallId;
    use crate::message::{AssistantMessage, ToolResultMessage, ToolResultStatus};

    fn tool_result(id: u64, name: &str) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: ToolCallId(id),
            tool_name: name.to_string(),
            output: "ok".to_string(),
            status: ToolResultStatus::Success,
        }
    }

    #[test]
    fn notification_queue_behaves_like_a_deque() {
        let mut mailbox = Mailbox::default();
        let later = AgentEvent::ToolCompleted {
            turn_id: TurnId(1),
            result: tool_result(2, "read"),
        };
        let now = AgentEvent::ModelCompleted {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        };

        mailbox.push_notification_back(later.clone());
        mailbox.push_notification_front(now.clone());

        let state = AgentState::RunningModel { turn_id: TurnId(1) };
        assert_eq!(mailbox.next_event(&state, TurnId(99)), Some(now));
        assert_eq!(mailbox.next_event(&state, TurnId(99)), Some(later));
        assert_eq!(mailbox.next_event(&state, TurnId(99)), None);
    }

    #[test]
    fn user_input_prefers_steer_before_follow_up() {
        let mut mailbox = Mailbox::default();
        mailbox.push_follow_up("follow-up");
        mailbox.push_steer("steer");

        assert_eq!(mailbox.pop_user_input(), Some("steer".to_string()));
        assert_eq!(mailbox.pop_user_input(), Some("follow-up".to_string()));
        assert_eq!(mailbox.pop_user_input(), None);
    }

    #[test]
    fn priority_order_is_interrupt_then_notification_then_steer_then_follow_up() {
        let mut mailbox = Mailbox::default();
        mailbox.push_follow_up("follow-up");
        mailbox.push_steer("steer");
        mailbox.push_notification_back(AgentEvent::ToolCompleted {
            turn_id: TurnId(1),
            result: tool_result(9, "bash"),
        });
        mailbox.request_interrupt();

        assert!(matches!(
            mailbox.next_event(
                &AgentState::RunningTools {
                    turn_id: TurnId(1),
                    tool_calls: Vec::new(),
                    completed_results: Vec::new(),
                    next_result_index: 0,
                },
                TurnId(99)
            ),
            Some(AgentEvent::Interrupt)
        ));
        assert!(matches!(
            mailbox.next_event(
                &AgentState::RunningTools {
                    turn_id: TurnId(1),
                    tool_calls: Vec::new(),
                    completed_results: Vec::new(),
                    next_result_index: 0,
                },
                TurnId(99)
            ),
            Some(AgentEvent::ToolCompleted { .. })
        ));
        assert_eq!(
            mailbox.next_event(
                &AgentState::ReadyToContinue { turn_id: TurnId(1) },
                TurnId(99)
            ),
            Some(AgentEvent::ContinueModel)
        );
        assert_eq!(
            mailbox.next_event(&AgentState::Idle, TurnId(2)),
            Some(AgentEvent::StartTurn {
                turn_id: TurnId(2),
                input: "steer".to_string()
            })
        );
        assert_eq!(
            mailbox.next_event(&AgentState::Idle, TurnId(3)),
            Some(AgentEvent::StartTurn {
                turn_id: TurnId(3),
                input: "follow-up".to_string()
            })
        );
    }
}
