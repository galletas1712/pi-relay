use std::collections::BTreeMap;

use agent_vocab::{ProviderKind, ReplayDisplayKind, ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use crate::context::ToolContext;
use crate::error::{ToolError, ToolResult};
use crate::tools::{ApplyPatchTool, BashTool, GrepTool, TextEditorTool};

#[async_trait]
pub trait AgentTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolListing {
    pub name: String,
    pub kind: ReplayDisplayKind,
    pub description: String,
    pub input_schema: Value,
}

pub fn builtin_tool_definition(name: &str) -> Option<ToolDefinition> {
    match name {
        "Edit" => Some(ApplyPatchTool.definition()),
        "Bash" => Some(BashTool.definition()),
        "Grep" => Some(GrepTool.definition()),
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

const OPENAI_LOCAL_TOOL_NAMES: &[&str] = &["Edit", "Bash", "Grep"];
const CLAUDE_LOCAL_TOOL_NAMES: &[&str] = &["Bash", "Grep", "Edit"];
const OPENAI_HOSTED_TOOL_NAMES: &[&str] = &["WebSearch"];
const CLAUDE_HOSTED_TOOL_NAMES: &[&str] = &["WebSearch", "WebFetch"];

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_builtin_tools() -> Self {
        let mut registry = Self::new();
        registry.register_for_provider(ProviderKind::OpenAi, ApplyPatchTool);
        registry.register_for_provider(ProviderKind::OpenAi, BashTool);
        registry.register_for_provider(ProviderKind::OpenAi, GrepTool);
        registry.register_for_provider(ProviderKind::Claude, BashTool);
        registry.register_for_provider(ProviderKind::Claude, GrepTool);
        registry.register_for_provider(ProviderKind::Claude, TextEditorTool);
        registry
    }

    pub fn register_for_provider(
        &mut self,
        provider: ProviderKind,
        tool: impl AgentTool + 'static,
    ) {
        let definition = tool.definition();
        self.tools.insert(
            provider_tool_key(provider, &definition.name),
            RegisteredTool {
                tool: Box::new(tool),
            },
        );
    }

    pub fn definitions_for_provider(&self, provider: ProviderKind) -> Vec<ToolDefinition> {
        self.local_tool_names_for_provider(provider)
            .iter()
            .map(|name| self.definition(provider, name))
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

    fn definition(&self, provider: ProviderKind, name: &str) -> ToolDefinition {
        self.tools
            .get(&provider_tool_key(provider, name))
            .unwrap_or_else(|| panic!("registered provider tool {name} is missing for {provider}"))
            .tool
            .definition()
    }

    fn local_listing(&self, provider: ProviderKind, name: &str) -> ToolListing {
        let definition = self.definition(provider, name);
        ToolListing {
            name: definition.name,
            kind: ReplayDisplayKind::LocalTool,
            description: definition.description,
            input_schema: definition.input_schema,
        }
    }

    pub async fn execute(
        &self,
        provider: ProviderKind,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> ToolResult<ToolResultMessage> {
        let tool = self
            .tools
            .get(&provider_tool_key(provider, &call.tool_name))
            .map(|registered| registered.tool.as_ref())
            .ok_or_else(|| ToolError::UnknownTool(call.tool_name.clone()))?;
        tool.execute(call, ctx).await
    }
}

fn provider_tool_key(provider: ProviderKind, name: &str) -> String {
    format!("{}:{name}", provider.as_str())
}

fn hosted_listing(provider: ProviderKind, name: &str) -> ToolListing {
    let _ = provider;
    ToolListing {
        name: name.to_string(),
        kind: ReplayDisplayKind::HostedTool,
        description: hosted_tool_description(name).to_string(),
        input_schema: hosted_tool_schema(name),
    }
}

fn hosted_tool_description(name: &str) -> &'static str {
    match name {
        "WebSearch" => "Provider-hosted web search.",
        "WebFetch" => "Provider-hosted web fetch.",
        other => panic!("hosted tool {other} needs a description"),
    }
}

fn hosted_tool_schema(name: &str) -> Value {
    match name {
        "WebSearch" => json!({ "type": "provider_hosted", "description": "Search the web." }),
        "WebFetch" => {
            json!({ "type": "provider_hosted", "description": "Fetch a specific web page." })
        }
        other => panic!("hosted tool {other} needs a schema"),
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

        assert!(names.contains(&"Bash".to_string()));
        assert!(names.contains(&"Edit".to_string()));
        assert!(names.contains(&"Grep".to_string()));
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

        assert_eq!(openai, ["Edit", "Bash", "Grep"]);
        assert_eq!(claude, ["Bash", "Grep", "Edit"]);
    }

    #[test]
    fn listings_for_provider_use_model_facing_names() {
        let registry = ToolRegistry::with_builtin_tools();
        let openai = registry
            .listings_for_provider(ProviderKind::OpenAi)
            .into_iter()
            .map(|listing| listing.name)
            .collect::<Vec<_>>();
        let claude = registry
            .listings_for_provider(ProviderKind::Claude)
            .into_iter()
            .map(|listing| listing.name)
            .collect::<Vec<_>>();

        assert_eq!(openai, ["Edit", "Bash", "Grep", "WebSearch"]);
        assert_eq!(claude, ["Bash", "Grep", "Edit", "WebSearch", "WebFetch"]);
    }
}
