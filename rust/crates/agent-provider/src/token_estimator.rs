use agent_vocab::{
    AssistantMessage, ContentBlock, ProviderKind, ProviderReplayItem, ToolCall, ToolResultMessage,
    TranscriptItem, UserMessage,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::{canonical_tool_name_for_provider, ModelTranscriptEntry, PromptSections};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenEstimate {
    pub tokens: usize,
    pub model_visible_bytes: usize,
}

impl TokenEstimate {
    pub fn from_model_visible_bytes(model_visible_bytes: usize) -> Self {
        Self {
            tokens: approx_tokens_from_byte_count(model_visible_bytes),
            model_visible_bytes,
        }
    }

    pub fn saturating_add(self, other: Self) -> Self {
        Self::from_model_visible_bytes(
            self.model_visible_bytes
                .saturating_add(other.model_visible_bytes),
        )
    }
}

impl std::iter::Sum for TokenEstimate {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::from_model_visible_bytes(0), Self::saturating_add)
    }
}

pub fn estimate_model_input_tokens(
    prompt: &PromptSections,
    transcript: &[ModelTranscriptEntry],
    provider: ProviderKind,
) -> usize {
    estimate_model_input(prompt, transcript, provider).tokens
}

pub fn estimate_model_input(
    prompt: &PromptSections,
    transcript: &[ModelTranscriptEntry],
    provider: ProviderKind,
) -> TokenEstimate {
    prompt_estimate(prompt).saturating_add(estimate_transcript_tokens(transcript, provider))
}

pub fn estimate_transcript_tokens(
    transcript: &[ModelTranscriptEntry],
    provider: ProviderKind,
) -> TokenEstimate {
    transcript
        .iter()
        .map(|entry| estimate_transcript_entry(entry, provider))
        .sum()
}

pub fn estimate_transcript_entry(
    entry: &ModelTranscriptEntry,
    provider: ProviderKind,
) -> TokenEstimate {
    match entry.item() {
        TranscriptItem::UserMessage(message) => {
            serialized_estimate(&user_message_wire(message, provider))
        }
        TranscriptItem::AssistantMessage(message) => {
            let replay = entry.provider_replay_for(provider);
            if replay.is_empty() {
                serialized_estimate(&assistant_wire(message, provider))
            } else {
                replay_estimate(&replay)
            }
        }
        TranscriptItem::ToolResult(result) => {
            serialized_estimate(&tool_result_wire(result, provider))
        }
        TranscriptItem::CompactionSummary(summary) => {
            let replay = entry.provider_replay_for(provider);
            if replay.is_empty() {
                serialized_estimate(&compaction_summary_wire(summary, provider))
            } else {
                replay_estimate(&replay)
            }
        }
        TranscriptItem::TurnStarted { .. }
        | TranscriptItem::ToolCallStarted { .. }
        | TranscriptItem::TurnFinished { .. } => TokenEstimate::from_model_visible_bytes(0),
    }
}

fn prompt_estimate(prompt: &PromptSections) -> TokenEstimate {
    prompt
        .render_joined()
        .map(|text| serialized_estimate(&text))
        .unwrap_or_else(|| TokenEstimate::from_model_visible_bytes(0))
}

fn replay_estimate(replay: &[ProviderReplayItem]) -> TokenEstimate {
    replay
        .iter()
        .map(|record| serialized_estimate(&record.raw_json))
        .sum()
}

fn serialized_estimate<T: Serialize>(value: &T) -> TokenEstimate {
    let bytes = serde_json::to_string(value)
        .map(|serialized| model_visible_bytes_for_serialized_json(&serialized))
        .unwrap_or_default();
    TokenEstimate::from_model_visible_bytes(bytes)
}

fn model_visible_bytes_for_serialized_json(serialized: &str) -> usize {
    let raw = serialized.len();
    let image_adjustment = estimate_image_data_url_adjustment(serialized);
    raw.saturating_sub(image_adjustment.payload_bytes)
        .saturating_add(image_adjustment.replacement_bytes)
}

#[derive(Debug, Clone, Copy, Default)]
struct ImageDataUrlAdjustment {
    payload_bytes: usize,
    replacement_bytes: usize,
}

