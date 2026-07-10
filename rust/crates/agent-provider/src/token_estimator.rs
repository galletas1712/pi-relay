use serde::Serialize;

use agent_tools::ProviderTool;

use crate::{ModelTranscriptEntry, PromptSections, ProviderResult};

// The local token estimator only ever runs for OpenAI: Claude sessions count
// tokens against the authoritative remote `count_tokens` endpoint (see
// `context_accounting.rs`). So the estimate reuses the OpenAI adapter's actual
// wire rendering (`openai::transcript_to_response_items`) and approximates
// tokens as ceil(model-visible bytes / 4), with a discount for base64 images.

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
) -> ProviderResult<usize> {
    estimate_model_input_tokens_with_tools(prompt, transcript, &[])
}

pub fn estimate_model_input_tokens_with_tools(
    prompt: &PromptSections,
    transcript: &[ModelTranscriptEntry],
    tools: &[ProviderTool],
) -> ProviderResult<usize> {
    Ok(estimate_model_input_with_tools(prompt, transcript, tools)?.tokens)
}

pub fn estimate_model_input(
    prompt: &PromptSections,
    transcript: &[ModelTranscriptEntry],
) -> ProviderResult<TokenEstimate> {
    estimate_model_input_with_tools(prompt, transcript, &[])
}

pub fn estimate_model_input_with_tools(
    prompt: &PromptSections,
    transcript: &[ModelTranscriptEntry],
    tools: &[ProviderTool],
) -> ProviderResult<TokenEstimate> {
    let tools = if tools.is_empty() {
        TokenEstimate::from_model_visible_bytes(0)
    } else {
        serialized_estimate(
            &tools
                .iter()
                .map(|tool| &tool.declaration)
                .collect::<Vec<_>>(),
        )?
    };
    Ok(prompt_estimate(prompt)?
        .saturating_add(estimate_transcript_tokens(prompt, transcript)?)
        .saturating_add(tools))
}

pub fn estimate_transcript_tokens(
    prompt: &PromptSections,
    transcript: &[ModelTranscriptEntry],
) -> ProviderResult<TokenEstimate> {
    crate::openai::transcript_to_response_items(prompt, transcript)?
        .iter()
        .map(serialized_estimate)
        .try_fold(
            TokenEstimate::from_model_visible_bytes(0),
            |total, estimate| Ok(total.saturating_add(estimate?)),
        )
}

fn prompt_estimate(prompt: &PromptSections) -> ProviderResult<TokenEstimate> {
    prompt
        .render_joined()
        .map(|text| serialized_estimate(&text))
        .unwrap_or_else(|| Ok(TokenEstimate::from_model_visible_bytes(0)))
}

fn serialized_estimate<T: Serialize>(value: &T) -> ProviderResult<TokenEstimate> {
    let serialized = serde_json::to_string(value)?;
    Ok(TokenEstimate::from_model_visible_bytes(
        model_visible_bytes_for_serialized_json(&serialized),
    ))
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

const RESIZED_IMAGE_BYTES_ESTIMATE: usize = 7373;

pub fn approx_tokens_from_byte_count(bytes: usize) -> usize {
    bytes.saturating_add(3) / 4
}

#[cfg(test)]
mod token_estimator_tests {
    use super::*;
    use agent_vocab::{
        AssistantItem, AssistantMessage, ContentBlock, ImageContent, ImageSource, ToolCall,
        ToolCallId, ToolResultMessage, ToolResultStatus, TranscriptItem, UserMessage,
    };
    use serde_json::json;

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

        let estimate = estimate_transcript_tokens(&PromptSections::default(), &transcript).unwrap();

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

        let small = estimate_transcript_tokens(
            &PromptSections::default(),
            std::slice::from_ref(&small_text),
        )
        .unwrap();
        let large = estimate_transcript_tokens(
            &PromptSections::default(),
            std::slice::from_ref(&large_text),
        )
        .unwrap();

        assert_eq!(small.model_visible_bytes, large.model_visible_bytes);
        assert!(large.model_visible_bytes < 10_000);
    }

    #[test]
    fn compaction_summary_estimate_uses_native_replay() {
        let summary = agent_vocab::CompactionSummary::new(
            "session-1",
            "leaf-1",
            "short recap",
            Some(1024),
            agent_vocab::TurnId(1),
        );
        let raw_replay = json!({
            "type": "compaction",
            "encrypted_content": "opaque compacted context",
        });
        let replay =
            agent_vocab::ProviderReplayItem::new(agent_vocab::ProviderKind::OpenAi, &raw_replay)
                .unwrap();
        let transcript = vec![ModelTranscriptEntry {
            item: TranscriptItem::CompactionSummary(summary),
            provider_replay: vec![replay],
        }];

        let estimate = estimate_transcript_tokens(&PromptSections::default(), &transcript).unwrap();
        let expected_bytes = serde_json::to_string(&raw_replay).unwrap().len();
        assert_eq!(
            estimate,
            TokenEstimate::from_model_visible_bytes(expected_bytes)
        );
    }

    #[test]
    fn transcript_estimator_propagates_invalid_replay() {
        let transcript = vec![ModelTranscriptEntry {
            item: TranscriptItem::CompactionSummary(agent_vocab::CompactionSummary::new(
                "session-1",
                "leaf-1",
                "short recap",
                Some(1024),
                agent_vocab::TurnId(1),
            )),
            provider_replay: vec![agent_vocab::ProviderReplayItem {
                provider: agent_vocab::ProviderKind::OpenAi,
                raw_json: "{".to_string(),
                display: None,
            }],
        }];

        assert!(estimate_transcript_tokens(&PromptSections::default(), &transcript).is_err());
    }

    #[test]
    fn model_input_estimator_includes_exact_provider_declarations() {
        let prompt = PromptSections::stable("stable");
        let transcript = vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()];
        let tool = agent_tools::ProviderTool::function_json_named(
            agent_vocab::ProviderKind::OpenAi,
            "mcp__server__read",
            "Read a value",
            json!({"type":"object","properties":{"key":{"type":"string"}}}),
        );

        let without = estimate_model_input_with_tools(&prompt, &transcript, &[]).unwrap();
        let with = estimate_model_input_with_tools(&prompt, &transcript, &[tool]).unwrap();

        assert!(with.model_visible_bytes > without.model_visible_bytes);
        assert!(with.tokens > without.tokens);
    }
}
