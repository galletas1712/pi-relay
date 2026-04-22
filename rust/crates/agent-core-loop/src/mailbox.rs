use std::collections::VecDeque;

use crate::ids::TurnId;
use crate::message::{AssistantMessage, ToolCall, ToolResultMessage, UserInput};

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
pub enum MailboxEntry {
    Event(MailboxEvent),
    UserInput(UserInput),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Mailbox {
    events: VecDeque<MailboxEvent>,
    steer: VecDeque<UserInput>,
    follow_up: VecDeque<UserInput>,
}

impl Mailbox {
    pub fn push_event_front(&mut self, event: MailboxEvent) {
        self.events.push_front(event);
    }

    pub fn push_event_back(&mut self, event: MailboxEvent) {
        self.events.push_back(event);
    }

    pub fn front_event(&self) -> Option<&MailboxEvent> {
        self.events.front()
    }

    pub fn pop_event(&mut self) -> Option<MailboxEvent> {
        self.events.pop_front()
    }

    pub fn push_steer(&mut self, input: UserInput) {
        self.steer.push_back(input);
    }

    pub fn push_follow_up(&mut self, input: UserInput) {
        self.follow_up.push_back(input);
    }

    pub fn pop_user_input(&mut self) -> Option<UserInput> {
        self.steer
            .pop_front()
            .or_else(|| self.follow_up.pop_front())
    }

    pub fn front_next(&self) -> Option<MailboxEntry> {
        self.events
            .front()
            .cloned()
            .map(MailboxEntry::Event)
            .or_else(|| self.steer.front().cloned().map(MailboxEntry::UserInput))
            .or_else(|| self.follow_up.front().cloned().map(MailboxEntry::UserInput))
    }

    pub fn pop_next(&mut self) -> Option<MailboxEntry> {
        self.events
            .pop_front()
            .map(MailboxEntry::Event)
            .or_else(|| self.steer.pop_front().map(MailboxEntry::UserInput))
            .or_else(|| self.follow_up.pop_front().map(MailboxEntry::UserInput))
    }

    pub fn event_len(&self) -> usize {
        self.events.len()
    }

    pub fn steer_len(&self) -> usize {
        self.steer.len()
    }

    pub fn follow_up_len(&self) -> usize {
        self.follow_up.len()
    }

    pub fn total_len(&self) -> usize {
        self.events.len() + self.steer.len() + self.follow_up.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ToolCallId;
    use crate::message::ToolResultStatus;

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

    #[test]
    fn event_queue_behaves_like_a_deque() {
        let mut mailbox = Mailbox::default();
        let later = MailboxEvent::ToolCallReady {
            turn_id: TurnId(1),
            tool_call: tool_call(2, "read"),
        };
        let now = MailboxEvent::AssistantMessage {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        };

        mailbox.push_event_back(later.clone());
        mailbox.push_event_front(now.clone());

        assert_eq!(mailbox.front_event(), Some(&now));
        assert_eq!(mailbox.pop_event(), Some(now));
        assert_eq!(mailbox.pop_event(), Some(later));
        assert_eq!(mailbox.pop_event(), None);
    }

    #[test]
    fn user_input_prefers_steer_before_follow_up() {
        let mut mailbox = Mailbox::default();
        mailbox.push_follow_up(UserInput::from("follow-up"));
        mailbox.push_steer(UserInput::from("steer"));

        assert_eq!(mailbox.pop_user_input(), Some(UserInput::from("steer")));
        assert_eq!(mailbox.pop_user_input(), Some(UserInput::from("follow-up")));
        assert_eq!(mailbox.pop_user_input(), None);
    }

    #[test]
    fn priority_order_is_event_then_steer_then_follow_up() {
        let mut mailbox = Mailbox::default();
        mailbox.push_follow_up(UserInput::from("follow-up"));
        mailbox.push_steer(UserInput::from("steer"));
        mailbox.push_event_back(MailboxEvent::ToolResult {
            turn_id: TurnId(1),
            result: tool_result(9, "bash"),
        });

        assert!(matches!(
            mailbox.front_next(),
            Some(MailboxEntry::Event(MailboxEvent::ToolResult { .. }))
        ));
        assert!(matches!(
            mailbox.pop_next(),
            Some(MailboxEntry::Event(MailboxEvent::ToolResult { .. }))
        ));
        assert_eq!(
            mailbox.pop_next(),
            Some(MailboxEntry::UserInput(UserInput::from("steer")))
        );
        assert_eq!(
            mailbox.pop_next(),
            Some(MailboxEntry::UserInput(UserInput::from("follow-up")))
        );
    }
}
