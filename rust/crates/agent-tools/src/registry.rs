use std::collections::BTreeMap;

use agent_vocab::{ProviderKind, ToolCall, ToolDefinition, ToolResultMessage};
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

/// Where/how a provider tool executes.
///
/// The raw provider declaration carries provider-specific shape. This enum only
/// captures the execution boundary and local-call payload shape pi-relay needs
/// to round-trip calls/results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecution {
    LocalJson,
    LocalFreeformText,
    Hosted,
}

impl ToolExecution {
    pub fn is_local(self) -> bool {
        matches!(self, Self::LocalJson | Self::LocalFreeformText)
    }

    pub fn is_hosted(self) -> bool {
        matches!(self, Self::Hosted)
    }
}

/// One provider-facing form of a canonical pi-relay tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderTool {
    /// Stable pi-relay-internal name used for execution and transcript state.
    pub canonical_name: String,
    /// Optional semantic alias key used by PI.md, e.g. `edit` or `shell`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_alias: Option<String>,
    /// Provider/model-facing tool name to show in PI.md and tools.list.
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Exact JSON object sent to the provider.
    pub declaration: Value,
    pub execution: ToolExecution,
}

impl ProviderTool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        declaration: Value,
        execution: ToolExecution,
    ) -> Self {
        Self {
            canonical_name: String::new(),
            prompt_alias: None,
            name: name.into(),
            description: description.into(),
            input_schema,
            declaration,
            execution,
        }
    }

    pub fn openai_function(definition: &ToolDefinition) -> Self {
        Self::new(
            definition.name.clone(),
            definition.description.clone(),
            definition.input_schema.clone(),
            json!({
                "type": "function",
                "name": definition.name,
                "description": definition.description,
                "parameters": definition.input_schema,
            }),
            ToolExecution::LocalJson,
        )
    }

    pub fn anthropic_client(definition: &ToolDefinition) -> Self {
        Self::new(
            definition.name.clone(),
            definition.description.clone(),
            definition.input_schema.clone(),
            json!({
                "name": definition.name,
                "description": definition.description,
                "input_schema": definition.input_schema,
            }),
            ToolExecution::LocalJson,
        )
    }

    pub fn function_json_named(
        provider: ProviderKind,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        let definition = ToolDefinition::new(name, description, input_schema);
        match provider {
            ProviderKind::OpenAi => Self::openai_function(&definition),
            ProviderKind::Claude => Self::anthropic_client(&definition),
        }
    }

    pub fn hosted(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        declaration: Value,
    ) -> Self {
        Self::new(
            name,
            description,
            input_schema,
            declaration,
            ToolExecution::Hosted,
        )
    }
}

/// Canonical tool plus its provider-specific forms and optional executors.
pub struct ToolDescriptor {
    canonical_name: String,
    prompt_alias: Option<String>,
    provider_tools: Vec<(ProviderKind, ProviderTool)>,
    executors: Vec<(ProviderKind, Box<dyn AgentTool>)>,
}

impl ToolDescriptor {
    pub fn new(canonical_name: impl Into<String>) -> Self {
        Self {
            canonical_name: canonical_name.into(),
            prompt_alias: None,
            provider_tools: Vec::new(),
            executors: Vec::new(),
        }
    }

    pub fn prompt_alias(mut self, alias: impl Into<String>) -> Self {
        self.prompt_alias = Some(alias.into());
        self
    }

    pub fn provider(mut self, provider: ProviderKind, provider_tool: ProviderTool) -> Self {
        self.provider_tools.push((provider, provider_tool));
        self
    }

    pub fn executor(mut self, provider: ProviderKind, tool: impl AgentTool + 'static) -> Self {
        self.executors.push((provider, Box::new(tool)));
        self
    }
}

/// A linked-in extension can declare new tools without changing provider code.
pub trait ToolExtension: Send + Sync {
    fn id(&self) -> &'static str;
    fn register(&self, registry: &mut ToolRegistry);
}

#[derive(Default)]
pub struct ToolRegistry {
    provider_tools: BTreeMap<String, RegisteredProviderTool>,
    aliases: BTreeMap<String, String>,
    tools: BTreeMap<String, RegisteredTool>,
    extensions: BTreeMap<&'static str, ()>,
}

#[derive(Clone)]
struct RegisteredProviderTool {
    provider: ProviderKind,
    tool: ProviderTool,
}

struct RegisteredTool {
    tool: Box<dyn AgentTool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_builtin_tools() -> Self {
        let mut registry = Self::new();
        registry.register_extension(&FirstPartyToolExtension);
        registry
    }

    pub fn register_extension(&mut self, extension: &dyn ToolExtension) {
        if self.extensions.insert(extension.id(), ()).is_none() {
            extension.register(self);
        }
    }

