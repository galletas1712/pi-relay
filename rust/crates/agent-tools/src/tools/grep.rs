use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::context::{workspace_path, ToolContext};
use crate::error::{ToolError, ToolResult};
use crate::output::limit_tool_output;
use crate::registry::AgentTool;

#[derive(Debug, Clone, Copy)]
pub struct GrepTool;

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    case_sensitive: Option<bool>,
    #[serde(default)]
    context: Option<u32>,
    #[serde(default)]
    max_matches: Option<u32>,
}

#[async_trait]
impl AgentTool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "Grep",
            "Search files under the session current working directory with ripgrep and return matching lines.".to_string(),
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "case_sensitive": { "type": "boolean" },
                    "context": { "type": "integer" },
                    "max_matches": { "type": "integer" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage> {
        let args: GrepArgs = serde_json::from_str(&call.args_json)?;
        let target = args
            .path
            .as_ref()
            .map(|path| workspace_path(ctx, path))
            .unwrap_or_else(|| ctx.cwd.clone());
        let mut command = tokio::process::Command::new("rg");
        command
            .arg("--line-number")
            .arg("--column")
            .arg("--hidden")
            .arg("--glob")
            .arg("!.git");
        if args.case_sensitive == Some(false) {
            command.arg("--ignore-case");
        }
        if let Some(context) = args.context {
            command.arg("--context").arg(context.to_string());
        }
        if let Some(max_matches) = args.max_matches {
            command.arg("--max-count").arg(max_matches.to_string());
        }
        command.arg(args.pattern).arg(target).current_dir(&ctx.cwd);
        let output = tokio::time::timeout(ctx.timeout, command.output())
            .await
            .map_err(|_| ToolError::Timeout)??;
        let text = format!(
            "exit: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let result = if output.status.success() || output.status.code() == Some(1) {
            ToolResultMessage::success(call.id.clone(), &call.tool_name, limit_tool_output(text))
        } else {
            ToolResultMessage::error(call.id.clone(), &call.tool_name, limit_tool_output(text))
        };
        Ok(result)
    }
}
