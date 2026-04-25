use agent_core::{AssistantItem, TranscriptRecord};

pub(crate) fn estimate_records_tokens(records: &[TranscriptRecord]) -> usize {
    records.iter().map(estimate_record_tokens).sum()
}

pub(crate) fn estimate_record_tokens(record: &TranscriptRecord) -> usize {
    let chars = match record {
        TranscriptRecord::TurnStarted { .. } | TranscriptRecord::TurnFinished { .. } => 0,
        TranscriptRecord::UserMessage(content) => content.len(),
        TranscriptRecord::AssistantMessage(message) => message
            .items
            .iter()
            .map(|item| match item {
                AssistantItem::Text(text) => text.len(),
                AssistantItem::ToolCall(tool_call) => {
                    tool_call.tool_name.len() + tool_call.args_json.len()
                }
            })
            .sum(),
        TranscriptRecord::ToolCallStarted { tool_call, .. } => {
            tool_call.tool_name.len() + tool_call.args_json.len()
        }
        TranscriptRecord::ToolResult(result) => result.tool_name.len() + result.output.len(),
        TranscriptRecord::Injected(message) => message.content.len(),
    };
    chars.div_ceil(4)
}
