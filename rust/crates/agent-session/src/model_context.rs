use agent_vocab::{
    ProviderReplayItem, ToolCall, ToolResultMessage, TranscriptItem, TurnId, TurnOutcome,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelContextEntry {
    pub item: TranscriptItem,
    pub provider_replay: Vec<ProviderReplayItem>,
}

/// Materialized model context for one transcript path.
///
/// The transcript store is canonical; `ModelContext` is a derived view over a
/// transcript item sequence. The session rebuilds one whenever it needs to feed
/// the core loop or model provider a contiguous ordered history, and uses the
/// same type to make copied or restored open turns structurally complete.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModelContext {
    items: Vec<TranscriptItem>,
    provider_replay: Vec<Vec<ProviderReplayItem>>,
}

impl ModelContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_transcript_items(items: Vec<TranscriptItem>) -> Self {
        let provider_replay = vec![Vec::new(); items.len()];
        Self {
            items,
            provider_replay,
        }
    }

    pub fn from_entries(entries: Vec<ModelContextEntry>) -> Self {
        let mut items = Vec::with_capacity(entries.len());
        let mut provider_replay = Vec::with_capacity(entries.len());
        for entry in entries {
            items.push(entry.item);
            provider_replay.push(entry.provider_replay);
        }
        Self {
            items,
            provider_replay,
        }
    }

    pub fn from_transcript_items_closing_open_turn_as_interrupted(
        items: Vec<TranscriptItem>,
    ) -> Self {
        Self::from_transcript_items(items).close_open_turn(OpenTurnClosure::Interrupted)
    }

    pub(crate) fn close_open_turn(mut self, closure: OpenTurnClosure) -> Self {
        Self::close_open_turn_items(&mut self.items, closure);
        self.provider_replay.resize_with(self.items.len(), Vec::new);
        self
    }

    pub fn transcript_items(&self) -> &[TranscriptItem] {
        &self.items
    }

    pub fn into_transcript_items(self) -> Vec<TranscriptItem> {
        self.items
    }

    pub fn into_entries(self) -> Vec<ModelContextEntry> {
        self.items
            .into_iter()
            .zip(self.provider_replay)
            .map(|(item, provider_replay)| ModelContextEntry {
                item,
                provider_replay,
            })
            .collect()
    }

    pub fn is_turn_boundary(&self) -> bool {
        for item in self.items.iter().rev() {
            match item {
                TranscriptItem::TurnFinished { .. } => return true,
                TranscriptItem::CompactionSummary(_) => return true,
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

    fn close_open_turn_items(items: &mut Vec<TranscriptItem>, closure: OpenTurnClosure) {
        let Some((turn_id, turn_start)) = Self::open_turn_start(items) else {
            return;
        };

        Self::complete_open_tool_calls(items, turn_start, turn_id, closure);
        items.push(TranscriptItem::TurnFinished {
            turn_id,
            outcome: closure.turn_outcome(),
        });
    }

    fn open_turn_start(items: &[TranscriptItem]) -> Option<(TurnId, usize)> {
        items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, item)| match item {
                TranscriptItem::TurnStarted { turn_id } => Some(Some((*turn_id, index))),
                TranscriptItem::TurnFinished { .. } => Some(None),
                TranscriptItem::CompactionSummary(_)
                | TranscriptItem::UserMessage(_)
                | TranscriptItem::AssistantMessage(_)
                | TranscriptItem::ToolCallStarted { .. }
                | TranscriptItem::ToolResult(_) => None,
            })
            .flatten()
    }

    fn complete_open_tool_calls(
        items: &mut Vec<TranscriptItem>,
        turn_start: usize,
        turn_id: TurnId,
        closure: OpenTurnClosure,
    ) {
        let mut tool_calls = Vec::new();
        let mut started_tool_calls = Vec::<ToolCall>::new();
        let mut completed_tool_results = Vec::<ToolResultMessage>::new();

        for item in &items[turn_start..] {
            match item {
                TranscriptItem::AssistantMessage(message) => {
                    tool_calls.extend(message.tool_calls().cloned());
                }
                TranscriptItem::ToolCallStarted { tool_call, .. } => {
                    started_tool_calls.push(tool_call.clone());
                }
                TranscriptItem::ToolResult(result) => {
                    completed_tool_results.push(result.clone());
                }
                TranscriptItem::TurnStarted { .. }
                | TranscriptItem::UserMessage(_)
                | TranscriptItem::TurnFinished { .. }
                | TranscriptItem::CompactionSummary(_) => {}
            }
        }

        for tool_call in tool_calls {
            if let Some(index) = started_tool_calls.iter().position(|started| {
                started.id == tool_call.id && started.tool_name == tool_call.tool_name
            }) {
                started_tool_calls.remove(index);
            } else {
                items.push(TranscriptItem::ToolCallStarted {
                    turn_id,
                    tool_call: tool_call.clone(),
                });
            }
            if let Some(index) = completed_tool_results.iter().position(|result| {
                result.tool_call_id == tool_call.id && result.tool_name == tool_call.tool_name
            }) {
                completed_tool_results.remove(index);
            } else {
                items.push(TranscriptItem::ToolResult(
                    closure.missing_tool_result(tool_call),
                ));
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenTurnClosure {
    Crashed,
    Interrupted,
}

impl OpenTurnClosure {
    fn turn_outcome(self) -> TurnOutcome {
        match self {
            Self::Crashed => TurnOutcome::Crashed,
            Self::Interrupted => TurnOutcome::Interrupted,
        }
    }

    fn missing_tool_result(self, tool_call: ToolCall) -> ToolResultMessage {
        match self {
            Self::Crashed => ToolResultMessage::crashed(tool_call.id, tool_call.tool_name),
            Self::Interrupted => ToolResultMessage::interrupted(tool_call.id, tool_call.tool_name),
        }
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
    use agent_vocab::{
        AssistantItem, AssistantMessage, CompactionSummary, ToolCallId, ToolResultStatus,
        UserMessage,
    };

    fn tool_call(id: u64, name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::from_u64(id),
            tool_name: name.to_string(),
            args_json: "{}".to_string(),
        }
    }

    fn tool_result(id: impl Into<ToolCallId>, name: &str) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: id.into(),
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
    fn compaction_summary_is_a_turn_boundary() {
        let transcript =
            ModelContext::from_transcript_items(vec![TranscriptItem::CompactionSummary(
                CompactionSummary::new("session", "source", "summary", None, TurnId(3)),
            )]);
        assert!(transcript.is_turn_boundary());
        assert_eq!(transcript.last_turn_id(), TurnId(3));
    }

    #[test]
    fn crashed_tail_patches_missing_tool_results_before_finishing_turn() {
        let first = tool_call(1, "bash");
        let second = tool_call(2, "read");

        let transcript = ModelContext::from_transcript_items(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(7) },
            TranscriptItem::UserMessage(UserMessage::text("hello")),
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
        ])
        .close_open_turn(OpenTurnClosure::Crashed);

        assert_eq!(
            transcript.transcript_items().last(),
            Some(&TranscriptItem::TurnFinished {
                turn_id: TurnId(7),
                outcome: TurnOutcome::Crashed,
            })
        );
        assert_eq!(
            transcript.transcript_items()[6],
            TranscriptItem::ToolResult(ToolResultMessage::crashed(second.id.clone(), "read"))
        );
    }

    #[test]
    fn crashed_tail_patches_assistant_tool_calls_even_without_start_items() {
        let tool_call = tool_call(1, "bash");

        let transcript = ModelContext::from_transcript_items(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(8) },
            TranscriptItem::UserMessage(UserMessage::text("hello")),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            }),
        ])
        .close_open_turn(OpenTurnClosure::Crashed);

        assert_eq!(
            transcript.transcript_items()[3],
            TranscriptItem::ToolCallStarted {
                turn_id: TurnId(8),
                tool_call: tool_call.clone(),
            }
        );
        assert_eq!(
            transcript.transcript_items()[4],
            TranscriptItem::ToolResult(ToolResultMessage::crashed(tool_call.id.clone(), "bash"))
        );
        assert_eq!(
            transcript.transcript_items()[5],
            TranscriptItem::TurnFinished {
                turn_id: TurnId(8),
                outcome: TurnOutcome::Crashed,
            }
        );
    }
}
