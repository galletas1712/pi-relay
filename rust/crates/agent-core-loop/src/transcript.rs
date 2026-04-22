use crate::event::{AgentEvent, TurnOutcome};
use crate::ids::TurnId;
use crate::message::CompactMessage;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Transcript {
    // Canonical append-only session log.
    // TODO: Add richer compaction, rewind/fork, and resume APIs on top of this
    // log. Boundary prefixes and crash-tail patching are the first primitives.
    events: Vec<AgentEvent>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_events(mut events: Vec<AgentEvent>) -> Self {
        Self::patch_crashed_tail(&mut events);
        Self { events }
    }

    pub fn events(&self) -> &[AgentEvent] {
        &self.events
    }

    pub fn into_events(self) -> Vec<AgentEvent> {
        self.events
    }

    pub fn is_turn_boundary(&self) -> bool {
        matches!(
            self.events.last(),
            None | Some(AgentEvent::TurnFinished { .. })
        )
    }

    pub fn boundary_prefix(&self, len: usize) -> Option<Self> {
        if len > self.events.len() {
            return None;
        }

        let prefix = Self {
            events: self.events[..len].to_vec(),
        };
        prefix.is_turn_boundary().then_some(prefix)
    }

    pub fn append(&mut self, event: AgentEvent) {
        self.events.push(event);
    }

    pub fn last_turn_id(&self) -> TurnId {
        self.events
            .iter()
            .rev()
            .find_map(AgentEvent::turn_id)
            .unwrap_or_default()
    }

    pub fn tail_outcome(&self) -> Option<TurnOutcome> {
        match self.events.last() {
            Some(AgentEvent::TurnFinished { outcome, .. }) => Some(*outcome),
            _ => None,
        }
    }

    pub fn compact(&self) -> Vec<CompactMessage> {
        self.events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::UserMessage(message) => Some(CompactMessage::User(message.clone())),
                AgentEvent::AssistantMessage(message) => {
                    Some(CompactMessage::Assistant(message.clone()))
                }
                AgentEvent::TurnStarted { .. }
                | AgentEvent::ToolCallStarted { .. }
                | AgentEvent::ToolResult(_)
                | AgentEvent::TurnFinished { .. } => None,
            })
            .collect()
    }

    fn patch_crashed_tail(events: &mut Vec<AgentEvent>) {
        let Some(turn_id) = Self::open_tail_turn_id(events) else {
            return;
        };

        events.push(AgentEvent::TurnFinished {
            turn_id,
            outcome: TurnOutcome::Crashed,
        });
    }

    fn open_tail_turn_id(events: &[AgentEvent]) -> Option<TurnId> {
        events
            .iter()
            .rev()
            .find_map(|event| match event {
                AgentEvent::TurnStarted { turn_id } => Some(Some(*turn_id)),
                AgentEvent::TurnFinished { .. } => Some(None),
                AgentEvent::UserMessage(_)
                | AgentEvent::AssistantMessage(_)
                | AgentEvent::ToolCallStarted { .. }
                | AgentEvent::ToolResult(_) => None,
            })
            .flatten()
    }
}

impl From<Vec<AgentEvent>> for Transcript {
    fn from(events: Vec<AgentEvent>) -> Self {
        Self::from_events(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::UserMessage;

    #[test]
    fn empty_transcript_is_a_turn_boundary() {
        assert!(Transcript::new().is_turn_boundary());
    }

    #[test]
    fn boundary_prefix_requires_a_finished_turn() {
        let transcript = Transcript::from_events(vec![
            AgentEvent::TurnStarted { turn_id: TurnId(1) },
            AgentEvent::UserMessage(UserMessage {
                text: "hello".to_string(),
            }),
            AgentEvent::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            AgentEvent::TurnStarted { turn_id: TurnId(2) },
            AgentEvent::UserMessage(UserMessage {
                text: "next".to_string(),
            }),
        ]);

        let prefix = transcript
            .boundary_prefix(3)
            .expect("finished turn should be a valid boundary");
        assert_eq!(prefix.events().len(), 3);
        assert!(transcript.boundary_prefix(4).is_none());
        assert!(transcript.boundary_prefix(99).is_none());
    }
}
