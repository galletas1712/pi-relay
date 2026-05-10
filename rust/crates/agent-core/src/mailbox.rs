use std::collections::VecDeque;

use crate::event::{AgentEvent, AgentInput, TurnOrigin};
use crate::ids::TurnId;
use crate::message::UserMessage;
use crate::state::AgentState;

/// A queued user input plus its optional sender/kind tags.
///
/// `from = None` (paired with `kind = None`) means the input came from the
/// human user (or unknown origin — same thing at the core layer). `from =
/// Some(session_id)` (paired with `kind = Some(kind_tag)`) means it was
/// injected by another session (e.g. a parent directive or a child report).
/// `from` and `kind` are always both `None` or both `Some`, matching the
/// `AgentInput::Steer`/`FollowUp` invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserInputEntry {
    pub(crate) from: Option<String>,
    pub(crate) kind: Option<String>,
    pub(crate) content: UserMessage,
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
    interrupt_requested: bool,
}

impl Mailbox {
    pub(crate) fn push_input(&mut self, input: AgentInput) -> Result<(), crate::AgentInputError> {
        input.validate()?;
        match input {
            AgentInput::Interrupt => self.request_interrupt(),
            AgentInput::Steer {
                from,
                kind,
                content,
            } => {
                self.steer.push_back(UserInputEntry {
                    from,
                    kind,
                    content,
                });
            }
            AgentInput::FollowUp {
                from,
                kind,
                content,
            } => {
                self.follow_up.push_back(UserInputEntry {
                    from,
                    kind,
                    content,
                });
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
        Ok(())
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
            from: None,
            kind: None,
            content: UserMessage::text(input),
        });
    }

    #[cfg(test)]
    fn push_follow_up(&mut self, input: impl Into<String>) {
        self.follow_up.push_back(UserInputEntry {
            from: None,
            kind: None,
            content: UserMessage::text(input),
        });
    }

    /// Pop the next queued user input entry (steer before follow-up),
    /// preserving the `from`/`kind` tags it was enqueued with.
    fn pop_user_input_entry(&mut self) -> Option<UserInputEntry> {
        self.steer
            .pop_front()
            .or_else(|| self.follow_up.pop_front())
    }

    /// Drain every queued user input as reconstructed `AgentInput` values,
    /// preserving priority order (steer before follow-up) and the
    /// `from`/`kind` tags each entry was enqueued with.
    ///
    /// Notifications and interrupt state are left untouched.
    pub(crate) fn drain_pending_inputs(&mut self) -> Vec<AgentInput> {
        let mut drained = Vec::with_capacity(self.steer.len() + self.follow_up.len());
        drained.extend(self.steer.drain(..).map(|entry| AgentInput::Steer {
            from: entry.from,
            kind: entry.kind,
            content: entry.content,
        }));
        drained.extend(self.follow_up.drain(..).map(|entry| AgentInput::FollowUp {
            from: entry.from,
            kind: entry.kind,
            content: entry.content,
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
            AgentState::ReadyToContinue { .. } => Some(AgentEvent::ContinueModel),
            AgentState::Idle => self.pop_user_input_entry().map(|entry| {
                let origin = entry
                    .from
                    .zip(entry.kind)
                    .map(|(from, kind)| TurnOrigin { from, kind });
                AgentEvent::StartTurn {
                    turn_id: next_turn_id,
                    input: entry.content,
                    origin,
                }
            }),
            AgentState::RunningModel { .. } | AgentState::RunningTools { .. } => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn steer_len(&self) -> usize {
        self.steer.len()
    }

    pub(crate) fn total_len(&self) -> usize {
        self.notifications.len() + self.steer.len() + self.follow_up.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentEvent;
    use crate::ids::{ActionId, ToolCallId};
    use crate::message::{AssistantMessage, ToolResultMessage, ToolResultStatus};

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
            Some(UserMessage::text("steer"))
        );
        assert_eq!(
            mailbox.pop_user_input_entry().map(|e| e.content),
            Some(UserMessage::text("follow-up"))
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
                    tool_calls: Vec::new(),
                    tool_action_ids: Vec::new(),
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
                    tool_action_ids: Vec::new(),
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
                input: UserMessage::text("steer"),
                origin: None,
            })
        );
        assert_eq!(
            mailbox.next_event(&AgentState::Idle, TurnId(3)),
            Some(AgentEvent::StartTurn {
                turn_id: TurnId(3),
                input: UserMessage::text("follow-up"),
                origin: None,
            })
        );
    }

    #[test]
    fn tagged_steer_surfaces_turn_origin_on_next_event() {
        let mut mailbox = Mailbox::default();
        mailbox
            .push_input(AgentInput::steer_tagged(
                "parent",
                "agent_directive",
                "do X",
            ))
            .expect("test input should be valid");

        let event = mailbox
            .next_event(&AgentState::Idle, TurnId(1))
            .expect("tagged steer should surface as StartTurn");

        assert_eq!(
            event,
            AgentEvent::StartTurn {
                turn_id: TurnId(1),
                input: UserMessage::text("do X"),
                origin: Some(TurnOrigin {
                    from: "parent".to_string(),
                    kind: "agent_directive".to_string(),
                }),
            }
        );
    }

    #[test]
    fn untagged_steer_surfaces_without_origin() {
        let mut mailbox = Mailbox::default();
        mailbox
            .push_input(AgentInput::steer("plain"))
            .expect("test input should be valid");

        let event = mailbox
            .next_event(&AgentState::Idle, TurnId(1))
            .expect("untagged steer should surface as StartTurn");

        assert_eq!(
            event,
            AgentEvent::StartTurn {
                turn_id: TurnId(1),
                input: UserMessage::text("plain"),
                origin: None,
            }
        );
    }
}
