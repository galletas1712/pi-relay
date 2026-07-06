use std::collections::VecDeque;

use crate::event::{AgentEvent, AgentInput, TurnInput};
use crate::state::AgentState;
use agent_vocab::{DaemonToolObservation, TurnId};

/// A queued turn input.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserInputEntry {
    pub(crate) content: TurnInput,
}

/// A queued daemon-authored observation. Kept distinct from user input so the
/// transcript can preserve authorship semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaemonObservationEntry {
    pub(crate) observation: DaemonToolObservation,
}

/// Volatile prioritized queues feeding the live agent FSM.
///
/// Mailbox contents are intentionally not durable: if the process dies, session
/// recovery is driven from persisted transcript items instead.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Mailbox {
    notifications: VecDeque<AgentEvent>,
    steer: VecDeque<UserInputEntry>,
    follow_up: VecDeque<UserInputEntry>,
    daemon_observations: VecDeque<DaemonObservationEntry>,
    interrupt_requested: bool,
}

impl Mailbox {
    pub(crate) fn push_input(&mut self, input: AgentInput) {
        match input {
            AgentInput::Interrupt => self.request_interrupt(),
            AgentInput::Steer { content } => {
                self.steer.push_back(UserInputEntry { content });
            }
            AgentInput::FollowUp { content } => {
                self.follow_up.push_back(UserInputEntry { content });
            }
            AgentInput::DaemonObservation { observation } => {
                self.daemon_observations
                    .push_back(DaemonObservationEntry { observation });
            }
            AgentInput::ModelCompleted {
                action_id,
                turn_id,
                assistant,
            } => {
                // External completions preempt queued user work, but preserve
                // arrival order relative to other completions.
                self.push_notification_back(AgentEvent::ModelCompleted {
                    action_id,
                    turn_id,
                    assistant,
                });
            }
            AgentInput::ModelFailed {
                action_id,
                turn_id,
                error,
            } => {
                // External completions/failures preempt queued user work, but
                // preserve arrival order relative to other notifications.
                self.push_notification_back(AgentEvent::ModelFailed {
                    action_id,
                    turn_id,
                    error,
                });
            }
            AgentInput::ToolCompleted {
                action_id,
                turn_id,
                result,
            } => {
                // External completions preempt queued user work, but preserve
                // arrival order relative to other completions.
                self.push_notification_back(AgentEvent::ToolCompleted {
                    action_id,
                    turn_id,
                    result,
                });
            }
        }
    }

    #[cfg(test)]
    fn push_notification_front(&mut self, notification: AgentEvent) {
        self.notifications.push_front(notification);
    }

    fn push_notification_back(&mut self, notification: AgentEvent) {
        self.notifications.push_back(notification);
    }

    fn request_interrupt(&mut self) {
        self.interrupt_requested = true;
    }

    #[cfg(test)]
    fn push_steer(&mut self, input: impl Into<String>) {
        self.steer.push_back(UserInputEntry {
            content: TurnInput(agent_vocab::UserMessage::text(input)),
        });
    }

    #[cfg(test)]
    fn push_follow_up(&mut self, input: impl Into<String>) {
        self.follow_up.push_back(UserInputEntry {
            content: TurnInput(agent_vocab::UserMessage::text(input)),
        });
    }

    /// Pop the next queued user input entry (steer before follow-up).
    fn pop_user_input_entry(&mut self) -> Option<UserInputEntry> {
        self.steer
            .pop_front()
            .or_else(|| self.follow_up.pop_front())
    }

    fn pop_daemon_observation_entry(&mut self) -> Option<DaemonObservationEntry> {
        self.daemon_observations.pop_front()
    }

    fn pop_steer_input_entry(&mut self) -> Option<UserInputEntry> {
        self.steer.pop_front()
    }

