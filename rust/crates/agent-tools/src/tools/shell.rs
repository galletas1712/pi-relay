use std::time::Duration;

use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::context::{workspace_path, ToolContext};
use crate::error::{ToolError, ToolResult};
use crate::output::limit_tool_output;
use crate::registry::AgentTool;

#[derive(Debug, Clone, Copy)]
pub struct BashTool;

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
}

#[derive(Debug, Deserialize)]
struct ShellArgs {
    command: ShellCommand,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ShellCommand {
    Text(String),
    Argv(Vec<String>),
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
        run_shell(
            call,
            ctx,
            ShellArgs {
                command: ShellCommand::Text(args.command),
                workdir: None,
                timeout_ms: None,
            },
        )
        .await
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ShellTool;

#[async_trait]
impl AgentTool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".to_string(),
            description: "Run a local shell command in the workspace.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "oneOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ]
                    },
                    "workdir": { "type": "string" },
                    "timeout_ms": { "type": "integer" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage> {
        let args: ShellArgs = serde_json::from_str(&call.args_json)?;
        run_shell(call, ctx, args).await
    }
}

async fn run_shell(
    call: &ToolCall,
    ctx: &ToolContext,
    args: ShellArgs,
) -> ToolResult<ToolResultMessage> {
    let cwd = args
        .workdir
        .as_ref()
        .map(|path| workspace_path(ctx, path))
        .unwrap_or_else(|| ctx.cwd.clone());
    let timeout = args
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(ctx.timeout);
    let mut command = match args.command {
        ShellCommand::Text(command) => {
            let mut process = tokio::process::Command::new("sh");
            process.arg("-lc").arg(command);
            process
        }
        ShellCommand::Argv(argv) => {
            let Some((program, argv)) = argv.split_first() else {
                return Err(ToolError::InvalidInput(
                    "shell command argv cannot be empty".to_string(),
                ));
            };
            let mut process = tokio::process::Command::new(program);
            process.args(argv);
            process
        }
    };
    command.current_dir(cwd);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| ToolError::Timeout)??;
    let text = format!(
        "exit: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = limit_tool_output(text);
    let result = if output.status.success() {
        ToolResultMessage::success(call.id.clone(), &call.tool_name, text)
    } else {
        ToolResultMessage::error(call.id.clone(), &call.tool_name, text)
    };
    Ok(result)
}