    pub fn register_tool(&mut self, descriptor: ToolDescriptor) {
        let canonical_name = descriptor.canonical_name;
        for (provider, tool) in descriptor.executors {
            self.tools.insert(
                provider_tool_key(provider, &canonical_name),
                RegisteredTool { tool },
            );
        }

        for (provider, mut provider_tool) in descriptor.provider_tools {
            provider_tool.canonical_name = canonical_name.clone();
            provider_tool.prompt_alias = descriptor.prompt_alias.clone();
            self.aliases.insert(
                provider_tool_key(provider, &provider_tool.canonical_name),
                provider_tool.canonical_name.clone(),
            );
            self.aliases.insert(
                provider_tool_key(provider, &provider_tool.name),
                provider_tool.canonical_name.clone(),
            );
            self.provider_tools.insert(
                provider_tool_key(provider, &provider_tool.canonical_name),
                RegisteredProviderTool {
                    provider,
                    tool: provider_tool,
                },
            );
        }
    }

    /// Back-compat registration hook for simple JSON tools.
    pub fn register_for_provider(
        &mut self,
        provider: ProviderKind,
        tool: impl AgentTool + 'static,
    ) {
        let definition = tool.definition();
        let name = definition.name.clone();
        let provider_tool = ProviderTool::function_json_named(
            provider,
            definition.name,
            definition.description,
            definition.input_schema,
        );
        self.register_tool(
            ToolDescriptor::new(name)
                .provider(provider, provider_tool)
                .executor(provider, tool),
        );
    }

    pub fn provider_tools_for_provider(&self, provider: ProviderKind) -> Vec<ProviderTool> {
        let mut tools = self
            .provider_tools
            .values()
            .filter(|registered| registered.provider == provider)
            .map(|registered| registered.tool.clone())
            .collect::<Vec<_>>();
        sort_tools_by_name(&mut tools);
        tools
    }

    pub fn definitions_for_provider(&self, provider: ProviderKind) -> Vec<ToolDefinition> {
        self.provider_tools_for_provider(provider)
            .into_iter()
            .filter(|tool| tool.execution.is_local())
            .map(|tool| {
                ToolDefinition::new(tool.canonical_name, tool.description, tool.input_schema)
            })
            .collect()
    }

    pub fn canonical_tool_name_for_provider<'a>(
        &'a self,
        provider: ProviderKind,
        name: &'a str,
    ) -> &'a str {
        self.aliases
            .get(&provider_tool_key(provider, name))
            .map(String::as_str)
            .unwrap_or(name)
    }

    pub async fn execute(
        &self,
        provider: ProviderKind,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> ToolResult<ToolResultMessage> {
        let canonical_name = self.canonical_tool_name_for_provider(provider, &call.tool_name);
        let tool = self
            .tools
            .get(&provider_tool_key(provider, canonical_name))
            .map(|registered| registered.tool.as_ref())
            .ok_or_else(|| ToolError::UnknownTool(call.tool_name.clone()))?;
        if canonical_name == call.tool_name {
            tool.execute(call, ctx).await
        } else {
            let mut canonical_call = call.clone();
            canonical_call.tool_name = canonical_name.to_string();
            tool.execute(&canonical_call, ctx).await
        }
    }
}

fn provider_tool_key(provider: ProviderKind, name: &str) -> String {
    format!("{}:{name}", provider.as_str())
}

pub(crate) fn sort_tools_by_name(tools: &mut [ProviderTool]) {
    tools.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.canonical_name.cmp(&right.canonical_name))
    });
}

pub fn builtin_tool_definition(name: &str) -> Option<ToolDefinition> {
    match name {
        "LoadSkill" => Some(load_skill_definition()),
        "Edit" | "apply_patch" => Some(ApplyPatchTool.definition()),
        "Bash" => Some(BashTool.definition()),
        "Grep" => Some(GrepTool.definition()),
        _ => None,
    }
}

fn load_skill_definition() -> ToolDefinition {
    ToolDefinition::new(
        "LoadSkill",
        "Activate one of the available skills by name. Use this when a task matches a skill description; pi-relay will inject that skill's instructions into the model context. If the skill is already loaded, the tool reports that it is already loaded.",
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The exact skill name from skills.index."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    )
}

pub struct FirstPartyToolExtension;

impl ToolExtension for FirstPartyToolExtension {
    fn id(&self) -> &'static str {
        "pi.first_party_tools"
    }

    fn register(&self, registry: &mut ToolRegistry) {
        register_load_skill(registry);
        register_edit(registry);
        register_bash(registry);
        register_grep(registry);
        register_web_search(registry);
        register_web_fetch(registry);
    }
}

