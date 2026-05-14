use agent_vocab::{ProviderKind, ReplayDisplay, ReplayDisplayKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDisplayInput {
    LocalTool,
    HostedTool,
}

#[derive(Debug, Clone, Copy)]
struct ToolDisplaySpec {
    name: &'static str,
    provider: Option<ProviderKind>,
    kind: ToolDisplayInput,
    pretty_name: &'static str,
    summary: ToolSummary,
}

#[derive(Debug, Clone, Copy)]
enum ToolSummary {
    None,
    Field(&'static str),
    ShellCommand,
    Grep,
    TextEditor,
    WebSearchQuery,
}

const TOOL_DISPLAY_SPECS: &[ToolDisplaySpec] = &[
    ToolDisplaySpec {
        name: "apply_patch",
        provider: Some(ProviderKind::OpenAi),
        kind: ToolDisplayInput::LocalTool,
        pretty_name: "Edit",
        summary: ToolSummary::None,
    },
    ToolDisplaySpec {
        name: "bash",
        provider: None,
        kind: ToolDisplayInput::LocalTool,
        pretty_name: "Bash",
        summary: ToolSummary::ShellCommand,
    },
    ToolDisplaySpec {
        name: "grep",
        provider: None,
        kind: ToolDisplayInput::LocalTool,
        pretty_name: "Grep",
        summary: ToolSummary::Grep,
    },
    ToolDisplaySpec {
        name: "open_page",
        provider: Some(ProviderKind::OpenAi),
        kind: ToolDisplayInput::HostedTool,
        pretty_name: "Open page",
        summary: ToolSummary::Field("url"),
    },
    ToolDisplaySpec {
        name: "str_replace_based_edit_tool",
        provider: Some(ProviderKind::Claude),
        kind: ToolDisplayInput::LocalTool,
        pretty_name: "Edit",
        summary: ToolSummary::TextEditor,
    },
    ToolDisplaySpec {
        name: "web_fetch",
        provider: Some(ProviderKind::Claude),
        kind: ToolDisplayInput::HostedTool,
        pretty_name: "Web fetch",
        summary: ToolSummary::Field("url"),
    },
    ToolDisplaySpec {
        name: "web_search",
        provider: None,
        kind: ToolDisplayInput::HostedTool,
        pretty_name: "Web search",
        summary: ToolSummary::WebSearchQuery,
    },
];

pub fn tool_display(
    provider: ProviderKind,
    name: &str,
    kind: ToolDisplayInput,
    input: Option<&serde_json::Value>,
) -> Option<ReplayDisplay> {
    let spec = tool_display_spec(provider, name, kind)?;
    Some(ReplayDisplay {
        kind: match kind {
            ToolDisplayInput::LocalTool => ReplayDisplayKind::LocalTool,
            ToolDisplayInput::HostedTool => ReplayDisplayKind::HostedTool,
        },
        pretty_name: spec.pretty_name.to_string(),
        input_summary: tool_summary(spec.summary, input),
    })
}

pub fn tool_pretty_name(
    provider: ProviderKind,
    name: &str,
    kind: ToolDisplayInput,
) -> Option<&'static str> {
    tool_display_spec(provider, name, kind).map(|spec| spec.pretty_name)
}

fn tool_display_spec(
    provider: ProviderKind,
    name: &str,
    kind: ToolDisplayInput,
) -> Option<&'static ToolDisplaySpec> {
    TOOL_DISPLAY_SPECS.iter().find(|spec| {
        spec.name == name && spec.kind == kind && provider_matches(spec.provider, provider)
    })
}

fn provider_matches(spec_provider: Option<ProviderKind>, provider: ProviderKind) -> bool {
    match spec_provider {
        None => true,
        Some(ProviderKind::OpenAi) => {
            matches!(provider, ProviderKind::OpenAi)
        }
        Some(kind) => kind == provider,
    }
}

fn tool_summary(summary: ToolSummary, input: Option<&serde_json::Value>) -> Option<String> {
    let input = input?;
    let text = match summary {
        ToolSummary::None => return None,
        ToolSummary::Field(key) => string_field(input, key),
        ToolSummary::ShellCommand => shell_command_summary(input),
        ToolSummary::Grep => joined_fields(input, &["pattern", "path"]),
        ToolSummary::TextEditor => joined_fields(input, &["command", "path"]),
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
                ProviderKind::Claude,
                "web_search",
                ToolDisplayInput::HostedTool,
                Some(&json!({ "type": "search", "query": "OpenAI Responses API" })),
            ),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::HostedTool,
                pretty_name: "Web search".to_string(),
                input_summary: Some("OpenAI Responses API".to_string()),
            })
        );
        assert_eq!(
            tool_display(
                ProviderKind::OpenAi,
                "open_page",
                ToolDisplayInput::HostedTool,
                Some(&json!({ "type": "open_page", "url": "https://example.com" })),
            ),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::HostedTool,
                pretty_name: "Open page".to_string(),
                input_summary: Some("https://example.com".to_string()),
            })
        );
        assert_eq!(
            tool_display(
                ProviderKind::Claude,
                "open_page",
                ToolDisplayInput::HostedTool,
                Some(&json!({ "type": "open_page", "url": "https://example.com" })),
            ),
            None
        );
    }

    #[test]
    fn display_registry_labels_local_tools() {
        assert_eq!(
            tool_display(
                ProviderKind::Claude,
                "str_replace_based_edit_tool",
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
                ProviderKind::OpenAi,
                "bash",
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
                ProviderKind::Claude,
                "bash",
                ToolDisplayInput::LocalTool,
                Some(&json!({ "command": "pwd && ls" })),
            ),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::LocalTool,
                pretty_name: "Bash".to_string(),
                input_summary: Some("pwd && ls".to_string()),
            })
        );
    }
}
