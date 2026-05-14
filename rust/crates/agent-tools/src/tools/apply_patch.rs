use std::process::Stdio;

use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::context::ToolContext;
use crate::error::{ToolError, ToolResult};
use crate::output::limit_tool_output;
use crate::registry::AgentTool;

#[derive(Debug, Clone, Copy)]
pub struct ApplyPatchTool;

#[derive(Debug, Deserialize)]
struct ApplyPatchArgs {
    input: String,
}

#[async_trait]
impl AgentTool for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".to_string(),
            description: "Apply a freeform patch to files in the workspace.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "input": { "type": "string" } },
                "required": ["input"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage> {
        let args: ApplyPatchArgs = serde_json::from_str(&call.args_json)?;
        let mut child = tokio::process::Command::new("apply_patch")
            .current_dir(&ctx.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(args.input.as_bytes()).await?;
        }
        drop(child.stdin.take());
        let output = tokio::time::timeout(ctx.timeout, child.wait_with_output())
            .await
            .map_err(|_| ToolError::Timeout)??;
        let text = format!(
            "exit: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let result = if output.status.success() {
            ToolResultMessage::success(call.id.clone(), &call.tool_name, limit_tool_output(text))
        } else {
            ToolResultMessage::error(call.id.clone(), &call.tool_name, limit_tool_output(text))
        };
        Ok(result)
    }
}
