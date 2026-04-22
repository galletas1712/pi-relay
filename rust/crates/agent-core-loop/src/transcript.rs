use crate::event::{AgentEvent, TurnOutcome};
use crate::ids::TurnId;
use crate::message::CompactMessage;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Transcript {
    // Canonical append-only session log.
    // TODO: Add first-class compaction, rewind/fork, and resume APIs on top of
    // this log instead of relying on direct event manipulation.
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