fn register_load_skill(registry: &mut ToolRegistry) {
    let definition = load_skill_definition();
    registry.register_tool(
        ToolDescriptor::new("LoadSkill")
            .prompt_alias("skill_loader")
            .provider(
                ProviderKind::OpenAi,
                ProviderTool::openai_function(&definition),
            )
            .provider(
                ProviderKind::Claude,
                ProviderTool::anthropic_client(&definition),
            ),
    );
}

fn register_edit(registry: &mut ToolRegistry) {
    let claude_definition = TextEditorTool.definition();
    registry.register_tool(
        ToolDescriptor::new("Edit")
            .prompt_alias("edit")
            .provider(ProviderKind::OpenAi, openai_apply_patch_tool())
            .provider(
                ProviderKind::Claude,
                ProviderTool::new(
                    "Edit",
                    claude_definition.description,
                    claude_definition.input_schema,
                    json!({
                        "type": "text_editor_20250728",
                        "name": "Edit",
                    }),
                    ToolExecution::LocalJson,
                ),
            )
            .executor(ProviderKind::OpenAi, ApplyPatchTool)
            .executor(ProviderKind::Claude, TextEditorTool),
    );
}

fn openai_apply_patch_tool() -> ProviderTool {
    let input_schema = json!({
        "type": "custom",
        "format": {
            "type": "grammar",
            "syntax": "lark",
            "definition": APPLY_PATCH_LARK_GRAMMAR,
        },
    });
    ProviderTool::new(
        "apply_patch",
        "Apply a freeform patch to files in the workspace. Emit the raw patch body, not JSON.",
        input_schema,
        json!({
            "type": "custom",
            "name": "apply_patch",
            "description": "Use the `apply_patch` tool to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.",
            "format": {
                "type": "grammar",
                "syntax": "lark",
                "definition": APPLY_PATCH_LARK_GRAMMAR,
            },
        }),
        ToolExecution::LocalFreeformText,
    )
}

fn register_bash(registry: &mut ToolRegistry) {
    let definition = BashTool.definition();
    registry.register_tool(
        ToolDescriptor::new("Bash")
            .prompt_alias("shell")
            .provider(
                ProviderKind::OpenAi,
                ProviderTool::openai_function(&definition),
            )
            .provider(
                ProviderKind::Claude,
                ProviderTool::anthropic_client(&definition),
            )
            .executor(ProviderKind::OpenAi, BashTool)
            .executor(ProviderKind::Claude, BashTool),
    );
}

fn register_grep(registry: &mut ToolRegistry) {
    let definition = GrepTool.definition();
    registry.register_tool(
        ToolDescriptor::new("Grep")
            .prompt_alias("workspace_search")
            .provider(
                ProviderKind::OpenAi,
                ProviderTool::openai_function(&definition),
            )
            .provider(
                ProviderKind::Claude,
                ProviderTool::anthropic_client(&definition),
            )
            .executor(ProviderKind::OpenAi, GrepTool)
            .executor(ProviderKind::Claude, GrepTool),
    );
}

fn register_web_search(registry: &mut ToolRegistry) {
    registry.register_tool(
        ToolDescriptor::new("WebSearch")
            .prompt_alias("web_search")
            .provider(
                ProviderKind::OpenAi,
                ProviderTool::hosted(
                    "web_search",
                    "OpenAI-hosted web search. The provider executes the search and returns web_search_call replay items with actions and citations when available.",
                    json!({
                        "type": "web_search",
                        "search_context_size": "high",
                    }),
                    json!({
                        "type": "web_search",
                        "search_context_size": "high",
                    }),
                ),
            )
            .provider(
                ProviderKind::Claude,
                ProviderTool::hosted(
                    "web_search",
                    "Anthropic-hosted web search. The provider executes the server tool; pi-relay does not execute it locally.",
                    json!({
                        "type": "web_search_20250305",
                        "name": "web_search",
                    }),
                    json!({
                        "type": "web_search_20250305",
                        "name": "web_search",
                    }),
                ),
            ),
    );
}

