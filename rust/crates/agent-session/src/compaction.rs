use agent_core::{AssistantItem, TranscriptItem};

use crate::model_context::ModelContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCompactionSettings {
    pub max_context_tokens: usize,
}

impl AutoCompactionSettings {
    pub fn new(max_context_tokens: usize) -> Self {
        Self { max_context_tokens }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompactionRequestId(pub u64);

impl CompactionRequestId {
    pub fn first() -> Self {
        Self(1)
    }

    pub fn take_next(next: &mut Self) -> Self {
        let current = *next;
        next.0 += 1;
        current
    }
}

pub(crate) fn should_auto_compact(
    model_context: &ModelContext,
    settings: AutoCompactionSettings,
) -> bool {
    !model_context.transcript_items().is_empty()
        && estimate_items_tokens(model_context.transcript_items()) > settings.max_context_tokens
}

pub(crate) fn estimate_items_tokens(items: &[TranscriptItem]) -> usize {
    items.iter().map(estimate_item_tokens).sum()
}

fn estimate_item_tokens(item: &TranscriptItem) -> usize {
    let chars = match item {
        TranscriptItem::TurnStarted { .. } | TranscriptItem::TurnFinished { .. } => 0,
        TranscriptItem::UserMessage(content) => content.len(),
        TranscriptItem::AssistantMessage(message) => message
            .items
            .iter()
            .map(|item| match item {
                AssistantItem::Text(text) => text.len(),
                AssistantItem::ToolCall(tool_call) => {
                    tool_call.tool_name.len() + tool_call.args_json.len()
                }
            })
            .sum(),
        TranscriptItem::ToolCallStarted { tool_call, .. } => {
            tool_call.tool_name.len() + tool_call.args_json.len()
        }
        TranscriptItem::ToolResult(result) => result.tool_name.len() + result.output.len(),
        TranscriptItem::Injected(message) => message.content.len(),
    };
    chars.div_ceil(4)
}
