use std::collections::VecDeque;

use crate::message::UserInput;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxCommand {
    Interrupt,
    // `Steer` is urgent user input: it drains before follow-up work.
    Steer(UserInput),
    // `FollowUp` is normal queued work and only runs once steer work is drained.
    FollowUp(UserInput),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Mailbox {
    steer: VecDeque<UserInput>,
    follow_up: VecDeque<UserInput>,
    interrupt: bool,
}

impl Mailbox {
    pub fn push(&mut self, command: MailboxCommand) {
        match command {
            MailboxCommand::Interrupt => {
                self.interrupt = true;
            }
            MailboxCommand::Steer(input) => {
                self.steer.push_back(input);
            }
            MailboxCommand::FollowUp(input) => {
                self.follow_up.push_back(input);
            }
        }
    }

    pub fn push_front_steer(&mut self, input: UserInput) {
        self.steer.push_front(input);
    }

    pub fn take_interrupt(&mut self) -> bool {
        let interrupted = self.interrupt;
        self.interrupt = false;
        interrupted
    }

    pub fn pop_next(&mut self) -> Option<UserInput> {
        self.steer
            .pop_front()
            .or_else(|| self.follow_up.pop_front())
    }

    pub fn steer_len(&self) -> usize {
        self.steer.len()
    }

    pub fn follow_up_len(&self) -> usize {
        self.follow_up.len()
    }

    pub fn pending_count(&self) -> usize {
        self.steer.len() + self.follow_up.len()
    }

    pub fn interrupt_pending(&self) -> bool {
        self.interrupt
    }

    pub fn is_empty(&self) -> bool {
        self.pending_count() == 0 && !self.interrupt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pop_next_prefers_steer_before_follow_up() {
        let mut mailbox = Mailbox::default();
        mailbox.push(MailboxCommand::FollowUp("later".into()));
        mailbox.push(MailboxCommand::Steer("now".into()));

        assert_eq!(mailbox.pop_next(), Some(UserInput::from("now")));
        assert_eq!(mailbox.pop_next(), Some(UserInput::from("later")));
        assert_eq!(mailbox.pop_next(), None);
    }

    #[test]
    fn interrupt_is_latched_separately_from_queued_work() {
        let mut mailbox = Mailbox::default();
        mailbox.push(MailboxCommand::FollowUp("later".into()));
        mailbox.push(MailboxCommand::Interrupt);

        assert!(mailbox.interrupt_pending());
        assert_eq!(mailbox.follow_up_len(), 1);
        assert!(mailbox.take_interrupt());
        assert!(!mailbox.interrupt_pending());
        assert_eq!(mailbox.pop_next(), Some(UserInput::from("later")));
    }

    #[test]
    fn push_front_steer_requeues_high_priority_work() {
        let mut mailbox = Mailbox::default();
        mailbox.push(MailboxCommand::Steer("second".into()));
        mailbox.push_front_steer("first".into());

        assert_eq!(mailbox.pop_next(), Some(UserInput::from("first")));
        assert_eq!(mailbox.pop_next(), Some(UserInput::from("second")));
    }
}
