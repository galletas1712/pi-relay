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
    fn truncates_on_char_boundaries() {
        let budget = DEFAULT_MAX_TOOL_OUTPUT_TOKENS * APPROX_CHARS_PER_OUTPUT_TOKEN;
        let head_chars =
            budget * TOOL_OUTPUT_HEAD_RATIO_NUMERATOR / TOOL_OUTPUT_HEAD_RATIO_DENOMINATOR;
        let tail_chars = budget - head_chars;
        let output = format!(
            "{}{}{}",
            "\u{03b1}".repeat(head_chars),
            "\u{03b2}".repeat(5_000),
            "\u{03b3}".repeat(tail_chars)
        );
        let limited = limit_tool_output(output);

        assert!(limited.starts_with(&"\u{03b1}".repeat(head_chars)));
        assert!(limited.contains("5000 characters omitted"));
        assert!(limited.ends_with(&"\u{03b3}".repeat(tail_chars)));
    }

    #[test]
    fn honors_smaller_per_call_token_budget() {
        let limited = limit_tool_output_with_max_tokens("abcdefghi".to_string(), Some(1));

        assert_eq!(
            limited,
            "ab\n\n[tool output truncated: 5 characters omitted]\n\nhi"
        );
    }
}
