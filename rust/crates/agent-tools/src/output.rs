use agent_vocab::{InlineContentBlock, InlineToolResultMessage, ToolCall};

/// Codex-style default budget for tool output returned to the model.
///
/// The tool crate does not carry a provider tokenizer, so the runtime enforces
/// this with a simple local character-budget approximation.
/// TODO: make this 10k cap configurable per session/provider.
pub const DEFAULT_MAX_TOOL_OUTPUT_TOKENS: usize = 10_000;

const APPROX_CHARS_PER_OUTPUT_TOKEN: usize = 4;
const TOOL_OUTPUT_HEAD_RATIO_NUMERATOR: usize = 3;
const TOOL_OUTPUT_HEAD_RATIO_DENOMINATOR: usize = 5;

pub fn limit_tool_output(output: String) -> String {
    limit_tool_output_with_max_tokens(output, None)
}

pub fn limit_tool_output_with_max_tokens(
    output: String,
    max_output_tokens: Option<usize>,
) -> String {
    let max_tokens = max_output_tokens.unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_TOKENS);
    let max_chars = max_tokens.saturating_mul(APPROX_CHARS_PER_OUTPUT_TOKEN);
    limit_tool_output_chars(output, max_chars)
}

pub fn requested_tool_output_limit(call: &ToolCall) -> Option<usize> {
    if !matches!(
        call.tool_name.as_str(),
        "Bash" | "WebSearch" | "web_search" | "WebFetch" | "web_fetch"
    ) {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&call.args_json)
        .ok()?
        .get("max_output_tokens")?
        .as_u64()?
        .try_into()
        .ok()
}

/// Finalize a transient tool result at the daemon execution boundary.
///
/// Image validation/storage is owned by the store's artifact ingestion.
pub fn finalize_tool_result_content(result: &mut InlineToolResultMessage) {
    finalize_tool_result_content_with_max_tokens(result, None);
}

pub fn finalize_tool_result_content_with_max_tokens(
    result: &mut InlineToolResultMessage,
    max_output_tokens: Option<usize>,
) {
    for block in &mut result.content {
        if let InlineContentBlock::Text { text } = block {
            if text.contains('\0') {
                *text = text.replace('\0', "\\x00");
            }
        }
    }
    let max_chars = max_output_tokens
        .unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_TOKENS)
        .saturating_mul(APPROX_CHARS_PER_OUTPUT_TOKEN);
    if is_finalized_content(&result.content, max_chars) {
        return;
    }
    if let [InlineContentBlock::Text { text }] = result.content.as_mut_slice() {
        *text = limit_tool_output_with_max_tokens(std::mem::take(text), max_output_tokens);
        return;
    }
    result.content =
        limit_inline_tool_content(std::mem::take(&mut result.content), max_output_tokens);
}

pub fn limit_inline_tool_content(
    content: Vec<InlineContentBlock>,
    max_output_tokens: Option<usize>,
) -> Vec<InlineContentBlock> {
    let max_tokens = max_output_tokens.unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_TOKENS);
    let max_chars = max_tokens.saturating_mul(APPROX_CHARS_PER_OUTPUT_TOKEN);
    let total = content
        .iter()
        .map(|block| match block {
            InlineContentBlock::Text { text } => text.chars().count(),
            InlineContentBlock::Image { .. } => 0,
        })
        .sum::<usize>();
    if total <= max_chars {
        return content
            .into_iter()
            .filter(|block| match block {
                InlineContentBlock::Text { text } => !text.is_empty(),
                InlineContentBlock::Image { .. } => true,
            })
            .collect();
    }

    let head_chars = max_chars.saturating_mul(TOOL_OUTPUT_HEAD_RATIO_NUMERATOR)
        / TOOL_OUTPUT_HEAD_RATIO_DENOMINATOR;
    let tail_chars = max_chars.saturating_sub(head_chars);
    let tail_start = total.saturating_sub(tail_chars);
    let omitted = total.saturating_sub(max_chars);
    let mut cursor = 0usize;
    let mut inserted_note = false;
    let mut out = Vec::with_capacity(content.len() + 1);
    for block in content {
        let InlineContentBlock::Text { text } = block else {
            out.push(block);
            continue;
        };
        let count = text.chars().count();
        let start = cursor;
        let end = cursor.saturating_add(count);
        cursor = end;

        let head_end = end.min(head_chars);
        if start < head_end {
            let kept = text.chars().take(head_end - start).collect::<String>();
            if !kept.is_empty() {
                out.push(InlineContentBlock::text(kept));
            }
        }

        if start < tail_start && end > head_chars && !inserted_note {
            out.push(InlineContentBlock::text(truncation_note(omitted)));
            inserted_note = true;
        }

        let tail_offset = tail_start.saturating_sub(start).min(count);
        if end > tail_start {
            let kept = text.chars().skip(tail_offset).collect::<String>();
            if !kept.is_empty() {
                out.push(InlineContentBlock::text(kept));
            }
        }
    }
    if !inserted_note {
        out.push(InlineContentBlock::text(truncation_note(omitted)));
    }
    out
}

