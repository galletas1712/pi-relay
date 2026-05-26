use std::path::{Component, Path, PathBuf};

use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::context::ToolContext;
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
            "Search files under the session current working directory with ripgrep and return matching lines with paths relative to that directory.".to_string(),
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
        let target = args.path.as_deref().and_then(|path| {
            let path = Path::new(path);
            if path.as_os_str().is_empty() {
                return None;
            }

            let relative_path = if path.is_absolute() {
                match path.strip_prefix(&ctx.cwd) {
                    Ok(path) => path,
                    Err(_) => return Some(path.to_path_buf()),
                }
            } else {
                path
            };

            let mut normalized_path = PathBuf::new();
            for component in relative_path.components() {
                match component {
                    Component::CurDir => {}
                    Component::Normal(segment) => normalized_path.push(segment),
                    Component::ParentDir => normalized_path.push(".."),
                    Component::RootDir | Component::Prefix(_) => {
                        normalized_path.push(component.as_os_str());
                    }
                }
            }
            if normalized_path.as_os_str().is_empty() {
                None
            } else {
                Some(normalized_path)
            }
        });
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
        command.arg(args.pattern);
        if let Some(target) = target {
            command.arg(target);
        }
        command.current_dir(&ctx.cwd);
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_vocab::{ToolCallId, ToolResultMessage};

    use super::*;

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn workspace() -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("pi-relay-grep-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp workspace");
        path
    }

    fn call(pattern: &str, path: Option<&str>) -> ToolCall {
        let mut args = json!({ "pattern": pattern });
        if let Some(path) = path {
            args["path"] = json!(path);
        }
        ToolCall {
            id: ToolCallId("call_grep".to_string()),
            tool_name: "Grep".to_string(),
            args_json: args.to_string(),
        }
    }

    #[tokio::test]
    async fn returns_paths_relative_to_workspace_for_relative_path_argument() {
        let root = workspace();
        let nested_dir = root.join("repo/src");
        fs::create_dir_all(&nested_dir).expect("create nested dir");
        fs::write(nested_dir.join("lib.rs"), "needle\n").expect("write fixture");
        let ctx = ToolContext::new(&root);

        let result = GrepTool
            .execute(&call("needle", Some("repo")), &ctx)
            .await
            .expect("grep execution succeeds");

        assert_eq!(
            result,
            ToolResultMessage::success(
                ToolCallId("call_grep".to_string()),
                "Grep",
                "exit: exit status: 0\nstdout:\nrepo/src/lib.rs:1:1:needle\n\nstderr:\n"
            )
        );
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn strips_current_dir_components_from_path_argument() {
        let root = workspace();
        let nested_dir = root.join("repo/src");
        fs::create_dir_all(&nested_dir).expect("create nested dir");
        fs::write(nested_dir.join("lib.rs"), "needle\n").expect("write fixture");
        let ctx = ToolContext::new(&root);

        let result = GrepTool
            .execute(&call("needle", Some("./repo/.")), &ctx)
            .await
            .expect("grep execution succeeds");

        assert_eq!(
            result,
            ToolResultMessage::success(
                ToolCallId("call_grep".to_string()),
                "Grep",
                "exit: exit status: 0\nstdout:\nrepo/src/lib.rs:1:1:needle\n\nstderr:\n"
            )
        );
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn returns_paths_relative_to_workspace_for_absolute_path_argument() {
        let root = workspace();
        let nested_dir = root.join("repo/src");
        fs::create_dir_all(&nested_dir).expect("create nested dir");
        fs::write(nested_dir.join("lib.rs"), "needle\n").expect("write fixture");
        let ctx = ToolContext::new(&root);

        let result = GrepTool
            .execute(
                &call("needle", Some(root.join("repo").to_string_lossy().as_ref())),
                &ctx,
            )
            .await
            .expect("grep execution succeeds");

        assert_eq!(
            result,
            ToolResultMessage::success(
                ToolCallId("call_grep".to_string()),
                "Grep",
                "exit: exit status: 0\nstdout:\nrepo/src/lib.rs:1:1:needle\n\nstderr:\n"
            )
        );
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn returns_paths_relative_to_workspace_without_path_argument() {
        let root = workspace();
        let nested_dir = root.join("repo/src");
        fs::create_dir_all(&nested_dir).expect("create nested dir");
        fs::write(nested_dir.join("lib.rs"), "needle\n").expect("write fixture");
        let ctx = ToolContext::new(&root);

        let result = GrepTool
            .execute(&call("needle", None), &ctx)
            .await
            .expect("grep execution succeeds");

        assert_eq!(
            result,
            ToolResultMessage::success(
                ToolCallId("call_grep".to_string()),
                "Grep",
                "exit: exit status: 0\nstdout:\nrepo/src/lib.rs:1:1:needle\n\nstderr:\n"
            )
        );
        fs::remove_dir_all(root).ok();
    }
}
