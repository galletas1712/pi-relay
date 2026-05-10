#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub timeout: Duration,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments: {0}")]
    InvalidArguments(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("command timed out")]
    Timeout,
    #[error("edit target text was not found")]
    EditTargetNotFound,
}

pub type ToolResult<T> = Result<T, ToolError>;

#[async_trait]
pub trait AgentTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Box<dyn AgentTool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_builtin_tools() -> Self {
        let mut registry = Self::new();
        registry.register(ReadTool);
        registry.register(WriteTool);
        registry.register(EditTool);
        registry.register(BashTool);
        registry
    }

    pub fn register(&mut self, tool: impl AgentTool + 'static) {
        self.tools.insert(tool.definition().name, Box::new(tool));
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub async fn execute(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> ToolResult<ToolResultMessage> {
        let tool = self
            .tools
            .get(&call.tool_name)
            .ok_or_else(|| ToolError::UnknownTool(call.tool_name.clone()))?;
        tool.execute(call, ctx).await
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReadTool;

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
}

#[async_trait]
impl AgentTool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".to_string(),
            description: "Read a UTF-8 text file from the workspace.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage> {
        let args: ReadArgs = serde_json::from_str(&call.args_json)?;
        let path = ctx.cwd.join(args.path);
        let output = tokio::fs::read_to_string(path).await?;
        Ok(ToolResultMessage::success(
            call.id.clone(),
            &call.tool_name,
            output,
        ))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WriteTool;

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl AgentTool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".to_string(),
            description: "Write UTF-8 text to a workspace file, creating parent directories."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage> {
        let args: WriteArgs = serde_json::from_str(&call.args_json)?;
        let path = ctx.cwd.join(args.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, args.content).await?;
        Ok(ToolResultMessage::success(
            call.id.clone(),
            &call.tool_name,
            "written",
        ))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EditTool;

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    old: String,
    new: String,
}

#[async_trait]
impl AgentTool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".to_string(),
            description: "Replace exact UTF-8 text in a workspace file.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old": { "type": "string" },
                    "new": { "type": "string" }
                },
                "required": ["path", "old", "new"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage> {
        let args: EditArgs = serde_json::from_str(&call.args_json)?;
        let path = ctx.cwd.join(args.path);
        let original = tokio::fs::read_to_string(&path).await?;
        if !original.contains(&args.old) {
            return Err(ToolError::EditTargetNotFound);
        }
        let edited = original.replacen(&args.old, &args.new, 1);
        tokio::fs::write(path, edited).await?;
        Ok(ToolResultMessage::success(
            call.id.clone(),
            &call.tool_name,
            "edited",
        ))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BashTool;

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
}

#[async_trait]
impl AgentTool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description: "Run a shell command in the workspace and return stdout/stderr."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage> {
        let args: BashArgs = serde_json::from_str(&call.args_json)?;
        let mut command = tokio::process::Command::new("sh");
        command.arg("-lc").arg(args.command).current_dir(&ctx.cwd);
        let output = tokio::time::timeout(ctx.timeout, command.output())
            .await
            .map_err(|_| ToolError::Timeout)??;
        let text = format!(
            "exit: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let result = if output.status.success() {
            ToolResultMessage::success(call.id.clone(), &call.tool_name, text)
        } else {
            ToolResultMessage::error(call.id.clone(), &call.tool_name, text)
        };
        Ok(result)
    }
}
