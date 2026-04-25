use agent_core::{AssistantItem, TranscriptItem};

pub(crate) fn estimate_items_tokens(items: &[TranscriptItem]) -> usize {
    items.iter().map(estimate_item_tokens).sum()
}

pub(crate) fn estimate_item_tokens(item: &TranscriptItem) -> usize {
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
