use crate::ids::{ToolCallId, TurnId};
use crate::message::{AssistantMessage, ToolCall, ToolResultMessage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Graceful,
    Interrupted,
    Crashed,
}

/// Durable append-only session record.
///
/// These records are persisted, replayed, compacted, forked, and rewound. They
/// are not hook/lifecycle events; hooks should attach to a separate lifecycle
/// notification stream derived while the loop is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptRecord {
    TurnStarted {
        turn_id: TurnId,
    },
    UserMessage(String),
    AssistantMessage(AssistantMessage),
    ToolCallStarted {
        turn_id: TurnId,
        tool_call: ToolCall,
    },
    ToolResult(ToolResultMessage),
    TurnFinished {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
}

impl TranscriptRecord {
    pub fn turn_id(&self) -> Option<TurnId> {
        match self {
            TranscriptRecord::TurnStarted { turn_id }
            | TranscriptRecord::ToolCallStarted { turn_id, .. }
            | TranscriptRecord::TurnFinished { turn_id, .. } => Some(*turn_id),
            TranscriptRecord::UserMessage(_)
            | TranscriptRecord::AssistantMessage(_)
            | TranscriptRecord::ToolResult(_) => None,
        }
    }
}

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

    fn patch_crashed_tail(records: &mut Vec<TranscriptRecord>) {
        let Some((turn_id, tail_start)) = Self::open_tail_turn(records) else {
            return;
        };

        Self::patch_missing_tool_results(records, tail_start);
        records.push(TranscriptRecord::TurnFinished {
            turn_id,
            outcome: TurnOutcome::Crashed,
        });
    }

    fn open_tail_turn(records: &[TranscriptRecord]) -> Option<(TurnId, usize)> {
        records
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, record)| match record {
                TranscriptRecord::TurnStarted { turn_id } => Some(Some((*turn_id, index))),
                TranscriptRecord::TurnFinished { .. } => Some(None),
                TranscriptRecord::UserMessage(_)
                | TranscriptRecord::AssistantMessage(_)
                | TranscriptRecord::ToolCallStarted { .. }
                | TranscriptRecord::ToolResult(_) => None,
            })
            .flatten()
    }

    fn patch_missing_tool_results(records: &mut Vec<TranscriptRecord>, tail_start: usize) {
        let mut tool_calls = Vec::new();
        let mut completed_tool_calls = Vec::new();

        for record in &records[tail_start..] {
            match record {
                TranscriptRecord::AssistantMessage(message) => {
                    tool_calls.extend(message.tool_calls().cloned());
                }
                TranscriptRecord::ToolResult(result) => {
                    completed_tool_calls.push((result.tool_call_id, result.tool_name.clone()));
                }
                TranscriptRecord::TurnStarted { .. }
                | TranscriptRecord::UserMessage(_)
                | TranscriptRecord::ToolCallStarted { .. }
                | TranscriptRecord::TurnFinished { .. } => {}
            }
        }

        for tool_call in Self::missing_tool_calls(tool_calls, completed_tool_calls) {
            records.push(TranscriptRecord::ToolResult(ToolResultMessage::crashed(
                tool_call.id,
                tool_call.tool_name,
            )));
        }
    }

    fn missing_tool_calls(
        tool_calls: Vec<ToolCall>,
        mut completed_tool_calls: Vec<(ToolCallId, String)>,
    ) -> Vec<ToolCall> {
        let mut missing = Vec::new();

        for tool_call in tool_calls {
            let Some(completed_index) = completed_tool_calls
                .iter()
                .position(|(id, name)| *id == tool_call.id && name == &tool_call.tool_name)
            else {
                missing.push(tool_call);
                continue;
            };

            completed_tool_calls.remove(completed_index);
        }

        missing
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
    use crate::message::{
        AssistantItem, AssistantMessage, ToolCall, ToolResultMessage, ToolResultStatus,
    };

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
    fn empty_transcript_is_a_turn_boundary() {
        assert!(Transcript::new().is_turn_boundary());
    }

    #[test]
    fn boundary_prefix_requires_a_finished_turn() {
        let transcript = Transcript::from_records(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage("hello".to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
            TranscriptRecord::UserMessage("next".to_string()),
        ]);

        let prefix = transcript
            .boundary_prefix(3)
            .expect("finished turn should be a valid boundary");
        assert_eq!(prefix.records().len(), 3);
        assert!(transcript.boundary_prefix(4).is_none());
        assert!(transcript.boundary_prefix(99).is_none());
    }

    #[test]
    fn crashed_tail_patches_missing_tool_results_before_finishing_turn() {
        let first = tool_call(1, "bash");
        let second = tool_call(2, "read");

        let transcript = Transcript::from_records(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(7) },
            TranscriptRecord::UserMessage("hello".to_string()),
            TranscriptRecord::AssistantMessage(AssistantMessage {
                items: vec![
                    AssistantItem::ToolCall(first.clone()),
                    AssistantItem::ToolCall(second.clone()),
                ],
            }),
            TranscriptRecord::ToolCallStarted {
                turn_id: TurnId(7),
                tool_call: first.clone(),
            },
            TranscriptRecord::ToolCallStarted {
                turn_id: TurnId(7),
                tool_call: second.clone(),
            },
            TranscriptRecord::ToolResult(tool_result(1, "bash")),
        ]);

        assert_eq!(
            transcript.records().last(),
            Some(&TranscriptRecord::TurnFinished {
                turn_id: TurnId(7),
                outcome: TurnOutcome::Crashed,
            })
        );
        assert_eq!(
            transcript.records()[6],
            TranscriptRecord::ToolResult(ToolResultMessage::crashed(second.id, "read"))
        );
    }

    #[test]
    fn crashed_tail_patches_assistant_tool_calls_even_without_start_records() {
        let tool_call = tool_call(1, "bash");

        let transcript = Transcript::from_records(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(8) },
            TranscriptRecord::UserMessage("hello".to_string()),
            TranscriptRecord::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            }),
        ]);

        assert_eq!(
            transcript.records()[3],
            TranscriptRecord::ToolResult(ToolResultMessage::crashed(tool_call.id, "bash"))
        );
        assert_eq!(
            transcript.records()[4],
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(8),
                outcome: TurnOutcome::Crashed,
            }
        );
    }
}