fn estimate_image_data_url_adjustment(serialized: &str) -> ImageDataUrlAdjustment {
    const DATA_PREFIX: &str = "data:";
    let mut adjustment = ImageDataUrlAdjustment::default();
    let mut offset = 0usize;
    while let Some(relative_start) = serialized[offset..].find(DATA_PREFIX) {
        let start = offset + relative_start;
        let Some(relative_end) = serialized[start..].find('"') else {
            break;
        };
        let raw_url = &serialized[start..start + relative_end];
        if let Some(payload_len) = parse_base64_image_data_url_payload_len(raw_url) {
            adjustment.payload_bytes = adjustment.payload_bytes.saturating_add(payload_len);
            adjustment.replacement_bytes = adjustment
                .replacement_bytes
                .saturating_add(RESIZED_IMAGE_BYTES_ESTIMATE);
        }
        offset = start.saturating_add(relative_end).saturating_add(1);
    }
    adjustment
}

fn parse_base64_image_data_url_payload_len(url: &str) -> Option<usize> {
    if !url
        .get(.."data:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        return None;
    }
    let comma_index = url.find(',')?;
    let metadata = &url[..comma_index];
    let payload = &url[comma_index + 1..];
    let metadata_without_scheme = &metadata["data:".len()..];
    let mut metadata_parts = metadata_without_scheme.split(';');
    let mime_type = metadata_parts.next().unwrap_or_default();
    let has_base64_marker = metadata_parts.any(|part| part.eq_ignore_ascii_case("base64"));
    if !mime_type
        .get(.."image/".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
    {
        return None;
    }
    has_base64_marker.then_some(payload.len())
}

fn user_message_wire(message: &UserMessage, provider: ProviderKind) -> Value {
    match provider {
        ProviderKind::OpenAi => json!({
            "type": "message",
            "role": "user",
            "content": message
                .content
                .iter()
                .map(openai_content_block_wire)
                .collect::<Vec<_>>(),
        }),
        ProviderKind::Claude => json!({
            "role": "user",
            "content": message
                .content
                .iter()
                .map(anthropic_content_block_wire)
                .collect::<Vec<_>>(),
        }),
    }
}

fn openai_content_block_wire(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({ "type": "input_text", "text": text }),
        ContentBlock::Image { image } => match &image.source {
            agent_vocab::ImageSource::Url(url) => {
                json!({ "type": "input_image", "image_url": url })
            }
            agent_vocab::ImageSource::Base64(data) => {
                let url = format!("data:{};base64,{}", image.mime_type, data);
                json!({ "type": "input_image", "image_url": url })
            }
        },
    }
}

fn anthropic_content_block_wire(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Image { image } => match &image.source {
            agent_vocab::ImageSource::Url(url) => {
                json!({ "type": "image", "source": { "type": "url", "url": url } })
            }
            agent_vocab::ImageSource::Base64(data) => json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.mime_type,
                    "data": data,
                }
            }),
        },
    }
}

fn compaction_summary_wire(
    summary: &agent_vocab::CompactionSummary,
    provider: ProviderKind,
) -> Value {
    let text = format!(
        "The conversation history before this point was compacted into this summary:\n\n{}",
        summary.summary
    );
    match provider {
        ProviderKind::OpenAi => json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        }),
        ProviderKind::Claude => json!({
            "role": "user",
            "content": [{ "type": "text", "text": text }],
        }),
    }
}

fn assistant_wire(message: &AssistantMessage, provider: ProviderKind) -> Vec<Value> {
    match provider {
        ProviderKind::OpenAi => openai_assistant_wire(message),
        ProviderKind::Claude => vec![anthropic_assistant_wire(message)],
    }
}

fn openai_assistant_wire(message: &AssistantMessage) -> Vec<Value> {
    let mut items = Vec::new();
    let text = message.text();
    if !text.is_empty() {
        items.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        }));
    }
    for call in message.tool_calls() {
        items.push(tool_call_wire(call, ProviderKind::OpenAi));
    }
    items
}

fn anthropic_assistant_wire(message: &AssistantMessage) -> Value {
    let content = message
        .items
        .iter()
        .map(|item| match item {
            agent_vocab::AssistantItem::Text(text) => json!({ "type": "text", "text": text }),
            agent_vocab::AssistantItem::ToolCall(call) => json!({
                "type": "tool_use",
                "id": call.id.as_str(),
                "name": anthropic_wire_tool_name(&call.tool_name),
                "input": call.args_value().unwrap_or_else(|_| json!({})),
            }),
        })
        .collect::<Vec<_>>();
    json!({ "role": "assistant", "content": content })
}

