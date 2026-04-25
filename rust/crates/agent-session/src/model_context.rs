use agent_core::{ContextItem, ToolCall, ToolCallId, ToolResultMessage, TurnId, TurnOutcome};

use crate::transcript_store::KIND_COMPACTION_SUMMARY;

/// Materialized model context for one transcript path.
///
/// The transcript store is canonical; `ModelContext` is a derived view over a
/// context-item sequence. The session rebuilds one whenever it needs to feed
/// the core loop or model provider a contiguous ordered history, and uses the
/// same type to rehydrate crashed sessions through explicit crash-tail
/// recovery.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModelContext {
    records: Vec<ContextItem>,
}

impl ModelContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_records(records: Vec<ContextItem>) -> Self {
        Self { records }
    }

    pub fn from_records_recovering_crashed_tail(mut records: Vec<ContextItem>) -> Self {
        Self::patch_crashed_tail(&mut records);
        Self { records }
    }

    pub fn records(&self) -> &[ContextItem] {
        &self.records
    }

    pub fn into_records(self) -> Vec<ContextItem> {
        self.records
    }

    pub fn is_turn_boundary(&self) -> bool {
        for record in self.records.iter().rev() {
            match record {
                ContextItem::TurnFinished { .. } => return true,
                ContextItem::Injected(_) => continue,
                _ => return false,
            }
        }
        true
    }

    /// Latest compaction summary on the transcript, if any. Returns the
    /// `content` string of the closest `ContextItem::Injected` with the
    /// well-known `compaction_summary` kind.
    pub fn latest_compaction_summary(&self) -> Option<&str> {
        self.records.iter().rev().find_map(|r| match r {
            ContextItem::Injected(cm) if cm.kind == KIND_COMPACTION_SUMMARY => {
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

    pub fn append(&mut self, record: ContextItem) {
        self.records.push(record);
    }

    pub fn last_turn_id(&self) -> TurnId {
        self.records
            .iter()
            .rev()
            .find_map(ContextItem::turn_id)
            .unwrap_or_default()
    }

    pub fn tail_outcome(&self) -> Option<TurnOutcome> {
        match self.records.last() {
            Some(ContextItem::TurnFinished { outcome, .. }) => Some(*outcome),
            _ => None,
        }
    }

    fn patch_crashed_tail(records: &mut Vec<ContextItem>) {
        let Some((turn_id, tail_start)) = Self::open_tail_turn(records) else {
            return;
        };

        Self::patch_missing_tool_results(records, tail_start);
        records.push(ContextItem::TurnFinished {
            turn_id,
            outcome: TurnOutcome::Crashed,
        });
    }

    fn open_tail_turn(records: &[ContextItem]) -> Option<(TurnId, usize)> {
        records
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, record)| match record {
                ContextItem::TurnStarted { turn_id } => Some(Some((*turn_id, index))),
                ContextItem::TurnFinished { .. } => Some(None),
                // Injected records do not determine whether the tail turn is
                // still open; keep looking backward for the nearest turn marker.
                ContextItem::Injected(_)
                | ContextItem::UserMessage(_)
                | ContextItem::AssistantMessage(_)
                | ContextItem::ToolCallStarted { .. }
                | ContextItem::ToolResult(_) => None,
            })
            .flatten()
    }

    fn patch_missing_tool_results(records: &mut Vec<ContextItem>, tail_start: usize) {
        let mut tool_calls = Vec::new();
        let mut completed_tool_calls = Vec::new();

        for record in &records[tail_start..] {
            match record {
                ContextItem::AssistantMessage(message) => {
                    tool_calls.extend(message.tool_calls().cloned());
                }
                ContextItem::ToolResult(result) => {
                    completed_tool_calls.push((result.tool_call_id, result.tool_name.clone()));
                }
                ContextItem::TurnStarted { .. }
                | ContextItem::UserMessage(_)
                | ContextItem::ToolCallStarted { .. }
                | ContextItem::TurnFinished { .. }
                | ContextItem::Injected(_) => {}
            }
        }

        for tool_call in Self::missing_tool_calls(tool_calls, completed_tool_calls) {
            records.push(ContextItem::ToolResult(ToolResultMessage::crashed(
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

impl From<Vec<ContextItem>> for ModelContext {
    fn from(records: Vec<ContextItem>) -> Self {
        Self::from_records(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript_store::compaction_summary;
    use agent_core::{AssistantItem, AssistantMessage, InjectedMessage, ToolResultStatus};

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
        assert!(ModelContext::new().is_turn_boundary());
    }

    #[test]
    fn turn_boundary_walks_past_injected_records() {
        let transcript = ModelContext::from_records(vec![
            ContextItem::TurnStarted { turn_id: TurnId(1) },
            ContextItem::UserMessage("hi".to_string()),
            ContextItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            ContextItem::Injected(compaction_summary("summary", "some_id", 100)),
            ContextItem::Injected(InjectedMessage::new("note", "branch note")),
        ]);
        assert!(transcript.is_turn_boundary());
        assert_eq!(transcript.latest_compaction_summary(), Some("summary"));
    }

    #[test]
    fn boundary_prefix_requires_a_finished_turn() {
        let transcript = ModelContext::from_records(vec![
            ContextItem::TurnStarted { turn_id: TurnId(1) },
            ContextItem::UserMessage("hello".to_string()),
            ContextItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            ContextItem::TurnStarted { turn_id: TurnId(2) },
            ContextItem::UserMessage("next".to_string()),
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

        let transcript = ModelContext::from_records_recovering_crashed_tail(vec![
            ContextItem::TurnStarted { turn_id: TurnId(7) },
            ContextItem::UserMessage("hello".to_string()),
            ContextItem::AssistantMessage(AssistantMessage {
                items: vec![
                    AssistantItem::ToolCall(first.clone()),
                    AssistantItem::ToolCall(second.clone()),
                ],
            }),
            ContextItem::ToolCallStarted {
                turn_id: TurnId(7),
                tool_call: first.clone(),
            },
            ContextItem::ToolCallStarted {
                turn_id: TurnId(7),
                tool_call: second.clone(),
            },
            ContextItem::ToolResult(tool_result(1, "bash")),
        ]);

        assert_eq!(
            transcript.records().last(),
            Some(&ContextItem::TurnFinished {
                turn_id: TurnId(7),
                outcome: TurnOutcome::Crashed,
            })
        );
        assert_eq!(
            transcript.records()[6],
            ContextItem::ToolResult(ToolResultMessage::crashed(second.id, "read"))
        );
    }

    #[test]
    fn crashed_tail_patches_assistant_tool_calls_even_without_start_records() {
        let tool_call = tool_call(1, "bash");

        let transcript = ModelContext::from_records_recovering_crashed_tail(vec![
            ContextItem::TurnStarted { turn_id: TurnId(8) },
            ContextItem::UserMessage("hello".to_string()),
            ContextItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            }),
        ]);

        assert_eq!(
            transcript.records()[3],
            ContextItem::ToolResult(ToolResultMessage::crashed(tool_call.id, "bash"))
        );
        assert_eq!(
            transcript.records()[4],
            ContextItem::TurnFinished {
                turn_id: TurnId(8),
                outcome: TurnOutcome::Crashed,
            }
        );
    }
}