fn is_finalized_content(content: &[InlineContentBlock], max_chars: usize) -> bool {
    let expected_head =
        max_chars * TOOL_OUTPUT_HEAD_RATIO_NUMERATOR / TOOL_OUTPUT_HEAD_RATIO_DENOMINATOR;
    let expected_tail = max_chars.saturating_sub(expected_head);
    match content {
        [InlineContentBlock::Text { text }] => {
            if max_chars == 0 {
                return parse_truncation_note(text).is_some();
            }
            let Some((head, remainder)) = text.split_once("\n\n[tool output truncated: ") else {
                return false;
            };
            let Some((omitted, tail)) = remainder.split_once(" characters omitted]\n\n") else {
                return false;
            };
            omitted.parse::<usize>().is_ok()
                && head.chars().count() == expected_head
                && tail.chars().count() == expected_tail
        }
        _ => {
            let mut before = 0usize;
            let mut after = 0usize;
            let mut saw_note = false;
            for block in content {
                let InlineContentBlock::Text { text } = block else {
                    continue;
                };
                if parse_truncation_note(text).is_some() {
                    if saw_note {
                        return false;
                    }
                    saw_note = true;
                } else if saw_note {
                    after = after.saturating_add(text.chars().count());
                } else {
                    before = before.saturating_add(text.chars().count());
                }
            }
            saw_note && before == expected_head && after == expected_tail
        }
    }
}

fn truncation_note(omitted: usize) -> String {
    format!("[tool output truncated: {omitted} characters omitted]")
}

fn parse_truncation_note(value: &str) -> Option<usize> {
    value
        .strip_prefix("[tool output truncated: ")?
        .strip_suffix(" characters omitted]")?
        .parse()
        .ok()
}

