use std::collections::VecDeque;

use crate::ids::TurnId;
use crate::message::{AssistantMessage, ToolCall, ToolResultMessage, UserInput};

/// Volatile notification sent into the loop by model/tool execution.
///
/// Notifications are queued in the mailbox and are lost on process death. They
/// are not durable transcript records and are not hook lifecycle events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxNotification {
    AssistantMessage {
        turn_id: TurnId,
        assistant: AssistantMessage,
    },
    ToolResult {
        turn_id: TurnId,
        result: ToolResultMessage,
    },
}

impl MailboxNotification {
    pub fn turn_id(&self) -> TurnId {
        match self {
            MailboxNotification::AssistantMessage { turn_id, .. }
            | MailboxNotification::ToolResult { turn_id, .. } => *turn_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxEntry {
    Notification(MailboxNotification),
    ToolCall(ToolCall),
    UserInput(UserInput),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Mailbox {
    notifications: VecDeque<MailboxNotification>,
    tool_calls: VecDeque<ToolCall>,
    steer: VecDeque<UserInput>,
    follow_up: VecDeque<UserInput>,
}

impl Mailbox {
    pub fn push_notification_front(&mut self, notification: MailboxNotification) {
        self.notifications.push_front(notification);
    }

    pub fn push_notification_back(&mut self, notification: MailboxNotification) {
        self.notifications.push_back(notification);
    }

    pub fn front_notification(&self) -> Option<&MailboxNotification> {
        self.notifications.front()
    }

    pub fn pop_notification(&mut self) -> Option<MailboxNotification> {
        self.notifications.pop_front()
    }

    pub fn push_tool_call(&mut self, tool_call: ToolCall) {
        self.tool_calls.push_back(tool_call);
    }

    pub fn pop_tool_call(&mut self) -> Option<ToolCall> {
        self.tool_calls.pop_front()
    }

    pub fn clear_tool_calls(&mut self) {
        self.tool_calls.clear();
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
        self.notifications
            .front()
            .cloned()
            .map(MailboxEntry::Notification)
            .or_else(|| self.tool_calls.front().cloned().map(MailboxEntry::ToolCall))
            .or_else(|| self.steer.front().cloned().map(MailboxEntry::UserInput))
            .or_else(|| self.follow_up.front().cloned().map(MailboxEntry::UserInput))
    }

    pub fn pop_next(&mut self) -> Option<MailboxEntry> {
        self.notifications
            .pop_front()
            .map(MailboxEntry::Notification)
            .or_else(|| self.tool_calls.pop_front().map(MailboxEntry::ToolCall))
            .or_else(|| self.steer.pop_front().map(MailboxEntry::UserInput))
            .or_else(|| self.follow_up.pop_front().map(MailboxEntry::UserInput))
    }

    pub fn notification_len(&self) -> usize {
        self.notifications.len()
    }

    pub fn tool_call_len(&self) -> usize {
        self.tool_calls.len()
    }

    pub fn steer_len(&self) -> usize {
        self.steer.len()
    }

    pub fn follow_up_len(&self) -> usize {
        self.follow_up.len()
    }

    pub fn total_len(&self) -> usize {
        self.notifications.len() + self.tool_calls.len() + self.steer.len() + self.follow_up.len()
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
    fn notification_queue_behaves_like_a_deque() {
        let mut mailbox = Mailbox::default();
        let later = MailboxNotification::ToolResult {
            turn_id: TurnId(1),
            result: tool_result(2, "read"),
        };
        let now = MailboxNotification::AssistantMessage {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        };

        mailbox.push_notification_back(later.clone());
        mailbox.push_notification_front(now.clone());

        assert_eq!(mailbox.front_notification(), Some(&now));
        assert_eq!(mailbox.pop_notification(), Some(now));
        assert_eq!(mailbox.pop_notification(), Some(later));
        assert_eq!(mailbox.pop_notification(), None);
    }

    #[test]
    fn tool_call_queue_behaves_like_a_deque() {
        let mut mailbox = Mailbox::default();
        let first = tool_call(1, "bash");
        let second = tool_call(2, "read");

        mailbox.push_tool_call(first.clone());
        mailbox.push_tool_call(second.clone());

        assert_eq!(mailbox.tool_call_len(), 2);
        assert_eq!(mailbox.pop_tool_call(), Some(first));
        assert_eq!(mailbox.pop_tool_call(), Some(second));
        assert_eq!(mailbox.pop_tool_call(), None);
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
    fn priority_order_is_notification_then_tool_call_then_steer_then_follow_up() {
        let mut mailbox = Mailbox::default();
        mailbox.push_follow_up(UserInput::from("follow-up"));
        mailbox.push_steer(UserInput::from("steer"));
        let tool_call = tool_call(7, "read");
        mailbox.push_tool_call(tool_call.clone());
        mailbox.push_notification_back(MailboxNotification::ToolResult {
            turn_id: TurnId(1),
            result: tool_result(9, "bash"),
        });

        assert!(matches!(
            mailbox.front_next(),
            Some(MailboxEntry::Notification(
                MailboxNotification::ToolResult { .. }
            ))
        ));
        assert!(matches!(
            mailbox.pop_next(),
            Some(MailboxEntry::Notification(
                MailboxNotification::ToolResult { .. }
            ))
        ));
        assert_eq!(mailbox.pop_next(), Some(MailboxEntry::ToolCall(tool_call)));
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