    /// Drain every queued turn-starting input as reconstructed `AgentInput`
    /// values, preserving dispatch order (steer, follow-up, then daemon
    /// observations).
    ///
    /// Notifications and interrupt state are left untouched.
    pub(crate) fn drain_pending_inputs(&mut self) -> Vec<AgentInput> {
        let mut drained = Vec::with_capacity(self.steer.len() + self.follow_up.len());
        drained.extend(self.steer.drain(..).map(|entry| AgentInput::Steer {
            content: entry.content,
        }));
        drained.extend(self.follow_up.drain(..).map(|entry| AgentInput::FollowUp {
            content: entry.content,
        }));
        drained.extend(self.daemon_observations.drain(..).map(|entry| {
            AgentInput::DaemonObservation {
                observation: entry.observation,
            }
        }));
        drained
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
            AgentState::ReadyToContinue { .. } => self
                .pop_steer_input_entry()
                .map(|entry| AgentEvent::Steer {
                    input: entry.content,
                })
                .or_else(|| {
                    self.pop_daemon_observation_entry()
                        .map(|entry| AgentEvent::DaemonObservation {
                            observation: entry.observation,
                        })
                })
                .or(Some(AgentEvent::ContinueModel)),
            AgentState::Idle => self
                .pop_user_input_entry()
                .map(|entry| AgentEvent::StartTurn {
                    turn_id: next_turn_id,
                    input: entry.content,
                })
                .or_else(|| {
                    self.pop_daemon_observation_entry().map(|entry| {
                        AgentEvent::StartDaemonObservationTurn {
                            turn_id: next_turn_id,
                            observation: entry.observation,
                        }
                    })
                }),
            AgentState::RunningModel { .. } | AgentState::RunningTools { .. } => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn steer_len(&self) -> usize {
        self.steer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentEvent;
    use agent_vocab::UserMessage;
    use agent_vocab::{
        ActionId, AssistantMessage, ToolCallId, ToolResultMessage, ToolResultStatus,
    };

    fn tool_result(id: u64, name: &str) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: ToolCallId::from_u64(id),
            tool_name: name.to_string(),
            output: "ok".to_string(),
            status: ToolResultStatus::Success,
        }
    }

    #[test]
    fn notification_queue_behaves_like_a_deque() {
        let mut mailbox = Mailbox::default();
        let later = AgentEvent::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: tool_result(2, "read"),
        };
        let now = AgentEvent::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        };

        mailbox.push_notification_back(later.clone());
        mailbox.push_notification_front(now.clone());

        let state = AgentState::RunningModel {
            action_id: ActionId(1),
            turn_id: TurnId(1),
        };
        assert_eq!(mailbox.next_event(&state, TurnId(99)), Some(now));
        assert_eq!(mailbox.next_event(&state, TurnId(99)), Some(later));
        assert_eq!(mailbox.next_event(&state, TurnId(99)), None);
    }

    #[test]
    fn user_input_prefers_steer_before_follow_up() {
        let mut mailbox = Mailbox::default();
        mailbox.push_follow_up("follow-up");
        mailbox.push_steer("steer");

        assert_eq!(
            mailbox.pop_user_input_entry().map(|e| e.content),
            Some(TurnInput(UserMessage::text("steer")))
        );
        assert_eq!(
            mailbox.pop_user_input_entry().map(|e| e.content),
            Some(TurnInput(UserMessage::text("follow-up")))
        );
        assert!(mailbox.pop_user_input_entry().is_none());
    }

    #[test]
    fn priority_order_is_interrupt_then_notification_then_steer_then_follow_up() {
        let mut mailbox = Mailbox::default();
        mailbox.push_follow_up("follow-up");
        mailbox.push_steer("steer");
        mailbox.push_notification_back(AgentEvent::ToolCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            result: tool_result(9, "bash"),
        });
        mailbox.request_interrupt();

        assert!(matches!(
            mailbox.next_event(
                &AgentState::RunningTools {
                    turn_id: TurnId(1),
                    tools: Vec::new(),
                    tool_index_by_action_id: std::collections::HashMap::new(),
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
                    tools: Vec::new(),
                    tool_index_by_action_id: std::collections::HashMap::new(),
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
            Some(AgentEvent::Steer {
                input: TurnInput(UserMessage::text("steer")),
            })
        );
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
                input: TurnInput(UserMessage::text("follow-up")),
            })
        );
    }

    #[test]
    fn user_steer_surfaces_user_turn_input() {
        let mut mailbox = Mailbox::default();
        mailbox.push_input(AgentInput::steer("plain"));

        let event = mailbox
            .next_event(&AgentState::Idle, TurnId(1))
            .expect("user steer should surface as StartTurn");

        assert_eq!(
            event,
            AgentEvent::StartTurn {
                turn_id: TurnId(1),
                input: TurnInput(UserMessage::text("plain")),
            }
        );
    }
}
