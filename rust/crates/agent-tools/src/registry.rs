use std::collections::BTreeMap;

use agent_vocab::{ProviderKind, ReplayDisplayKind, ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use crate::context::ToolContext;
use crate::display::{tool_pretty_name, ToolDisplayInput};
use crate::error::{ToolError, ToolResult};
use crate::tools::{ApplyPatchTool, BashTool, GrepTool, ShellTool, TextEditorTool};

#[async_trait]
pub trait AgentTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolListing {
    pub name: String,
    pub pretty_name: String,
    pub kind: ReplayDisplayKind,
    pub description: String,
    pub input_schema: Value,
}

pub fn builtin_tool_definition(name: &str) -> Option<ToolDefinition> {
    match name {
        "apply_patch" => Some(ApplyPatchTool.definition()),
        "bash" => Some(BashTool.definition()),
        "grep" => Some(GrepTool.definition()),
        "shell" => Some(ShellTool.definition()),
        "str_replace_based_edit_tool" => Some(TextEditorTool.definition()),
        _ => None,
    }
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
}

struct RegisteredTool {
    tool: Box<dyn AgentTool>,
}

const OPENAI_LOCAL_TOOL_NAMES: &[&str] = &["apply_patch", "grep", "shell"];
const CLAUDE_LOCAL_TOOL_NAMES: &[&str] = &["bash", "grep", "str_replace_based_edit_tool"];
const OPENAI_HOSTED_TOOL_NAMES: &[&str] = &["web_search"];
const CLAUDE_HOSTED_TOOL_NAMES: &[&str] = &["web_search", "web_fetch"];

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_builtin_tools() -> Self {
        let mut registry = Self::new();
        registry.register(BashTool);
        registry.register(ShellTool);
        registry.register(GrepTool);
        registry.register(ApplyPatchTool);
        registry.register(TextEditorTool);
        registry
    }

    pub fn register(&mut self, tool: impl AgentTool + 'static) {
        self.tools.insert(
            tool.definition().name,
            RegisteredTool {
                tool: Box::new(tool),
            },
        );
    }

    pub fn definitions_for_provider(&self, provider: ProviderKind) -> Vec<ToolDefinition> {
        self.local_tool_names_for_provider(provider)
            .iter()
            .map(|name| self.definition(name))
            .collect()
    }

    pub fn listings_for_provider(&self, provider: ProviderKind) -> Vec<ToolListing> {
        self.local_tool_names_for_provider(provider)
            .iter()
            .map(|name| self.local_listing(provider, name))
            .chain(
                self.hosted_tool_names_for_provider(provider)
                    .iter()
                    .map(|name| hosted_listing(provider, name)),
            )
            .collect()
    }

    fn local_tool_names_for_provider(&self, provider: ProviderKind) -> &'static [&'static str] {
        match provider {
            ProviderKind::OpenAi => OPENAI_LOCAL_TOOL_NAMES,
            ProviderKind::Claude => CLAUDE_LOCAL_TOOL_NAMES,
        }
    }

    fn hosted_tool_names_for_provider(&self, provider: ProviderKind) -> &'static [&'static str] {
        match provider {
            ProviderKind::OpenAi => OPENAI_HOSTED_TOOL_NAMES,
            ProviderKind::Claude => CLAUDE_HOSTED_TOOL_NAMES,
        }
    }

    fn definition(&self, name: &str) -> ToolDefinition {
        self.tools
            .get(name)
            .unwrap_or_else(|| panic!("registered provider tool {name} is missing"))
            .tool
            .definition()
    }

    fn local_listing(&self, provider: ProviderKind, name: &str) -> ToolListing {
        let definition = self.definition(name);
        let pretty_name = tool_pretty_name(provider, &definition.name, ToolDisplayInput::LocalTool)
            .unwrap_or_else(|| panic!("registered tool {} needs a pretty name", definition.name));
        ToolListing {
            name: definition.name,
            pretty_name: pretty_name.to_string(),
            kind: ReplayDisplayKind::LocalTool,
            description: definition.description,
            input_schema: definition.input_schema,
        }
    }

    pub async fn execute(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> ToolResult<ToolResultMessage> {
        let tool = self
            .tools
            .get(&call.tool_name)
            .map(|registered| registered.tool.as_ref())
            .ok_or_else(|| ToolError::UnknownTool(call.tool_name.clone()))?;
        tool.execute(call, ctx).await
    }
}

fn hosted_listing(provider: ProviderKind, name: &str) -> ToolListing {
    let pretty_name = tool_pretty_name(provider, name, ToolDisplayInput::HostedTool)
        .unwrap_or_else(|| panic!("hosted tool {name} needs a pretty name"));
    ToolListing {
        name: name.to_string(),
        pretty_name: pretty_name.to_string(),
        kind: ReplayDisplayKind::HostedTool,
        description: hosted_tool_description(name).to_string(),
        input_schema: json!({}),
    }
}

fn hosted_tool_description(name: &str) -> &'static str {
    match name {
        "web_search" => "Provider-hosted web search.",
        "web_fetch" => "Provider-hosted web fetch.",
        other => panic!("hosted tool {other} needs a description"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_definitions_expose_current_coding_tools() {
        let registry = ToolRegistry::with_builtin_tools();
        let names = [
            registry.definitions_for_provider(ProviderKind::OpenAi),
            registry.definitions_for_provider(ProviderKind::Claude),
        ]
        .concat()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();

        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"apply_patch".to_string()));
        assert!(names.contains(&"grep".to_string()));
        assert!(names.contains(&"str_replace_based_edit_tool".to_string()));
    }

    #[test]
    fn definitions_for_provider_expose_only_that_provider() {
        let registry = ToolRegistry::with_builtin_tools();
        let openai = registry
            .definitions_for_provider(ProviderKind::OpenAi)
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        let claude = registry
            .definitions_for_provider(ProviderKind::Claude)
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert_eq!(openai, ["apply_patch", "grep", "shell"]);
        assert_eq!(claude, ["bash", "grep", "str_replace_based_edit_tool"]);
    }

    #[test]
    fn listings_for_provider_include_pretty_names() {
        let registry = ToolRegistry::with_builtin_tools();
        let openai = registry
            .listings_for_provider(ProviderKind::OpenAi)
            .into_iter()
            .map(|listing| (listing.name, listing.pretty_name))
            .collect::<Vec<_>>();
        let claude = registry
            .listings_for_provider(ProviderKind::Claude)
            .into_iter()
            .map(|listing| (listing.name, listing.pretty_name))
            .collect::<Vec<_>>();

        assert_eq!(
            openai,
            [
                ("apply_patch".to_string(), "Edit".to_string()),
                ("grep".to_string(), "Grep".to_string()),
                ("shell".to_string(), "Bash".to_string()),
                ("web_search".to_string(), "Web search".to_string()),
            ]
        );
        assert_eq!(
            claude,
            [
                ("bash".to_string(), "Bash".to_string()),
                ("grep".to_string(), "Grep".to_string()),
                (
                    "str_replace_based_edit_tool".to_string(),
                    "Edit".to_string()
                ),
                ("web_search".to_string(), "Web search".to_string()),
                ("web_fetch".to_string(), "Web fetch".to_string()),
            ]
        );
    }
}
