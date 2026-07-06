use agent_vocab::{
    ProviderReplayItem, ToolCall, ToolResultMessage, TranscriptItem, TurnId, TurnOutcome,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelContextEntry {
    pub item: TranscriptItem,
    pub provider_replay: Vec<ProviderReplayItem>,
}

fn transcript_item_bytes(item: &TranscriptItem) -> usize {
    match item {
        TranscriptItem::TurnStarted { .. } | TranscriptItem::TurnFinished { .. } => 0,
        TranscriptItem::UserMessage(message) => message
            .content
            .iter()
            .map(|block| match block {
                agent_vocab::ContentBlock::Text { text } => text.len(),
                agent_vocab::ContentBlock::Image { image } => {
                    image.mime_type.len()
                        + match &image.source {
                            agent_vocab::ImageSource::Base64(value)
                            | agent_vocab::ImageSource::Url(value) => value.len(),
                        }
                }
            })
            .sum(),
        TranscriptItem::AssistantMessage(message) => message
            .items
            .iter()
            .map(|item| match item {
                agent_vocab::AssistantItem::Text(text) => text.len(),
                agent_vocab::AssistantItem::ToolCall(call) => {
                    call.id.as_str().len() + call.tool_name.len() + call.args_json.len()
                }
            })
            .sum(),
        TranscriptItem::ToolCallStarted { tool_call, .. } => {
            tool_call.id.as_str().len() + tool_call.tool_name.len() + tool_call.args_json.len()
        }
        TranscriptItem::ToolResult(result) => {
            result.tool_call_id.as_str().len() + result.tool_name.len() + result.output.len()
        }
        TranscriptItem::CompactionSummary(summary) => {
            summary.source_session_id.len() + summary.source_leaf_id.len() + summary.summary.len()
        }
        TranscriptItem::DaemonToolObservation(observation) => {
            observation.tool_call_id.as_str().len()
                + observation.tool_name.len()
                + observation.args_json.len()
                + json_value_payload_bytes(&observation.result_json)
                + observation.summary.as_ref().map_or(0, String::len)
        }
    }
}

fn json_value_payload_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
        serde_json::Value::String(value) => value.len(),
        serde_json::Value::Array(values) => values.iter().map(json_value_payload_bytes).sum(),
        serde_json::Value::Object(values) => values
            .iter()
            .map(|(key, value)| key.len() + json_value_payload_bytes(value))
            .sum(),
    }
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

    pub(crate) fn close_open_turn(mut self) -> Self {
        Self::close_open_turn_items(&mut self.items);
        self.provider_replay.resize_with(self.items.len(), Vec::new);
        self
    }

    pub(crate) fn close_open_turn_to_boundary(mut self) -> Self {
        Self::close_open_turn_items_to_boundary(&mut self.items, TurnOutcome::Crashed);
        self.provider_replay.resize_with(self.items.len(), Vec::new);
        self
    }

    pub(crate) fn close_open_turn_to_interrupted_boundary(mut self) -> Self {
        Self::close_open_turn_items_to_boundary(&mut self.items, TurnOutcome::Interrupted);
        self.provider_replay.resize_with(self.items.len(), Vec::new);
        self
    }

    pub fn transcript_items(&self) -> &[TranscriptItem] {
        &self.items
    }

    /// Model-visible payload bytes used by deterministic copy probes.
    ///
    /// This is a content-size observation, not allocator or serialized wire
    /// usage. Structural fields and container capacity are intentionally not
    /// included.
    pub fn measured_content_bytes(&self) -> usize {
        self.items
            .iter()
            .zip(&self.provider_replay)
            .map(|(item, replay)| {
                transcript_item_bytes(item).saturating_add(
                    replay
                        .iter()
                        .map(|item| {
                            item.raw_json.len()
                                + item
                                    .display
                                    .as_ref()
                                    .map(|display| {
                                        display.pretty_name.len()
                                            + display.input_summary.as_ref().map_or(0, String::len)
                                    })
                                    .unwrap_or_default()
                        })
                        .sum::<usize>(),
                )
            })
            .sum()
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
        match self.items.last() {
            Some(TranscriptItem::TurnFinished { .. } | TranscriptItem::CompactionSummary(_)) => {
                true
            }
            Some(_) => false,
            None => true,
        }
    }

    pub fn last_turn_id(&self) -> TurnId {
        self.items
            .iter()
            .rev()
            .find_map(TranscriptItem::turn_id)
            .unwrap_or_default()
    }

    pub fn open_turn_entries(
        &self,
    ) -> Option<impl Iterator<Item = (&TranscriptItem, &[ProviderReplayItem])>> {
        let (items, provider_replay) = self.open_turn_slices()?;
        Some(
            items
                .iter()
                .zip(provider_replay)
                .map(|(item, replay)| (item, replay.as_slice())),
        )
    }

    pub fn split_before_open_turn(&self) -> Option<(Self, Vec<ModelContextEntry>)> {
        let (open_turn_items, open_turn_replay) = self.open_turn_slices()?;
        let turn_start = self.items.len() - open_turn_items.len();
        let prefix = Self {
            items: self.items[..turn_start].to_vec(),
            provider_replay: self.provider_replay[..turn_start].to_vec(),
        };
        let suffix = open_turn_items
            .iter()
            .cloned()
            .zip(open_turn_replay.iter().cloned())
            .map(|(item, provider_replay)| ModelContextEntry {
                item,
                provider_replay,
            })
            .collect();
        Some((prefix, suffix))
    }

    fn open_turn_slices(&self) -> Option<(&[TranscriptItem], &[Vec<ProviderReplayItem>])> {
        let (_, turn_start) = Self::open_turn_start(&self.items)?;
        Some((
            &self.items[turn_start..],
            &self.provider_replay[turn_start..],
        ))
    }

    pub fn open_turn_ready_to_continue(&self) -> Option<TurnId> {
        Self::open_turn_ready_to_continue_items(&self.items)
    }

    fn close_open_turn_items(items: &mut Vec<TranscriptItem>) {
        let Some((turn_id, turn_start)) = Self::open_turn_start(items) else {
            return;
        };

        Self::complete_open_tool_calls(items, turn_start, turn_id);
        if Self::open_turn_ready_to_continue_items(items).is_some() {
            return;
        }
        items.push(TranscriptItem::TurnFinished {
            turn_id,
            outcome: TurnOutcome::Crashed,
        });
    }

    fn close_open_turn_items_to_boundary(items: &mut Vec<TranscriptItem>, outcome: TurnOutcome) {
        let Some((turn_id, turn_start)) = Self::open_turn_start(items) else {
            return;
        };

        Self::complete_open_tool_calls(items, turn_start, turn_id);
        if !Self::is_turn_boundary_items(items) {
            items.push(TranscriptItem::TurnFinished { turn_id, outcome });
        }
    }

    fn open_turn_start(items: &[TranscriptItem]) -> Option<(TurnId, usize)> {
        let mut saw_open_tail = false;
        for (index, item) in items.iter().enumerate().rev() {
            match item {
                TranscriptItem::TurnStarted { turn_id } => return Some((*turn_id, index)),
                TranscriptItem::TurnFinished { .. } => return None,
                TranscriptItem::CompactionSummary(summary) => {
                    // Mid-turn compaction replaces the original `turn_started`
                    // with a summary root. If assistant/tool rows were appended
                    // after that root and the process died, crash recovery must
                    // still be able to complete the open turn instead of leaving
                    // the active leaf at a non-boundary tool item.
                    return saw_open_tail.then_some((summary.last_turn_id, index));
                }
                TranscriptItem::UserMessage(_)
                | TranscriptItem::DaemonToolObservation(_)
                | TranscriptItem::AssistantMessage(_)
                | TranscriptItem::ToolCallStarted { .. }
                | TranscriptItem::ToolResult(_) => saw_open_tail = true,
            }
        }
        None
    }

    fn is_turn_boundary_items(items: &[TranscriptItem]) -> bool {
        match items.last() {
            Some(TranscriptItem::TurnFinished { .. } | TranscriptItem::CompactionSummary(_)) => {
                true
            }
            Some(_) => false,
            None => true,
        }
    }

    fn complete_open_tool_calls(
        items: &mut Vec<TranscriptItem>,
        turn_start: usize,
        turn_id: TurnId,
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
                | TranscriptItem::DaemonToolObservation(_)
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
                items.push(TranscriptItem::ToolResult(ToolResultMessage::crashed(
                    tool_call.id,
                    tool_call.tool_name,
                )));
            }
        }
    }

    fn open_turn_ready_to_continue_items(items: &[TranscriptItem]) -> Option<TurnId> {
        let (turn_id, turn_start) = Self::open_turn_start(items)?;
        let mut tool_calls = Vec::<ToolCall>::new();
        let mut tool_results = Vec::<ToolResultMessage>::new();
        for item in &items[turn_start..] {
            match item {
                TranscriptItem::AssistantMessage(message) => {
                    tool_calls.extend(message.tool_calls().cloned());
                }
                TranscriptItem::ToolResult(result) => tool_results.push(result.clone()),
                TranscriptItem::TurnStarted { .. }
                | TranscriptItem::UserMessage(_)
                | TranscriptItem::DaemonToolObservation(_)
                | TranscriptItem::ToolCallStarted { .. }
                | TranscriptItem::TurnFinished { .. }
                | TranscriptItem::CompactionSummary(_) => {}
            }
        }
        if tool_calls.is_empty() {
            return None;
        }
        let all_tools_have_results = tool_calls.into_iter().all(|tool_call| {
            tool_results.iter().any(|result| {
                result.tool_call_id == tool_call.id && result.tool_name == tool_call.tool_name
            })
        });
        all_tools_have_results.then_some(turn_id)
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
    #[ignore = "allocates four 10 MiB contexts; deterministic clone-cost probe"]
    fn common_context_clone_cost_probe() {
        const MIB: usize = 10;
        const CLONES: usize = 4;
        let context = ModelContext::from_transcript_items(vec![TranscriptItem::UserMessage(
            UserMessage::text("x".repeat(MIB * 1024 * 1024)),
        )]);

        let started = std::time::Instant::now();
        let copies = std::iter::repeat_with(|| context.clone())
            .take(CLONES)
            .collect::<Vec<_>>();
        let elapsed = started.elapsed();
        let copied_bytes = copies
            .iter()
            .map(ModelContext::measured_content_bytes)
            .sum::<usize>();

        assert_eq!(copied_bytes, MIB * 1024 * 1024 * CLONES);
        eprintln!(
            "perf fixture=common_context_clones clone_sites=dispatch_vector,session_start,deferred_subagent,title_worker clones={CLONES} copied_bytes={copied_bytes} elapsed_ns={} mib_per_second={:.1}",
            elapsed.as_nanos(),
            MIB as f64 * CLONES as f64 / elapsed.as_secs_f64()
        );
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
    fn split_before_open_turn_preserves_whole_open_turn_suffix() {
        let replay = ProviderReplayItem {
            provider: agent_vocab::ProviderKind::OpenAi,
            raw_json: r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"tool please"}]}"#
                .to_string(),
            display: None,
        };
        let tool = tool_call(1, "bash");
        let context = ModelContext::from_entries(vec![
            ModelContextEntry {
                item: TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                provider_replay: Vec::new(),
            },
            ModelContextEntry {
                item: TranscriptItem::UserMessage(UserMessage::text("old")),
                provider_replay: Vec::new(),
            },
            ModelContextEntry {
                item: TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
                provider_replay: Vec::new(),
            },
            ModelContextEntry {
                item: TranscriptItem::TurnStarted { turn_id: TurnId(2) },
                provider_replay: Vec::new(),
            },
            ModelContextEntry {
                item: TranscriptItem::UserMessage(UserMessage::text("current")),
                provider_replay: Vec::new(),
            },
            ModelContextEntry {
                item: TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::ToolCall(tool.clone())],
                }),
                provider_replay: vec![replay.clone()],
            },
            ModelContextEntry {
                item: TranscriptItem::ToolCallStarted {
                    turn_id: TurnId(2),
                    tool_call: tool,
                },
                provider_replay: Vec::new(),
            },
        ]);

        let (prefix, suffix) = context
            .split_before_open_turn()
            .expect("open turn should split");

        assert_eq!(prefix.last_turn_id(), TurnId(1));
        assert_eq!(prefix.transcript_items().len(), 3);
        assert_eq!(suffix.len(), 4);
        assert!(matches!(
            suffix[0].item,
            TranscriptItem::TurnStarted { turn_id: TurnId(2) }
        ));
        assert!(matches!(suffix[1].item, TranscriptItem::UserMessage(_)));
        assert!(matches!(
            suffix[2].item,
            TranscriptItem::AssistantMessage(_)
        ));
        assert_eq!(suffix[2].provider_replay, vec![replay]);
        assert!(matches!(
            suffix[3].item,
            TranscriptItem::ToolCallStarted { .. }
        ));
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
        .close_open_turn();

        assert_eq!(
            transcript.transcript_items().last(),
            Some(&TranscriptItem::ToolResult(ToolResultMessage::crashed(
                second.id.clone(),
                "read"
            )))
        );
        assert_eq!(
            transcript.transcript_items()[6],
            TranscriptItem::ToolResult(ToolResultMessage::crashed(second.id.clone(), "read"))
        );
        assert_eq!(transcript.open_turn_ready_to_continue(), Some(TurnId(7)));
    }

    #[test]
    fn crashed_compacted_tail_patches_missing_tool_results_and_finishes_turn() {
        let first = tool_call(1, "bash");
        let second = tool_call(2, "read");

        let transcript = ModelContext::from_transcript_items(vec![
            TranscriptItem::CompactionSummary(CompactionSummary::new(
                "session",
                "source",
                "summary",
                None,
                TurnId(58),
            )),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![
                    AssistantItem::ToolCall(first.clone()),
                    AssistantItem::ToolCall(second.clone()),
                ],
            }),
            TranscriptItem::ToolCallStarted {
                turn_id: TurnId(58),
                tool_call: first.clone(),
            },
            TranscriptItem::ToolResult(tool_result(first.id.clone(), "bash")),
        ])
        .close_open_turn_to_boundary();

        assert!(transcript.transcript_items().iter().any(|item| matches!(
            item,
            TranscriptItem::ToolCallStarted { turn_id, tool_call }
                if *turn_id == TurnId(58) && tool_call.id == second.id
        )));
        assert!(transcript.transcript_items().iter().any(|item| matches!(
            item,
            TranscriptItem::ToolResult(result)
                if result.tool_call_id == second.id && result.status == ToolResultStatus::Crashed
        )));
        assert_eq!(
            transcript.transcript_items().last(),
            Some(&TranscriptItem::TurnFinished {
                turn_id: TurnId(58),
                outcome: TurnOutcome::Crashed,
            })
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
        .close_open_turn();

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
        assert_eq!(transcript.transcript_items().len(), 5);
        assert_eq!(transcript.open_turn_ready_to_continue(), Some(TurnId(8)));
    }
}
