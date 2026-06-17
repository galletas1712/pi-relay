use agent_vocab::{AssistantItem, CompactionSummary};
use reqwest::StatusCode;

use crate::{ProviderError, ProviderResult};

pub(crate) async fn response_text(
    response: reqwest::Response,
) -> ProviderResult<(StatusCode, String)> {
    let status = response.status();
    let bytes = response.bytes().await?;
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

pub(crate) fn ensure_success(
    status: StatusCode,
    body: &str,
    error_message: fn(&str) -> String,
) -> ProviderResult<()> {
    if status.is_success() {
        return Ok(());
    }
    Err(ProviderError::Status {
        status: status.as_u16(),
        message: error_message(body),
    })
}

pub(crate) fn response_excerpt(body: &str) -> String {
    const MAX_CHARS: usize = 1200;
    let trimmed = body.trim();
    let mut excerpt = trimmed.chars().take(MAX_CHARS).collect::<String>();
    if trimmed.chars().count() > MAX_CHARS {
        excerpt.push_str("...");
    }
    if excerpt.is_empty() {
        "empty response body".to_string()
    } else {
        excerpt
    }
}

pub(crate) fn push_text_item(items: &mut Vec<AssistantItem>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(AssistantItem::Text(previous)) = items.last_mut() {
        previous.push_str(text);
    } else {
        items.push(AssistantItem::Text(text.to_string()));
    }
}

pub(crate) fn compaction_summary_text(
    summary: &CompactionSummary,
    prompt: &crate::PromptSections,
) -> String {
    match &prompt.stable_prefix {
        Some(pi_prompt) if !pi_prompt.trim().is_empty() => format!(
            "The active PI.md system prompt is included below because it still applies after this compaction.

{}

The conversation history before this point was compacted into this summary:

{}",
            pi_prompt, summary.summary
        ),
        _ => format!(
            "The conversation history before this point was compacted into this summary:

{}",
            summary.summary
        ),
    }
}
