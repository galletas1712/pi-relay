use agent_core::{AssistantItem, ContextItem};

pub(crate) fn estimate_records_tokens(records: &[ContextItem]) -> usize {
    records.iter().map(estimate_record_tokens).sum()
}

pub(crate) fn estimate_record_tokens(record: &ContextItem) -> usize {
    let chars = match record {
        ContextItem::TurnStarted { .. } | ContextItem::TurnFinished { .. } => 0,
        ContextItem::UserMessage(content) => content.len(),
        ContextItem::AssistantMessage(message) => message
            .items
            .iter()
            .map(|item| match item {
                AssistantItem::Text(text) => text.len(),
                AssistantItem::ToolCall(tool_call) => {
                    tool_call.tool_name.len() + tool_call.args_json.len()
                }
            })
            .sum(),
        ContextItem::ToolCallStarted { tool_call, .. } => {
            tool_call.tool_name.len() + tool_call.args_json.len()
        }
        ContextItem::ToolResult(result) => result.tool_name.len() + result.output.len(),
        ContextItem::Injected(message) => message.content.len(),
    };
    chars.div_ceil(4)
}
