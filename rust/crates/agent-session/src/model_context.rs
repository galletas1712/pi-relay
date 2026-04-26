use agent_core::{ToolCall, ToolCallId, ToolResultMessage, TranscriptItem, TurnId, TurnOutcome};

/// Materialized model context for one transcript path.
///
/// The transcript store is canonical; `ModelContext` is a derived view over a
/// transcript item sequence. The session rebuilds one whenever it needs to feed
/// the core loop or model provider a contiguous ordered history, and uses the
/// same type to rehydrate crashed sessions through explicit crash-tail
/// recovery.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModelContext {
    items: Vec<TranscriptItem>,
}

impl ModelContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_transcript_items(items: Vec<TranscriptItem>) -> Self {
        Self { items }
    }

    pub fn from_transcript_items_recovering_crashed_tail(mut items: Vec<TranscriptItem>) -> Self {
        Self::patch_crashed_tail(&mut items);
        Self { items }
    }

    pub fn transcript_items(&self) -> &[TranscriptItem] {
        &self.items
    }

    pub fn into_transcript_items(self) -> Vec<TranscriptItem> {
        self.items
    }

    pub fn is_turn_boundary(&self) -> bool {
        for item in self.items.iter().rev() {
            match item {
                TranscriptItem::TurnFinished { .. } => return true,
                TranscriptItem::Injected(_) => continue,
                _ => return false,
            }
        }
        true
    }

    pub fn last_turn_id(&self) -> TurnId {
        self.items
            .iter()
            .rev()
            .find_map(TranscriptItem::turn_id)
            .unwrap_or_default()
    }

    fn patch_crashed_tail(items: &mut Vec<TranscriptItem>) {
        let Some((turn_id, tail_start)) = Self::open_tail_turn(items) else {
            return;
        };

        Self::patch_missing_tool_results(items, tail_start);
        items.push(TranscriptItem::TurnFinished {
            turn_id,
            outcome: TurnOutcome::Crashed,
        });
    }

    fn open_tail_turn(items: &[TranscriptItem]) -> Option<(TurnId, usize)> {
        items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, item)| match item {
                TranscriptItem::TurnStarted { turn_id } => Some(Some((*turn_id, index))),
                TranscriptItem::TurnFinished { .. } => Some(None),
                // Injected messages do not determine whether the tail turn is
                // still open; keep looking backward for the nearest turn marker.
                TranscriptItem::Injected(_)
                | TranscriptItem::UserMessage(_)
                | TranscriptItem::AssistantMessage(_)
                | TranscriptItem::ToolCallStarted { .. }
                | TranscriptItem::ToolResult(_) => None,
            })
            .flatten()
    }

    fn patch_missing_tool_results(items: &mut Vec<TranscriptItem>, tail_start: usize) {
        let mut tool_calls = Vec::new();
        let mut completed_tool_calls = Vec::new();

        for item in &items[tail_start..] {
            match item {
                TranscriptItem::AssistantMessage(message) => {
                    tool_calls.extend(message.tool_calls().cloned());
                }
                TranscriptItem::ToolResult(result) => {
                    completed_tool_calls.push((result.tool_call_id, result.tool_name.clone()));
                }
                TranscriptItem::TurnStarted { .. }
                | TranscriptItem::UserMessage(_)
                | TranscriptItem::ToolCallStarted { .. }
                | TranscriptItem::TurnFinished { .. }
                | TranscriptItem::Injected(_) => {}
            }
        }

        for tool_call in Self::missing_tool_calls(tool_calls, completed_tool_calls) {
            items.push(TranscriptItem::ToolResult(ToolResultMessage::crashed(
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

impl From<Vec<TranscriptItem>> for ModelContext {
    fn from(items: Vec<TranscriptItem>) -> Self {
        Self::from_transcript_items(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn turn_boundary_walks_past_injected_messages() {
        let transcript = ModelContext::from_transcript_items(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage("hi".to_string()),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptItem::Injected(InjectedMessage::new("compaction", "summary")),
            TranscriptItem::Injected(InjectedMessage::new("note", "branch note")),
        ]);
        assert!(transcript.is_turn_boundary());
    }

    #[test]
    fn crashed_tail_patches_missing_tool_results_before_finishing_turn() {
        let first = tool_call(1, "bash");
        let second = tool_call(2, "read");

        let transcript = ModelContext::from_transcript_items_recovering_crashed_tail(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(7) },
            TranscriptItem::UserMessage("hello".to_string()),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![
                    AssistantItem::ToolCall(first.clone()),
                    AssistantItem::ToolCall(second.clone()),
                ],
            }),
            TranscriptItem::ToolCallStarted {
                turn_id: TurnId(7),
                tool_call: first.clone(),
            },
            TranscriptItem::ToolCallStarted {
                turn_id: TurnId(7),
                tool_call: second.clone(),
            },
            TranscriptItem::ToolResult(tool_result(1, "bash")),
        ]);

        assert_eq!(
            transcript.transcript_items().last(),
            Some(&TranscriptItem::TurnFinished {
                turn_id: TurnId(7),
                outcome: TurnOutcome::Crashed,
            })
        );
        assert_eq!(
            transcript.transcript_items()[6],
            TranscriptItem::ToolResult(ToolResultMessage::crashed(second.id, "read"))
        );
    }

    #[test]
    fn crashed_tail_patches_assistant_tool_calls_even_without_start_items() {
        let tool_call = tool_call(1, "bash");

        let transcript = ModelContext::from_transcript_items_recovering_crashed_tail(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(8) },
            TranscriptItem::UserMessage("hello".to_string()),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            }),
        ]);

        assert_eq!(
            transcript.transcript_items()[3],
            TranscriptItem::ToolResult(ToolResultMessage::crashed(tool_call.id, "bash"))
        );
        assert_eq!(
            transcript.transcript_items()[4],
            TranscriptItem::TurnFinished {
                turn_id: TurnId(8),
                outcome: TurnOutcome::Crashed,
            }
        );
    }
}
