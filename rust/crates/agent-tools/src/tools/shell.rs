use std::time::Duration;

use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::context::ToolContext;
use crate::error::{ToolError, ToolResult};
use crate::output::limit_tool_output;
use crate::registry::AgentTool;

/// Single shell tool, registered as `Bash` for both providers.
///
/// Each call runs in a fresh `sh -lc` subprocess rooted at the daemon
/// workspace. There is no persistent shell state across calls and no
/// per-call working-directory override — the model is told to chain with
/// `&&` (or call `cd` inside the command) when it needs to scope work to a
/// subdirectory.
#[derive(Debug, Clone, Copy)]
pub struct BashTool;

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: ShellCommand,
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
        ToolDefinition::new(
            "Bash",
            "Run a shell command in the daemon workspace and return stdout/stderr. \
                Each call runs in a fresh shell rooted at the workspace; chain commands with `&&` \
                (or call `cd` inside the command) when you need to scope work to a subdirectory."
                .to_string(),
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "oneOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ],
                        "description": "Shell command to execute. Either a single string (run via `sh -lc`) or an argv array (executed directly)."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Optional command timeout in milliseconds."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage> {
        let args: BashArgs = serde_json::from_str(&call.args_json)?;
        run_bash(call, ctx, args).await
    }
}

async fn run_bash(
    call: &ToolCall,
    ctx: &ToolContext,
    args: BashArgs,
) -> ToolResult<ToolResultMessage> {
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
                    "bash command argv cannot be empty".to_string(),
                ));
            };
            let mut process = tokio::process::Command::new(program);
            process.args(argv);
            process
        }
    };
    command.current_dir(&ctx.cwd);
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
