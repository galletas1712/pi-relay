use agent_core::{ToolCall, ToolCallId, ToolResultMessage, TranscriptRecord, TurnId, TurnOutcome};

use crate::context::compaction::KIND_COMPACTION_SUMMARY;

/// Materialized session history.
///
/// The `Context` is the canonical store; `Transcript` is a derived view over
/// a record sequence. The session rebuilds one whenever it needs to feed the
/// core loop or model context a contiguous ordered history, and uses the same
/// type to rehydrate crashed sessions through `from_records`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Transcript {
    records: Vec<TranscriptRecord>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_records(records: Vec<TranscriptRecord>) -> Self {
        Self { records }
    }

    pub fn from_records_recovering_crashed_tail(mut records: Vec<TranscriptRecord>) -> Self {
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
        for record in self.records.iter().rev() {
            match record {
                TranscriptRecord::TurnFinished { .. } => return true,
                TranscriptRecord::Custom(_) => continue,
                _ => return false,
            }
        }
        true
    }

    /// Latest compaction summary on the transcript, if any. Returns the
    /// `content` string of the closest `TranscriptRecord::Custom` with the
    /// well-known `compaction_summary` kind.
    pub fn latest_compaction_summary(&self) -> Option<&str> {
        self.records.iter().rev().find_map(|r| match r {
            TranscriptRecord::Custom(cm) if cm.kind == KIND_COMPACTION_SUMMARY => {
                Some(cm.content.as_str())
            }
            _ => None,
        })
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
                // Custom records live strictly between turns; they don't
                // participate in crash-tail patching. Keep looking backward.
                TranscriptRecord::Custom(_)
                | TranscriptRecord::UserMessage(_)
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
                | TranscriptRecord::TurnFinished { .. }
                | TranscriptRecord::Custom(_) => {}
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
    use crate::context::compaction::compaction_summary;
    use agent_core::{AssistantItem, AssistantMessage, CustomMessage, ToolResultStatus};

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
    fn turn_boundary_walks_past_custom_records() {
        let transcript = Transcript::from_records(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage("hi".to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptRecord::Custom(compaction_summary("summary", "some_id", 100)),
            TranscriptRecord::Custom(CustomMessage::new("note", "branch note")),
        ]);
        assert!(transcript.is_turn_boundary());
        assert_eq!(transcript.latest_compaction_summary(), Some("summary"));
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

        let transcript = Transcript::from_records_recovering_crashed_tail(vec![
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

        let transcript = Transcript::from_records_recovering_crashed_tail(vec![
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