fn register_web_fetch(registry: &mut ToolRegistry) {
    registry.register_tool(
        ToolDescriptor::new("WebFetch")
            .prompt_alias("web_fetch")
            .provider(
                ProviderKind::Claude,
                ProviderTool::hosted(
                    "web_fetch",
                    "Anthropic-hosted web fetch. The provider fetches a specific web page and returns citations when available.",
                    json!({
                        "type": "web_fetch_20250910",
                        "name": "web_fetch",
                        "citations": { "enabled": true },
                    }),
                    json!({
                        "type": "web_fetch_20250910",
                        "name": "web_fetch",
                        "citations": { "enabled": true },
                    }),
                ),
            ),
    );
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

        assert_eq!(openai, ["Edit", "Bash", "Grep", "LoadSkill"]);
        assert_eq!(claude, ["Bash", "Edit", "Grep", "LoadSkill"]);
    }

    #[test]
    fn provider_tools_use_provider_facing_names() {
        let registry = ToolRegistry::with_builtin_tools();
        let openai = registry
            .provider_tools_for_provider(ProviderKind::OpenAi)
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        let claude = registry
            .provider_tools_for_provider(ProviderKind::Claude)
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();

        assert_eq!(
            openai,
            ["apply_patch", "Bash", "Grep", "LoadSkill", "web_search"]
        );
        assert_eq!(
            claude,
            [
                "Bash",
                "Edit",
                "Grep",
                "LoadSkill",
                "web_fetch",
                "web_search"
            ]
        );
    }

    #[test]
    fn edit_tool_is_provider_specific() {
        let registry = ToolRegistry::with_builtin_tools();
        let openai_edit = registry
            .provider_tools_for_provider(ProviderKind::OpenAi)
            .into_iter()
            .find(|tool| tool.canonical_name == "Edit")
            .expect("OpenAI Edit tool");
        let claude_edit = registry
            .provider_tools_for_provider(ProviderKind::Claude)
            .into_iter()
            .find(|tool| tool.canonical_name == "Edit")
            .expect("Claude Edit tool");

        assert_eq!(openai_edit.name, "apply_patch");
        assert_eq!(openai_edit.execution, ToolExecution::LocalFreeformText);
        assert_eq!(openai_edit.input_schema["type"], "custom");
        assert_eq!(openai_edit.input_schema["format"]["syntax"], "lark");
        assert_eq!(claude_edit.name, "Edit");
        assert_eq!(claude_edit.execution, ToolExecution::LocalJson);
        assert_eq!(claude_edit.input_schema["type"], "object");
        assert!(claude_edit.input_schema["properties"]
            .get("command")
            .is_some());
    }

    #[test]
    fn hosted_tools_are_provider_specific() {
        let registry = ToolRegistry::with_builtin_tools();
        let openai_web = registry
            .provider_tools_for_provider(ProviderKind::OpenAi)
            .into_iter()
            .find(|tool| tool.canonical_name == "WebSearch")
            .expect("OpenAI WebSearch tool");
        let claude_web = registry
            .provider_tools_for_provider(ProviderKind::Claude)
            .into_iter()
            .find(|tool| tool.canonical_name == "WebSearch")
            .expect("Claude WebSearch tool");
        let claude_fetch = registry
            .provider_tools_for_provider(ProviderKind::Claude)
            .into_iter()
            .find(|tool| tool.canonical_name == "WebFetch")
            .expect("Claude WebFetch tool");

        assert_eq!(openai_web.name, "web_search");
        assert_eq!(openai_web.execution, ToolExecution::Hosted);
        assert_eq!(openai_web.input_schema["type"], "web_search");
        assert_eq!(openai_web.input_schema["search_context_size"], "high");
        assert_eq!(claude_web.name, "web_search");
        assert_eq!(claude_web.execution, ToolExecution::Hosted);
        assert_eq!(claude_web.input_schema["type"], "web_search_20250305");
        assert_eq!(claude_web.input_schema["name"], "web_search");
        assert_eq!(claude_fetch.name, "web_fetch");
        assert_eq!(claude_fetch.execution, ToolExecution::Hosted);
        assert_eq!(claude_fetch.input_schema["type"], "web_fetch_20250910");
        assert_eq!(claude_fetch.input_schema["name"], "web_fetch");
        assert_eq!(claude_fetch.input_schema["citations"]["enabled"], true);
    }

    #[test]
    fn provider_tools_carry_raw_declarations() {
        let registry = ToolRegistry::with_builtin_tools();
        let openai_edit = registry
            .provider_tools_for_provider(ProviderKind::OpenAi)
            .into_iter()
            .find(|tool| tool.canonical_name == "Edit")
            .expect("OpenAI Edit binding");
        let claude_edit = registry
            .provider_tools_for_provider(ProviderKind::Claude)
            .into_iter()
            .find(|tool| tool.canonical_name == "Edit")
            .expect("Claude Edit binding");

        assert_eq!(openai_edit.name, "apply_patch");
        assert_eq!(openai_edit.declaration["name"], "apply_patch");
        assert_eq!(openai_edit.execution, ToolExecution::LocalFreeformText);
        assert_eq!(claude_edit.name, "Edit");
        assert_eq!(claude_edit.declaration["type"], "text_editor_20250728");
    }

    #[test]
    fn canonicalizes_provider_wire_aliases_for_execution() {
        let registry = ToolRegistry::with_builtin_tools();
        assert_eq!(
            registry.canonical_tool_name_for_provider(ProviderKind::OpenAi, "apply_patch"),
            "Edit"
        );
        assert_eq!(
            registry.canonical_tool_name_for_provider(ProviderKind::Claude, "web_fetch"),
            "WebFetch"
        );
    }
}
