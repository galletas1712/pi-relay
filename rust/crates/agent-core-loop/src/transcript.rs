use crate::ids::TurnId;
use crate::message::CompactMessage;
use crate::transcript_record::{TranscriptRecord, TurnOutcome};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Transcript {
    // Canonical append-only session log.
    // TODO: Add richer compaction, rewind/fork, and resume APIs on top of this
    // log. Boundary prefixes and crash-tail patching are the first primitives.
    records: Vec<TranscriptRecord>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_records(mut records: Vec<TranscriptRecord>) -> Self {
        Self::patch_crashed_tail(&mut records);
        Self { records }
    }

    pub fn records(&self) -> &[TranscriptRecord] {
        &self.records
    }

    pub fn into_records(self) -> Vec<TranscriptRecord> {
        self.records
    }

    pub fn is_turn_boundary(&self) -> bool {
        matches!(
            self.records.last(),
            None | Some(TranscriptRecord::TurnFinished { .. })
        )
    }

    pub fn boundary_prefix(&self, len: usize) -> Option<Self> {
        if len > self.records.len() {
            return None;
        }

        let prefix = Self {
            records: self.records[..len].to_vec(),
        };
        prefix.is_turn_boundary().then_some(prefix)
    }

    pub fn append(&mut self, record: TranscriptRecord) {
        self.records.push(record);
    }

    pub fn last_turn_id(&self) -> TurnId {
        self.records
            .iter()
            .rev()
            .find_map(TranscriptRecord::turn_id)
            .unwrap_or_default()
    }

    pub fn tail_outcome(&self) -> Option<TurnOutcome> {
        match self.records.last() {
            Some(TranscriptRecord::TurnFinished { outcome, .. }) => Some(*outcome),
            _ => None,
        }
    }

    pub fn compact(&self) -> Vec<CompactMessage> {
        self.records
            .iter()
            .filter_map(|record| match record {
                TranscriptRecord::UserMessage(message) => {
                    Some(CompactMessage::User(message.clone()))
                }
                TranscriptRecord::AssistantMessage(message) => {
                    Some(CompactMessage::Assistant(message.clone()))
                }
                TranscriptRecord::TurnStarted { .. }
                | TranscriptRecord::ToolCallStarted { .. }
                | TranscriptRecord::ToolResult(_)
                | TranscriptRecord::TurnFinished { .. } => None,
            })
            .collect()
    }

    fn patch_crashed_tail(records: &mut Vec<TranscriptRecord>) {
        let Some(turn_id) = Self::open_tail_turn_id(records) else {
            return;
        };

        records.push(TranscriptRecord::TurnFinished {
            turn_id,
            outcome: TurnOutcome::Crashed,
        });
    }

    fn open_tail_turn_id(records: &[TranscriptRecord]) -> Option<TurnId> {
        records
            .iter()
            .rev()
            .find_map(|record| match record {
                TranscriptRecord::TurnStarted { turn_id } => Some(Some(*turn_id)),
                TranscriptRecord::TurnFinished { .. } => Some(None),
                TranscriptRecord::UserMessage(_)
                | TranscriptRecord::AssistantMessage(_)
                | TranscriptRecord::ToolCallStarted { .. }
                | TranscriptRecord::ToolResult(_) => None,
            })
            .flatten()
    }
}

impl From<Vec<TranscriptRecord>> for Transcript {
    fn from(records: Vec<TranscriptRecord>) -> Self {
        Self::from_records(records)
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
        let transcript = Transcript::from_records(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage(UserMessage {
                text: "hello".to_string(),
            }),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
            TranscriptRecord::UserMessage(UserMessage {
                text: "next".to_string(),
            }),
        ]);

        let prefix = transcript
            .boundary_prefix(3)
            .expect("finished turn should be a valid boundary");
        assert_eq!(prefix.records().len(), 3);
        assert!(transcript.boundary_prefix(4).is_none());
        assert!(transcript.boundary_prefix(99).is_none());
    }
}
