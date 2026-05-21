use agent_vocab::{ReplayDisplay, ReplayDisplayKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDisplayInput {
    LocalTool,
    /// Legacy provider-replay display only. The tool-extension execution API
    /// no longer has a hosted execution mode.
    HostedTool,
}

#[derive(Debug, Clone, Copy)]
struct ToolDisplaySpec {
    name: &'static str,
    kind: ToolDisplayInput,
    display_name: &'static str,
    summary: ToolSummary,
}

#[derive(Debug, Clone, Copy)]
enum ToolSummary {
    Field(&'static str),
    ShellCommand,
    Grep,
    TextEditor,
    SkillName,
    WebSearchQuery,
}

const TOOL_DISPLAY_SPECS: &[ToolDisplaySpec] = &[
    ToolDisplaySpec {
        name: "LoadSkill",
        kind: ToolDisplayInput::LocalTool,
        display_name: "LoadSkill",
        summary: ToolSummary::SkillName,
    },
    ToolDisplaySpec {
        name: "Edit",
        kind: ToolDisplayInput::LocalTool,
        display_name: "Edit",
        summary: ToolSummary::TextEditor,
    },
    ToolDisplaySpec {
        name: "Bash",
        kind: ToolDisplayInput::LocalTool,
        display_name: "Bash",
        summary: ToolSummary::ShellCommand,
    },
    ToolDisplaySpec {
        name: "Grep",
        kind: ToolDisplayInput::LocalTool,
        display_name: "Grep",
        summary: ToolSummary::Grep,
    },
    ToolDisplaySpec {
        name: "WebFetch",
        kind: ToolDisplayInput::LocalTool,
        display_name: "WebFetch",
        summary: ToolSummary::Field("url"),
    },
    ToolDisplaySpec {
        name: "WebSearch",
        kind: ToolDisplayInput::LocalTool,
        display_name: "WebSearch",
        summary: ToolSummary::WebSearchQuery,
    },
    ToolDisplaySpec {
        name: "OpenPage",
        kind: ToolDisplayInput::HostedTool,
        display_name: "OpenPage",
        summary: ToolSummary::Field("url"),
    },
    ToolDisplaySpec {
        name: "WebFetch",
        kind: ToolDisplayInput::HostedTool,
        display_name: "WebFetch",
        summary: ToolSummary::Field("url"),
    },
    ToolDisplaySpec {
        name: "WebSearch",
        kind: ToolDisplayInput::HostedTool,
        display_name: "WebSearch",
        summary: ToolSummary::WebSearchQuery,
    },
];

pub fn tool_display(
    name: &str,
    kind: ToolDisplayInput,
    input: Option<&serde_json::Value>,
) -> Option<ReplayDisplay> {
    let spec = tool_display_spec(name, kind)?;
    Some(ReplayDisplay {
        kind: match kind {
            ToolDisplayInput::LocalTool => ReplayDisplayKind::LocalTool,
            ToolDisplayInput::HostedTool => ReplayDisplayKind::HostedTool,
        },
        pretty_name: spec.display_name.to_string(),
        input_summary: tool_summary(spec.summary, input),
    })
}

fn tool_display_spec(name: &str, kind: ToolDisplayInput) -> Option<&'static ToolDisplaySpec> {
    TOOL_DISPLAY_SPECS
        .iter()
        .find(|spec| spec.name == name && spec.kind == kind)
}

fn tool_summary(summary: ToolSummary, input: Option<&serde_json::Value>) -> Option<String> {
    let input = input?;
    let text = match summary {
        ToolSummary::Field(key) => string_field(input, key),
        ToolSummary::ShellCommand => shell_command_summary(input),
        ToolSummary::Grep => joined_fields(input, &["pattern", "path"]),
        ToolSummary::TextEditor => joined_fields(input, &["command", "path"]),
        ToolSummary::SkillName => string_field(input, "name"),
        ToolSummary::WebSearchQuery => web_search_query(input),
    }?;
    nonempty_summary(first_line(&text).to_string())
}

fn shell_command_summary(input: &serde_json::Value) -> Option<String> {
    let command = input.get("command")?;
    if let Some(text) = command.as_str() {
        return Some(text.to_string());
    }
    command.as_array().map(|argv| {
        argv.iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    })
}

fn joined_fields(input: &serde_json::Value, keys: &[&str]) -> Option<String> {
    nonempty_summary(
        keys.iter()
            .filter_map(|key| string_field(input, key))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn web_search_query(input: &serde_json::Value) -> Option<String> {
    if let Some(query) = string_field(input, "query") {
        return Some(query);
    }
    input
        .get("queries")
        .and_then(serde_json::Value::as_array)
        .map(|queries| {
            queries
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
}

fn string_field(input: &serde_json::Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

fn nonempty_summary(text: String) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn display_registry_labels_hosted_tools() {
        assert_eq!(
            tool_display(
                "WebSearch",
                ToolDisplayInput::HostedTool,
                Some(&json!({ "type": "search", "query": "OpenAI Responses API" })),
            ),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::HostedTool,
                pretty_name: "WebSearch".to_string(),
                input_summary: Some("OpenAI Responses API".to_string()),
            })
        );
        assert_eq!(
            tool_display(
                "OpenPage",
                ToolDisplayInput::HostedTool,
                Some(&json!({ "type": "OpenPage", "url": "https://example.com" })),
            ),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::HostedTool,
                pretty_name: "OpenPage".to_string(),
                input_summary: Some("https://example.com".to_string()),
            })
        );
    }

    #[test]
    fn display_registry_labels_local_tools() {
        assert_eq!(
            tool_display(
                "Edit",
                ToolDisplayInput::LocalTool,
                Some(&json!({ "command": "view", "path": "src/main.rs" })),
            ),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::LocalTool,
                pretty_name: "Edit".to_string(),
                input_summary: Some("view src/main.rs".to_string()),
            })
        );
        assert_eq!(
            tool_display(
                "Bash",
                ToolDisplayInput::LocalTool,
                Some(&json!({ "command": ["pwd"] })),
            ),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::LocalTool,
                pretty_name: "Bash".to_string(),
                input_summary: Some("pwd".to_string()),
            })
        );
        assert_eq!(
            tool_display(
                "Bash",
                ToolDisplayInput::LocalTool,
                Some(&json!({ "command": "pwd && ls" })),
            ),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::LocalTool,
                pretty_name: "Bash".to_string(),
                input_summary: Some("pwd && ls".to_string()),
            })
        );
        assert_eq!(
            tool_display(
                "LoadSkill",
                ToolDisplayInput::LocalTool,
                Some(&json!({ "name": "rust-refactor" })),
            ),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::LocalTool,
                pretty_name: "LoadSkill".to_string(),
                input_summary: Some("rust-refactor".to_string()),
            })
        );
        assert_eq!(
            tool_display(
                "WebSearch",
                ToolDisplayInput::LocalTool,
                Some(&json!({ "query": "OpenAI Responses API" })),
            ),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::LocalTool,
                pretty_name: "WebSearch".to_string(),
                input_summary: Some("OpenAI Responses API".to_string()),
            })
        );
        assert_eq!(
            tool_display(
                "WebFetch",
                ToolDisplayInput::LocalTool,
                Some(&json!({ "url": "https://example.com" })),
            ),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::LocalTool,
                pretty_name: "WebFetch".to_string(),
                input_summary: Some("https://example.com".to_string()),
            })
        );
    }
}
