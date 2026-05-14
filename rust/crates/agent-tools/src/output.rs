const MAX_TOOL_OUTPUT_CHARS: usize = 24_000;
const TOOL_OUTPUT_HEAD_CHARS: usize = 12_000;
const TOOL_OUTPUT_TAIL_CHARS: usize = 8_000;

pub fn limit_tool_output(output: String) -> String {
    let total = output.chars().count();
    if total <= MAX_TOOL_OUTPUT_CHARS {
        return output;
    }

    let head: String = output.chars().take(TOOL_OUTPUT_HEAD_CHARS).collect();
    let tail_chars: Vec<char> = output.chars().rev().take(TOOL_OUTPUT_TAIL_CHARS).collect();
    let tail: String = tail_chars.into_iter().rev().collect();
    let omitted = total.saturating_sub(TOOL_OUTPUT_HEAD_CHARS + TOOL_OUTPUT_TAIL_CHARS);
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
        let output = format!(
            "{}{}{}",
            "a".repeat(TOOL_OUTPUT_HEAD_CHARS),
            "b".repeat(5_000),
            "c".repeat(TOOL_OUTPUT_TAIL_CHARS)
        );
        let limited = limit_tool_output(output);

        assert!(limited.starts_with(&"a".repeat(TOOL_OUTPUT_HEAD_CHARS)));
        assert!(limited.contains("[tool output truncated: 5000 characters omitted]"));
        assert!(limited.ends_with(&"c".repeat(TOOL_OUTPUT_TAIL_CHARS)));
        assert!(!limited.contains(&"b".repeat(5_000)));
    }

    #[test]
    fn truncates_on_char_boundaries() {
        let output = format!(
            "{}{}{}",
            "\u{03b1}".repeat(TOOL_OUTPUT_HEAD_CHARS),
            "\u{03b2}".repeat(5_000),
            "\u{03b3}".repeat(TOOL_OUTPUT_TAIL_CHARS)
        );
        let limited = limit_tool_output(output);

        assert!(limited.starts_with(&"\u{03b1}".repeat(TOOL_OUTPUT_HEAD_CHARS)));
        assert!(limited.contains("5000 characters omitted"));
        assert!(limited.ends_with(&"\u{03b3}".repeat(TOOL_OUTPUT_TAIL_CHARS)));
    }
}
