use std::collections::VecDeque;

use crate::ids::TurnId;
use crate::message::{AssistantMessage, ToolCall, ToolResultMessage, UserInput};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxQueue {
    Event,
    Steer,
    FollowUp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxEvent {
    AssistantMessage {
        turn_id: TurnId,
        assistant: AssistantMessage,
    },
    ToolCallReady {
        turn_id: TurnId,
        tool_call: ToolCall,
    },
    ToolResult {
        turn_id: TurnId,
        result: ToolResultMessage,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxItem {
    Event(MailboxEvent),
    UserInput(UserInput),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxError {
    QueueItemMismatch {
        queue: MailboxQueue,
        item: MailboxItem,
    },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Mailbox {
    events: VecDeque<MailboxEvent>,
    steer: VecDeque<UserInput>,
    follow_up: VecDeque<UserInput>,
}

impl Mailbox {
    pub fn push_back(
        &mut self,
        queue: MailboxQueue,
        item: MailboxItem,
    ) -> Result<(), MailboxError> {
        match (queue, item) {
            (MailboxQueue::Event, MailboxItem::Event(event)) => {
                self.events.push_back(event);
                Ok(())
            }
            (MailboxQueue::Steer, MailboxItem::UserInput(input)) => {
                self.steer.push_back(input);
                Ok(())
            }
            (MailboxQueue::FollowUp, MailboxItem::UserInput(input)) => {
                self.follow_up.push_back(input);
                Ok(())
            }
            (queue, item) => Err(MailboxError::QueueItemMismatch { queue, item }),
        }
    }

    pub fn push_front(
        &mut self,
        queue: MailboxQueue,
        item: MailboxItem,
    ) -> Result<(), MailboxError> {
        match (queue, item) {
            (MailboxQueue::Event, MailboxItem::Event(event)) => {
                self.events.push_front(event);
                Ok(())
            }
            (MailboxQueue::Steer, MailboxItem::UserInput(input)) => {
                self.steer.push_front(input);
                Ok(())
            }
            (MailboxQueue::FollowUp, MailboxItem::UserInput(input)) => {
                self.follow_up.push_front(input);
                Ok(())
            }
            (queue, item) => Err(MailboxError::QueueItemMismatch { queue, item }),
        }
    }

    pub fn front(&self, queue: MailboxQueue) -> Option<MailboxItem> {
        match queue {
            MailboxQueue::Event => self.events.front().cloned().map(MailboxItem::Event),
            MailboxQueue::Steer => self.steer.front().cloned().map(MailboxItem::UserInput),
            MailboxQueue::FollowUp => self.follow_up.front().cloned().map(MailboxItem::UserInput),
        }
    }

    pub fn front_next(&self) -> Option<MailboxItem> {
        self.front(MailboxQueue::Event)
            .or_else(|| self.front(MailboxQueue::Steer))
            .or_else(|| self.front(MailboxQueue::FollowUp))
    }

    pub fn pop_front(&mut self, queue: MailboxQueue) -> Option<MailboxItem> {
        match queue {
            MailboxQueue::Event => self.events.pop_front().map(MailboxItem::Event),
            MailboxQueue::Steer => self.steer.pop_front().map(MailboxItem::UserInput),
            MailboxQueue::FollowUp => self.follow_up.pop_front().map(MailboxItem::UserInput),
        }
    }

    pub fn pop_next(&mut self) -> Option<MailboxItem> {
        self.pop_front(MailboxQueue::Event)
            .or_else(|| self.pop_front(MailboxQueue::Steer))
            .or_else(|| self.pop_front(MailboxQueue::FollowUp))
    }

    pub fn len(&self, queue: MailboxQueue) -> usize {
        match queue {
            MailboxQueue::Event => self.events.len(),
            MailboxQueue::Steer => self.steer.len(),
            MailboxQueue::FollowUp => self.follow_up.len(),
        }
    }

    pub fn is_empty(&self, queue: MailboxQueue) -> bool {
        self.len(queue) == 0
    }

    pub fn total_len(&self) -> usize {
        self.events.len() + self.steer.len() + self.follow_up.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::EventId;
    use crate::message::ToolResultStatus;

    #[test]
    fn each_queue_behaves_like_a_deque() {
        let mut mailbox = Mailbox::default();
        mailbox
            .push_back(
                MailboxQueue::Event,
                MailboxItem::Event(MailboxEvent::ToolResult {
                    turn_id: TurnId(1),
                    result: ToolResultMessage {
                        id: EventId(1),
                        tool_call_id: EventId(2),
                        tool_name: "bash".to_string(),
                        output: "ok".to_string(),
                        status: ToolResultStatus::Success,
                    },
                }),
            )
            .unwrap();
        mailbox
            .push_back(
                MailboxQueue::Steer,
                MailboxItem::UserInput(UserInput::from("steer")),
            )
            .unwrap();
        mailbox
            .push_back(
                MailboxQueue::FollowUp,
                MailboxItem::UserInput(UserInput::from("follow-up")),
            )
            .unwrap();

        assert_eq!(
            mailbox.front(MailboxQueue::Event),
            Some(MailboxItem::Event(MailboxEvent::ToolResult {
                turn_id: TurnId(1),
                result: ToolResultMessage {
                    id: EventId(1),
                    tool_call_id: EventId(2),
                    tool_name: "bash".to_string(),
                    output: "ok".to_string(),
                    status: ToolResultStatus::Success,
                },
            }))
        );
        assert_eq!(
            mailbox.pop_front(MailboxQueue::Event),
            Some(MailboxItem::Event(MailboxEvent::ToolResult {
                turn_id: TurnId(1),
                result: ToolResultMessage {
                    id: EventId(1),
                    tool_call_id: EventId(2),
                    tool_name: "bash".to_string(),
                    output: "ok".to_string(),
                    status: ToolResultStatus::Success,
                },
            }))
        );
        assert_eq!(
            mailbox.pop_front(MailboxQueue::Steer),
            Some(MailboxItem::UserInput(UserInput::from("steer")))
        );
        assert_eq!(
            mailbox.pop_front(MailboxQueue::FollowUp),
            Some(MailboxItem::UserInput(UserInput::from("follow-up")))
        );
    }

    #[test]
    fn priority_order_is_event_then_steer_then_follow_up() {
        let mut mailbox = Mailbox::default();
        mailbox
            .push_back(
                MailboxQueue::FollowUp,
                MailboxItem::UserInput(UserInput::from("follow-up")),
            )
            .unwrap();
        mailbox
            .push_back(
                MailboxQueue::Steer,
                MailboxItem::UserInput(UserInput::from("steer")),
            )
            .unwrap();
        mailbox
            .push_back(
                MailboxQueue::Event,
                MailboxItem::Event(MailboxEvent::ToolCallReady {
                    turn_id: TurnId(1),
                    tool_call: ToolCall {
                        id: EventId(9),
                        tool_name: "bash".to_string(),
                        args_json: "{}".to_string(),
                    },
                }),
            )
            .unwrap();

        assert!(matches!(
            mailbox.front_next(),
            Some(MailboxItem::Event(MailboxEvent::ToolCallReady { .. }))
        ));
        assert!(matches!(
            mailbox.pop_next(),
            Some(MailboxItem::Event(MailboxEvent::ToolCallReady { .. }))
        ));
        assert_eq!(
            mailbox.pop_next(),
            Some(MailboxItem::UserInput(UserInput::from("steer")))
        );
        assert_eq!(
            mailbox.pop_next(),
            Some(MailboxItem::UserInput(UserInput::from("follow-up")))
        );
    }

    #[test]
    fn push_front_preempts_existing_items_within_one_queue() {
        let mut mailbox = Mailbox::default();
        mailbox
            .push_back(
                MailboxQueue::Event,
                MailboxItem::Event(MailboxEvent::ToolCallReady {
                    turn_id: TurnId(1),
                    tool_call: ToolCall {
                        id: EventId(2),
                        tool_name: "read".to_string(),
                        args_json: "{}".to_string(),
                    },
                }),
            )
            .unwrap();
        mailbox
            .push_front(
                MailboxQueue::Event,
                MailboxItem::Event(MailboxEvent::AssistantMessage {
                    turn_id: TurnId(1),
                    assistant: AssistantMessage {
                        id: EventId(1),
                        items: Vec::new(),
                    },
                }),
            )
            .unwrap();

        assert_eq!(
            mailbox.front(MailboxQueue::Event),
            Some(MailboxItem::Event(MailboxEvent::AssistantMessage {
                turn_id: TurnId(1),
                assistant: AssistantMessage {
                    id: EventId(1),
                    items: Vec::new(),
                },
            }))
        );
    }

    #[test]
    fn queue_and_item_type_must_match() {
        let mut mailbox = Mailbox::default();
        assert_eq!(
            mailbox.push_back(
                MailboxQueue::Event,
                MailboxItem::UserInput(UserInput::from("wrong")),
            ),
            Err(MailboxError::QueueItemMismatch {
                queue: MailboxQueue::Event,
                item: MailboxItem::UserInput(UserInput::from("wrong")),
            })
        );
    }
}
