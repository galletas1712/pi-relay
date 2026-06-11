use std::time::Duration;

use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::context::ToolContext;
use crate::error::ToolResult;
use crate::output::limit_tool_output_with_max_tokens;
use crate::registry::AgentTool;

/// Single shell tool, registered as `Bash` for both providers.
///
/// Each call runs in a fresh `bash -lc` subprocess rooted at the session
/// current working directory. There is no persistent shell state across calls and no
/// per-call working-directory override — the model is told to chain with
/// `&&` (or call `cd` inside the command) when it needs to scope work to a
/// subdirectory.
#[derive(Debug, Clone, Copy)]
pub struct BashTool;

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[async_trait]
impl AgentTool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "Bash",
            "Run a shell command in the session current working directory and return stdout/stderr. \
                Each call runs in a fresh shell rooted at that cwd; chain commands with `&&` \
                (or call `cd` inside the command) when you need to scope work to a subdirectory."
                .to_string(),
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute via `bash -lc`."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Optional command timeout in milliseconds."
                    },
                    "max_output_tokens": {
                        "type": "integer",
                        "description": "Maximum number of tokens to return. Excess output will be truncated. Defaults to 10000."
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
    let mut command = tokio::process::Command::new("bash");
    command.arg("-lc").arg(args.command);
    command.current_dir(&ctx.cwd);
    // Keep timeouts as ordinary tool results so the transcript records a
    // recoverable failure at the tool-call boundary instead of a generic crash.
    command.kill_on_drop(true);
    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(output) => output?,
        Err(_) => {
            return Ok(ToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                format!("command timed out after {} ms", timeout.as_millis()),
            ));
        }
    };
    let text = format!(
        "exit: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = limit_tool_output_with_max_tokens(text, args.max_output_tokens);
    let result = if output.status.success() {
        ToolResultMessage::success(call.id.clone(), &call.tool_name, text)
    } else {
        ToolResultMessage::error(call.id.clone(), &call.tool_name, text)
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_vocab::{ToolCallId, ToolResultMessage, ToolResultStatus};
    use serde_json::json;

    use super::*;

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn workspace() -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("pi-relay-bash-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp workspace");
        path
    }

    fn text_call(command: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId("call_bash".to_string()),
            tool_name: "Bash".to_string(),
            args_json: json!({ "command": command }).to_string(),
        }
    }

    #[test]
    fn definition_advertises_string_command_only() {
        let definition = BashTool.definition();

        assert_eq!(
            definition.input_schema["properties"]["command"]["type"],
            "string"
        );
        assert!(definition.input_schema["properties"]["command"]
            .get("oneOf")
            .is_none());
    }

    #[tokio::test]
    async fn text_commands_run_with_bash_semantics() {
        let root = workspace();
        let ctx = ToolContext::new(&root);

        let result = BashTool
            .execute(
                &text_call(r#"[[ -n "${BASH_VERSION:-}" ]] && printf 'bash\n'"#),
                &ctx,
            )
            .await
            .expect("bash execution succeeds");

        assert_eq!(
            result,
            ToolResultMessage::success(
                ToolCallId("call_bash".to_string()),
                "Bash",
                "exit: exit status: 0\nstdout:\nbash\n\nstderr:\n",
            )
        );
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn text_commands_do_not_enable_strict_mode() {
        let root = workspace();
        let ctx = ToolContext::new(&root);

        let result = BashTool
            .execute(
                &text_call("printf 'before\\n'; false; printf 'after\\n'"),
                &ctx,
            )
            .await
            .expect("bash execution succeeds");

        assert_eq!(
            result,
            ToolResultMessage::success(
                ToolCallId("call_bash".to_string()),
                "Bash",
                "exit: exit status: 0\nstdout:\nbefore\nafter\n\nstderr:\n",
            )
        );
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn timeouts_return_tool_error_results() {
        let root = workspace();
        let ctx = ToolContext::new(&root);
        let call = ToolCall {
            id: ToolCallId("call_bash".to_string()),
            tool_name: "Bash".to_string(),
            args_json: json!({ "command": "sleep 1", "timeout_ms": 10 }).to_string(),
        };

        let result = BashTool
            .execute(&call, &ctx)
            .await
            .expect("timeout is represented as a tool result");

        assert_eq!(result.status, ToolResultStatus::Error);
        assert_eq!(result.tool_call_id, ToolCallId("call_bash".to_string()));
        assert_eq!(result.tool_name, "Bash");
        assert!(result.output.contains("command timed out after"));
        fs::remove_dir_all(root).ok();
    }
}
