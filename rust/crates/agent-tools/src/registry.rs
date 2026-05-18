use std::collections::BTreeMap;

use agent_vocab::{ProviderKind, ReplayDisplayKind, ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use crate::context::ToolContext;
use crate::error::{ToolError, ToolResult};
use crate::tools::{ApplyPatchTool, BashTool, GrepTool, TextEditorTool, APPLY_PATCH_LARK_GRAMMAR};

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

fn openai_edit_listing() -> ToolListing {
    ToolListing {
        name: "Edit".to_string(),
        kind: ReplayDisplayKind::LocalTool,
        description:
            "Apply a freeform patch to files in the workspace. Emit the raw patch body, not JSON."
                .to_string(),
        input_schema: json!({
            "type": "custom",
            "format": {
                "type": "grammar",
                "syntax": "lark",
                "definition": APPLY_PATCH_LARK_GRAMMAR,
            },
        }),
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
        if provider == ProviderKind::OpenAi && name == "Edit" {
            return openai_edit_listing();
        }
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
    match (provider, name) {
        (ProviderKind::OpenAi, "WebSearch") => ToolListing {
            name: "WebSearch".to_string(),
            kind: ReplayDisplayKind::HostedTool,
            description: "OpenAI-hosted web search. The provider executes the search and returns web_search_call replay items with actions and citations when available.".to_string(),
            input_schema: json!({
                "type": "web_search",
                "search_context_size": "high"
            }),
        },
        (ProviderKind::Claude, "WebSearch") => ToolListing {
            name: "WebSearch".to_string(),
            kind: ReplayDisplayKind::HostedTool,
            description: "Anthropic-hosted web search. The provider executes the server tool; pi-relay does not execute it locally.".to_string(),
            input_schema: json!({
                "type": "web_search_20250305",
                "name": "WebSearch"
            }),
        },
        (ProviderKind::Claude, "WebFetch") => ToolListing {
            name: "WebFetch".to_string(),
            kind: ReplayDisplayKind::HostedTool,
            description: "Anthropic-hosted web fetch. The provider fetches a specific web page and returns citations when available.".to_string(),
            input_schema: json!({
                "type": "web_fetch_20250910",
                "name": "WebFetch",
                "citations": { "enabled": true }
            }),
        },
        _ => panic!("hosted tool {name} is not registered for {provider}"),
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

    #[test]
    fn edit_listing_is_provider_specific() {
        let registry = ToolRegistry::with_builtin_tools();
        let openai_edit = registry
            .listings_for_provider(ProviderKind::OpenAi)
            .into_iter()
            .find(|listing| listing.name == "Edit")
            .expect("OpenAI Edit listing");
        let claude_edit = registry
            .listings_for_provider(ProviderKind::Claude)
            .into_iter()
            .find(|listing| listing.name == "Edit")
            .expect("Claude Edit listing");

        assert_eq!(openai_edit.input_schema["type"], "custom");
        assert_eq!(openai_edit.input_schema["format"]["syntax"], "lark");
        assert_eq!(claude_edit.input_schema["type"], "object");
        assert!(claude_edit.input_schema["properties"]
            .get("command")
            .is_some());
    }

    #[test]
    fn hosted_listings_are_provider_specific() {
        let registry = ToolRegistry::with_builtin_tools();
        let openai_web = registry
            .listings_for_provider(ProviderKind::OpenAi)
            .into_iter()
            .find(|listing| listing.name == "WebSearch")
            .expect("OpenAI WebSearch listing");
        let claude_web = registry
            .listings_for_provider(ProviderKind::Claude)
            .into_iter()
            .find(|listing| listing.name == "WebSearch")
            .expect("Claude WebSearch listing");
        let claude_fetch = registry
            .listings_for_provider(ProviderKind::Claude)
            .into_iter()
            .find(|listing| listing.name == "WebFetch")
            .expect("Claude WebFetch listing");

        assert_eq!(openai_web.input_schema["type"], "web_search");
        assert_eq!(openai_web.input_schema["search_context_size"], "high");
        assert_eq!(claude_web.input_schema["type"], "web_search_20250305");
        assert_eq!(claude_web.input_schema["name"], "WebSearch");
        assert_eq!(claude_fetch.input_schema["type"], "web_fetch_20250910");
        assert_eq!(claude_fetch.input_schema["name"], "WebFetch");
        assert_eq!(claude_fetch.input_schema["citations"]["enabled"], true);
    }
}
