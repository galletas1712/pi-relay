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
        Self::patch_open_tail(&mut items, TurnOutcome::Crashed);
        Self { items }
    }

    pub fn from_transcript_items_recovering_interrupted_tail(
        mut items: Vec<TranscriptItem>,
    ) -> Self {
        Self::patch_open_tail(&mut items, TurnOutcome::Interrupted);
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

    pub(crate) fn structural_error(&self) -> Option<String> {
        let mut active_turn: Option<ActiveTurnValidation> = None;

        for item in &self.items {
            match item {
                TranscriptItem::TurnStarted { turn_id } => {
                    if active_turn.is_some() {
                        return Some(format!(
                            "turn {:?} starts before prior turn finished",
                            turn_id
                        ));
                    }
                    active_turn = Some(ActiveTurnValidation::new(*turn_id));
                }
                TranscriptItem::UserMessage(_) => {
                    if active_turn.is_none() {
                        return Some("user message appears outside a turn".to_string());
                    }
                }
                TranscriptItem::Injected(_) => {}
                TranscriptItem::AssistantMessage(message) => {
                    let Some(turn) = active_turn.as_mut() else {
                        return Some("assistant message appears outside a turn".to_string());
                    };
                    if turn.has_pending_tools() {
                        return Some(
                            "assistant message appears before prior tool calls completed"
                                .to_string(),
                        );
                    }
                    turn.awaiting_starts.extend(message.tool_calls().cloned());
                }
                TranscriptItem::ToolCallStarted { turn_id, tool_call } => {
                    let Some(turn) = active_turn.as_mut() else {
                        return Some("tool call starts outside a turn".to_string());
                    };
                    if turn.turn_id != *turn_id {
                        return Some(format!(
                            "tool call start turn {:?} does not match open turn {:?}",
                            turn_id, turn.turn_id
                        ));
                    }
                    let Some(index) = turn.awaiting_starts.iter().position(|pending| {
                        pending.id == tool_call.id && pending.tool_name == tool_call.tool_name
                    }) else {
                        return Some(format!(
                            "tool call {:?}/{} starts without a matching assistant tool call",
                            tool_call.id, tool_call.tool_name
                        ));
                    };
                    let tool_call = turn.awaiting_starts.remove(index);
                    turn.awaiting_results.push(tool_call);
                }
                TranscriptItem::ToolResult(result) => {
                    let Some(turn) = active_turn.as_mut() else {
                        return Some("tool result appears outside a turn".to_string());
                    };
                    let Some(index) = turn.awaiting_results.iter().position(|pending| {
                        pending.id == result.tool_call_id && pending.tool_name == result.tool_name
                    }) else {
                        return Some(format!(
                            "tool result {:?}/{} has no matching started tool call",
                            result.tool_call_id, result.tool_name
                        ));
                    };
                    turn.awaiting_results.remove(index);
                }
                TranscriptItem::TurnFinished { turn_id, .. } => {
                    let Some(turn) = active_turn.take() else {
                        return Some("turn finished appears without an open turn".to_string());
                    };
                    if turn.turn_id != *turn_id {
                        return Some(format!(
                            "turn finish {:?} does not match open turn {:?}",
                            turn_id, turn.turn_id
                        ));
                    }
                    if turn.has_pending_tools() {
                        return Some(
                            "turn finished before assistant tool calls had matching results"
                                .to_string(),
                        );
                    }
                }
            }
        }

        if let Some(turn) = active_turn {
            if turn.has_pending_tools() {
                return Some(
                    "open turn has assistant tool calls without matching results".to_string(),
                );
            }
        }

        None
    }

    fn patch_open_tail(items: &mut Vec<TranscriptItem>, outcome: TurnOutcome) {
        let Some((turn_id, tail_start)) = Self::open_tail_turn(items) else {
            return;
        };

        Self::patch_missing_tool_results(items, tail_start, turn_id, outcome);
        items.push(TranscriptItem::TurnFinished { turn_id, outcome });
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

    fn patch_missing_tool_results(
        items: &mut Vec<TranscriptItem>,
        tail_start: usize,
        turn_id: TurnId,
        outcome: TurnOutcome,
    ) {
        let mut tool_calls = Vec::new();
        let mut started_tool_calls = Vec::new();
        let mut completed_tool_calls = Vec::new();

        for item in &items[tail_start..] {
            match item {
                TranscriptItem::AssistantMessage(message) => {
                    tool_calls.extend(message.tool_calls().cloned());
                }
                TranscriptItem::ToolCallStarted { tool_call, .. } => {
                    started_tool_calls.push((tool_call.id.clone(), tool_call.tool_name.clone()));
                }
                TranscriptItem::ToolResult(result) => {
                    completed_tool_calls
                        .push((result.tool_call_id.clone(), result.tool_name.clone()));
                }
                TranscriptItem::TurnStarted { .. }
                | TranscriptItem::UserMessage(_)
                | TranscriptItem::TurnFinished { .. }
                | TranscriptItem::Injected(_) => {}
            }
        }

        for tool_call in tool_calls {
            if !Self::remove_matching_tool_call(&mut started_tool_calls, &tool_call) {
                items.push(TranscriptItem::ToolCallStarted {
                    turn_id,
                    tool_call: tool_call.clone(),
                });
            }
            if !Self::remove_matching_tool_call(&mut completed_tool_calls, &tool_call) {
                let result = match outcome {
                    TurnOutcome::Interrupted => {
                        ToolResultMessage::interrupted(tool_call.id, tool_call.tool_name)
                    }
                    TurnOutcome::Graceful | TurnOutcome::Crashed => {
                        ToolResultMessage::crashed(tool_call.id, tool_call.tool_name)
                    }
                };
                items.push(TranscriptItem::ToolResult(result));
            }
        }
    }

    fn remove_matching_tool_call(
        tool_calls: &mut Vec<(ToolCallId, String)>,
        tool_call: &ToolCall,
    ) -> bool {
        let Some(index) = tool_calls
            .iter()
            .position(|(id, name)| *id == tool_call.id && name == &tool_call.tool_name)
        else {
            return false;
        };
        tool_calls.remove(index);
        true
    }
}

struct ActiveTurnValidation {
    turn_id: TurnId,
    awaiting_starts: Vec<ToolCall>,
    awaiting_results: Vec<ToolCall>,
}

impl ActiveTurnValidation {
    fn new(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            awaiting_starts: Vec::new(),
            awaiting_results: Vec::new(),
        }
    }

    fn has_pending_tools(&self) -> bool {
        !self.awaiting_starts.is_empty() || !self.awaiting_results.is_empty()
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
    use agent_core::{
        AssistantItem, AssistantMessage, InjectedMessage, ToolResultStatus, UserMessage,
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
    fn turn_boundary_walks_past_injected_messages() {
        let transcript = ModelContext::from_transcript_items(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage(UserMessage::text("hi")),
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
            TranscriptItem::ToolResult(ToolResultMessage::crashed(second.id.clone(), "read"))
        );
    }

    #[test]
    fn crashed_tail_patches_assistant_tool_calls_even_without_start_items() {
        let tool_call = tool_call(1, "bash");

        let transcript = ModelContext::from_transcript_items_recovering_crashed_tail(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(8) },
            TranscriptItem::UserMessage(UserMessage::text("hello")),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            }),
        ]);

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

    #[test]
    fn detects_structurally_invalid_tool_sequences() {
        let tool_call = tool_call(1, "bash");
        let missing_result = ModelContext::from_transcript_items(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            }),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ]);
        assert!(missing_result.structural_error().is_some());

        let complete = ModelContext::from_transcript_items(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            }),
            TranscriptItem::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: tool_call.clone(),
            },
            TranscriptItem::ToolResult(tool_result(tool_call.id.clone(), &tool_call.tool_name)),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ]);
        assert_eq!(complete.structural_error(), None);
    }
}
