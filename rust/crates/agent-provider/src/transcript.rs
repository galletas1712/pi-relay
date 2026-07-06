use agent_tools::limit_tool_output;
use agent_vocab::{ProviderKind, ProviderReplayItem, ToolCall, TranscriptItem};

use crate::{canonical_tool_name_for_provider, ModelTranscriptEntry};

pub fn normalize_transcript_for_provider(
    transcript: Vec<ModelTranscriptEntry>,
) -> Vec<ModelTranscriptEntry> {
    transcript
        .into_iter()
        .map(|entry| ModelTranscriptEntry {
            item: normalize_transcript_item_for_provider(
                limit_transcript_tool_output(entry.item),
                entry.provider_replay.as_slice(),
            ),
            provider_replay: entry.provider_replay,
        })
        .collect()
}

fn limit_transcript_tool_output(item: TranscriptItem) -> TranscriptItem {
    match item {
        TranscriptItem::ToolResult(mut result) => {
            result.output = limit_tool_output(result.output);
            TranscriptItem::ToolResult(result)
        }
        item => item,
    }
}

fn normalize_transcript_item_for_provider(
    item: TranscriptItem,
    provider_replay: &[ProviderReplayItem],
) -> TranscriptItem {
    let Some(provider) = provider_replay.first().map(|record| record.provider) else {
        return item;
    };
    match item {
        TranscriptItem::AssistantMessage(mut message) => {
            for item in &mut message.items {
                if let agent_vocab::AssistantItem::ToolCall(call) = item {
                    canonicalize_owned_tool_call_for_provider(provider, call);
                }
            }
            TranscriptItem::AssistantMessage(message)
        }
        TranscriptItem::ToolCallStarted {
            turn_id,
            mut tool_call,
        } => {
            canonicalize_owned_tool_call_for_provider(provider, &mut tool_call);
            TranscriptItem::ToolCallStarted { turn_id, tool_call }
        }
        item => item,
    }
}

fn canonicalize_owned_tool_call_for_provider(provider: ProviderKind, call: &mut ToolCall) {
    let tool_name = canonical_tool_name_for_provider(provider, &call.tool_name);
    if tool_name != call.tool_name {
        let tool_name = tool_name.to_string();
        call.tool_name = tool_name;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_vocab::{ToolCallId, ToolResultMessage, TranscriptItem};

    #[test]
    fn provider_transcript_bounds_historical_tool_results() {
        let transcript = vec![ModelTranscriptEntry::from(TranscriptItem::ToolResult(
            ToolResultMessage::success(ToolCallId::from_u64(1), "bash", "x".repeat(50_000)),
        ))];

        let transcript = normalize_transcript_for_provider(transcript);
        let TranscriptItem::ToolResult(result) = &transcript[0].item else {
            panic!("expected tool result");
        };

        assert!(result.output.len() < 50_000);
        assert!(result.output.contains("[tool output truncated:"));
    }
}