fn limit_tool_output_chars(output: String, max_chars: usize) -> String {
    let total = output.chars().count();
    if total <= max_chars {
        return output;
    }
    if max_chars == 0 {
        return format!("[tool output truncated: {total} characters omitted]");
    }

    let head_chars = max_chars.saturating_mul(TOOL_OUTPUT_HEAD_RATIO_NUMERATOR)
        / TOOL_OUTPUT_HEAD_RATIO_DENOMINATOR;
    let tail_chars_count = max_chars.saturating_sub(head_chars);
    let head: String = output.chars().take(head_chars).collect();
    let tail_chars: Vec<char> = output.chars().rev().take(tail_chars_count).collect();
    let tail: String = tail_chars.into_iter().rev().collect();
    let omitted = total.saturating_sub(head_chars + tail_chars_count);
    format!("{head}\n\n[tool output truncated: {omitted} characters omitted]\n\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_short_tool_output_alone() {
        assert_eq!(limit_tool_output("hello".to_string()), "hello");
    }

    #[test]
    fn truncates_tool_output_with_head_and_tail() {
        let budget = DEFAULT_MAX_TOOL_OUTPUT_TOKENS * APPROX_CHARS_PER_OUTPUT_TOKEN;
        let head_chars =
            budget * TOOL_OUTPUT_HEAD_RATIO_NUMERATOR / TOOL_OUTPUT_HEAD_RATIO_DENOMINATOR;
        let tail_chars = budget - head_chars;
        let output = format!(
            "{}{}{}",
            "a".repeat(head_chars),
            "b".repeat(5_000),
            "c".repeat(tail_chars)
        );
        let limited = limit_tool_output(output);

        assert!(limited.starts_with(&"a".repeat(head_chars)));
        assert!(limited.contains("[tool output truncated: 5000 characters omitted]"));
        assert!(limited.ends_with(&"c".repeat(tail_chars)));
        assert!(!limited.contains(&"b".repeat(5_000)));
    }

    #[test]
    fn honors_smaller_per_call_token_budget() {
        let limited = limit_tool_output_with_max_tokens("abcdefghi".to_string(), Some(1));

        assert_eq!(
            limited,
            "ab\n\n[tool output truncated: 5 characters omitted]\n\nhi"
        );
    }

    #[test]
    fn explicit_limit_is_owned_only_by_first_party_tool_contracts() {
        let bash = ToolCall {
            id: "call".into(),
            tool_name: "Bash".to_string(),
            args_json: r#"{"command":"true","max_output_tokens":123}"#.to_string(),
        };
        let mut mcp = bash.clone();
        mcp.tool_name = "mcp__fixture__read".to_string();

        assert_eq!(requested_tool_output_limit(&bash), Some(123));
        assert_eq!(requested_tool_output_limit(&mcp), None);
    }

    #[test]
    fn explicit_budget_finalization_is_stable() {
        let mut result = InlineToolResultMessage::success(
            "call",
            "Bash",
            format!("{}{}{}", "h".repeat(12), "x".repeat(5), "t".repeat(8)),
        );

        finalize_tool_result_content_with_max_tokens(&mut result, Some(5));
        let once = result.clone();
        finalize_tool_result_content_with_max_tokens(&mut result, Some(5));

        assert_eq!(result, once);
        assert_eq!(
            result.display_text(),
            format!(
                "{}\n\n[tool output truncated: 5 characters omitted]\n\n{}",
                "h".repeat(12),
                "t".repeat(8)
            )
        );
    }

    #[test]
    fn finalization_is_stable_for_ordinary_text() {
        let mut result = InlineToolResultMessage::success(
            "call",
            "Bash",
            format!(
                "{}{}{}",
                "h".repeat(24_000),
                "x".repeat(5_000),
                "t".repeat(16_000)
            ),
        );

        finalize_tool_result_content(&mut result);
        let once = result.clone();
        finalize_tool_result_content(&mut result);

        assert_eq!(result, once);
        let text = result.display_text();
        assert!(text.starts_with(&"h".repeat(24_000)));
        assert!(text.contains("[tool output truncated: 5000 characters omitted]"));
        assert!(text.ends_with(&"t".repeat(16_000)));
    }

    #[test]
    fn mixed_finalization_keeps_head_tail_images_order_and_is_stable() {
        let first = InlineContentBlock::image("image/png", "first");
        let second = InlineContentBlock::image("image/png", "second");
        let mut result = InlineToolResultMessage::success_content(
            "call",
            "mcp__fixture__mixed",
            vec![
                InlineContentBlock::text("h".repeat(24_000)),
                first.clone(),
                InlineContentBlock::text("x".repeat(5_000)),
                second.clone(),
                InlineContentBlock::text("t".repeat(16_000)),
            ],
        );

        finalize_tool_result_content(&mut result);
        let once = result.clone();
        finalize_tool_result_content(&mut result);

        assert_eq!(result, once);
        assert_eq!(
            result.content,
            vec![
                InlineContentBlock::text("h".repeat(24_000)),
                first,
                InlineContentBlock::text("[tool output truncated: 5000 characters omitted]"),
                second,
                InlineContentBlock::text("t".repeat(16_000)),
            ]
        );
    }
}