fn tool_call_wire(call: &ToolCall, provider: ProviderKind) -> Value {
    let tool_name = canonical_tool_name_for_provider(provider, &call.tool_name);
    if tool_name == "Edit" {
        let input = call
            .args_value()
            .ok()
            .and_then(|value| {
                value
                    .get("input")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| call.args_json.clone());
        json!({
            "type": "custom_tool_call",
            "call_id": call.id.as_str(),
            "name": openai_wire_tool_name(tool_name),
            "input": input,
        })
    } else {
        json!({
            "type": "function_call",
            "call_id": call.id.as_str(),
            "name": openai_wire_tool_name(tool_name),
            "arguments": call.args_json,
        })
    }
}

fn openai_wire_tool_name(canonical_name: &str) -> &str {
    match canonical_name {
        "Edit" => "apply_patch",
        "WebFetch" => "web_fetch",
        "WebSearch" => "web_search",
        other => other,
    }
}

fn anthropic_wire_tool_name(canonical_name: &str) -> &str {
    match canonical_name {
        "WebFetch" => "web_fetch",
        "WebSearch" => "web_search",
        other => other,
    }
}

fn tool_result_wire(result: &ToolResultMessage, provider: ProviderKind) -> Value {
    match provider {
        ProviderKind::OpenAi => openai_tool_result_wire(result),
        ProviderKind::Claude => anthropic_tool_result_wire(result),
    }
}

fn openai_tool_result_wire(result: &ToolResultMessage) -> Value {
    if result.tool_name == "Edit" {
        json!({
            "type": "custom_tool_call_output",
            "call_id": result.tool_call_id.as_str(),
            "output": result.output,
        })
    } else {
        json!({
            "type": "function_call_output",
            "call_id": result.tool_call_id.as_str(),
            "output": result.output,
        })
    }
}

fn anthropic_tool_result_wire(result: &ToolResultMessage) -> Value {
    let is_error = matches!(
        result.status,
        agent_vocab::ToolResultStatus::Error
            | agent_vocab::ToolResultStatus::Interrupted
            | agent_vocab::ToolResultStatus::Crashed
    );
    json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": result.tool_call_id.as_str(),
            "content": result.output,
            "is_error": is_error,
        }],
    })
}

const RESIZED_IMAGE_BYTES_ESTIMATE: usize = 7373;

pub fn approx_tokens_from_byte_count(bytes: usize) -> usize {
    bytes.saturating_add(3) / 4
}

#[cfg(test)]
mod token_estimator_tests {
    use super::*;
    use agent_vocab::{AssistantItem, ImageContent, ImageSource, ToolCallId, ToolResultStatus};

    #[test]
    fn transcript_estimator_uses_serialized_model_visible_bytes() {
        let transcript = vec![
            TranscriptItem::UserMessage(UserMessage::text("hello world")).into(),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![
                    AssistantItem::Text("working".to_string()),
                    AssistantItem::ToolCall(ToolCall {
                        id: ToolCallId::from_u64(1),
                        tool_name: "Bash".to_string(),
                        args_json: r#"{"command":"pwd"}"#.to_string(),
                    }),
                ],
            })
            .into(),
            TranscriptItem::ToolResult(ToolResultMessage {
                tool_call_id: ToolCallId::from_u64(1),
                tool_name: "Bash".to_string(),
                output: "ok".to_string(),
                status: ToolResultStatus::Success,
            })
            .into(),
        ];

        let estimate = estimate_transcript_tokens(&transcript, ProviderKind::OpenAi);

        assert!(estimate.model_visible_bytes > "hello worldworkingok".len());
        assert_eq!(
            estimate.tokens,
            approx_tokens_from_byte_count(estimate.model_visible_bytes)
        );
    }

    #[test]
    fn image_payload_estimator_discounts_base64_bytes_like_codex() {
        let small_text =
            ModelTranscriptEntry::from(TranscriptItem::UserMessage(UserMessage::from_parts(vec![
                ContentBlock::image(ImageContent {
                    mime_type: "image/png".to_string(),
                    source: ImageSource::Base64("a".repeat(16)),
                }),
            ])));
        let large_text =
            ModelTranscriptEntry::from(TranscriptItem::UserMessage(UserMessage::from_parts(vec![
                ContentBlock::image(ImageContent {
                    mime_type: "image/png".to_string(),
                    source: ImageSource::Base64("a".repeat(16_000)),
                }),
            ])));

        let small = estimate_transcript_entry(&small_text, ProviderKind::OpenAi);
        let large = estimate_transcript_entry(&large_text, ProviderKind::OpenAi);

        assert_eq!(small.model_visible_bytes, large.model_visible_bytes);
        assert!(large.model_visible_bytes < 10_000);
    }
}
