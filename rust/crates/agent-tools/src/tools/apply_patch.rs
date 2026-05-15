use std::fs;
use std::path::Path;

use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::context::{workspace_path, ToolContext};
use crate::error::ToolResult;
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
        let result = match apply_patch(&args.input, ctx) {
            Ok(changes) => ToolResultMessage::success(
                call.id.clone(),
                &call.tool_name,
                limit_tool_output(success_message(&changes)),
            ),
            Err(error) => ToolResultMessage::error(call.id.clone(), &call.tool_name, error),
        };
        Ok(result)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatchOp {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<LineHunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LineHunk {
    old: String,
    new: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileChange {
    marker: &'static str,
    path: String,
}

fn apply_patch(input: &str, ctx: &ToolContext) -> Result<Vec<FileChange>, String> {
    let ops = parse_patch(input)?;
    let mut changes = Vec::new();
    for op in ops {
        match op {
            PatchOp::Add { path, lines } => {
                let full_path = workspace_path(ctx, &path);
                if full_path.exists() {
                    return Err(format!("file already exists: {path}"));
                }
                write_file(&full_path, &lines_to_text(&lines))
                    .map_err(|error| format!("{path}: {error}"))?;
                changes.push(FileChange { marker: "A", path });
            }
            PatchOp::Delete { path } => {
                let full_path = workspace_path(ctx, &path);
                fs::remove_file(&full_path).map_err(|error| format!("{path}: {error}"))?;
                changes.push(FileChange { marker: "D", path });
            }
            PatchOp::Update {
                path,
                move_to,
                hunks,
            } => {
                let source_path = workspace_path(ctx, &path);
                let mut content =
                    fs::read_to_string(&source_path).map_err(|error| format!("{path}: {error}"))?;
                for hunk in hunks {
                    if hunk.old.is_empty() {
                        return Err(format!(
                            "{path}: update hunk has no context or removed lines"
                        ));
                    }
                    let Some(offset) = content.find(&hunk.old) else {
                        return Err(format!("{path}: update hunk did not match file contents"));
                    };
                    content.replace_range(offset..offset + hunk.old.len(), &hunk.new);
                }

                let marker = if move_to.is_some() { "R" } else { "M" };
                let display_path = if let Some(target) = move_to {
                    let target_path = workspace_path(ctx, &target);
                    write_file(&target_path, &content)
                        .map_err(|error| format!("{target}: {error}"))?;
                    fs::remove_file(&source_path).map_err(|error| format!("{path}: {error}"))?;
                    format!("{path} -> {target}")
                } else {
                    write_file(&source_path, &content)
                        .map_err(|error| format!("{path}: {error}"))?;
                    path
                };
                changes.push(FileChange {
                    marker,
                    path: display_path,
                });
            }
        }
    }
    Ok(changes)
}

fn parse_patch(input: &str) -> Result<Vec<PatchOp>, String> {
    let mut lines = input.split('\n').collect::<Vec<_>>();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.first() != Some(&"*** Begin Patch") {
        return Err("patch must start with *** Begin Patch".to_string());
    }

    let mut index = 1;
    let mut ops = Vec::new();
    while index < lines.len() {
        let line = lines[index];
        if line == "*** End Patch" {
            if index + 1 != lines.len() {
                return Err("unexpected content after *** End Patch".to_string());
            }
            return Ok(ops);
        }

        if let Some(path) = line.strip_prefix("*** Add File: ") {
            index += 1;
            let mut add_lines = Vec::new();
            while index < lines.len() && !is_patch_boundary(lines[index]) {
                let Some(content) = lines[index].strip_prefix('+') else {
                    return Err(format!("add file line must start with +: {}", lines[index]));
                };
                add_lines.push(content.to_string());
                index += 1;
            }
            if add_lines.is_empty() {
                return Err(format!("add file has no content: {path}"));
            }
            ops.push(PatchOp::Add {
                path: path.to_string(),
                lines: add_lines,
            });
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(PatchOp::Delete {
                path: path.to_string(),
            });
            index += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Update File: ") {
            index += 1;
            let move_to = if let Some(target) = lines
                .get(index)
                .and_then(|line| line.strip_prefix("*** Move to: "))
            {
                index += 1;
                Some(target.to_string())
            } else {
                None
            };

            let mut hunks = Vec::new();
            let mut old = String::new();
            let mut new = String::new();
            while index < lines.len() && !is_patch_boundary(lines[index]) {
                let line = lines[index];
                if line == "@@" || line.starts_with("@@ ") {
                    push_hunk(&mut hunks, &mut old, &mut new);
                    index += 1;
                    continue;
                }
                if line == "*** End of File" {
                    index += 1;
                    continue;
                }
                let Some(prefix) = line.chars().next() else {
                    return Err("empty update line".to_string());
                };
                let content = &line[prefix.len_utf8()..];
                match prefix {
                    ' ' => {
                        old.push_str(content);
                        old.push('\n');
                        new.push_str(content);
                        new.push('\n');
                    }
                    '-' => {
                        old.push_str(content);
                        old.push('\n');
                    }
                    '+' => {
                        new.push_str(content);
                        new.push('\n');
                    }
                    _ => return Err(format!("invalid update line: {line}")),
                }
                index += 1;
            }
            push_hunk(&mut hunks, &mut old, &mut new);
            if hunks.is_empty() && move_to.is_none() {
                return Err(format!("update file has no changes: {path}"));
            }
            ops.push(PatchOp::Update {
                path: path.to_string(),
                move_to,
                hunks,
            });
            continue;
        }

        return Err(format!("unknown patch line: {line}"));
    }

    Err("patch must end with *** End Patch".to_string())
}

fn push_hunk(hunks: &mut Vec<LineHunk>, old: &mut String, new: &mut String) {
    if old.is_empty() && new.is_empty() {
        return;
    }
    hunks.push(LineHunk {
        old: std::mem::take(old),
        new: std::mem::take(new),
    });
}

fn is_patch_boundary(line: &str) -> bool {
    line == "*** End Patch"
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}

fn lines_to_text(lines: &[String]) -> String {
    let mut content = lines.join("\n");
    content.push('\n');
    content
}

fn write_file(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)
}

fn success_message(changes: &[FileChange]) -> String {
    let mut message = "Success. Updated the following files:\n".to_string();
    for change in changes {
        message.push_str(change.marker);
        message.push(' ');
        message.push_str(&change.path);
        message.push('\n');
    }
    message
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_vocab::{ToolCallId, ToolResultStatus};

    use super::*;

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn workspace() -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("pi-relay-apply-patch-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp workspace");
        path
    }

    fn call(input: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId("call_patch".to_string()),
            tool_name: "apply_patch".to_string(),
            args_json: json!({ "input": input }).to_string(),
        }
    }

    #[test]
    fn adds_file_relative_to_workspace() {
        let root = workspace();
        let ctx = ToolContext::new(&root);
        let patch =
            "*** Begin Patch\n*** Add File: nested/hello.txt\n+hello\n+world\n*** End Patch\n";

        let changes = apply_patch(patch, &ctx).expect("apply patch");

        assert_eq!(changes[0].marker, "A");
        assert_eq!(
            fs::read_to_string(root.join("nested/hello.txt")).expect("read file"),
            "hello\nworld\n"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn updates_file_with_exact_hunk() {
        let root = workspace();
        fs::write(root.join("app.txt"), "alpha\nbeta\ngamma\n").expect("write file");
        let ctx = ToolContext::new(&root);
        let patch = "*** Begin Patch\n*** Update File: app.txt\n@@\n alpha\n-beta\n+bravo\n gamma\n*** End Patch\n";

        apply_patch(patch, &ctx).expect("apply patch");

        assert_eq!(
            fs::read_to_string(root.join("app.txt")).expect("read file"),
            "alpha\nbravo\ngamma\n"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn deletes_file() {
        let root = workspace();
        fs::write(root.join("old.txt"), "bye\n").expect("write file");
        let ctx = ToolContext::new(&root);
        let patch = "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch\n";

        apply_patch(patch, &ctx).expect("apply patch");

        assert!(!root.join("old.txt").exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn moves_and_updates_file() {
        let root = workspace();
        fs::write(root.join("old.txt"), "name = old\n").expect("write file");
        let ctx = ToolContext::new(&root);
        let patch = "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n@@\n-name = old\n+name = new\n*** End Patch\n";

        let changes = apply_patch(patch, &ctx).expect("apply patch");

        assert_eq!(changes[0].marker, "R");
        assert!(!root.join("old.txt").exists());
        assert_eq!(
            fs::read_to_string(root.join("new.txt")).expect("read file"),
            "name = new\n"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn reports_unmatched_update() {
        let root = workspace();
        fs::write(root.join("app.txt"), "alpha\n").expect("write file");
        let ctx = ToolContext::new(&root);
        let patch = "*** Begin Patch\n*** Update File: app.txt\n@@\n-beta\n+bravo\n*** End Patch\n";

        let error = apply_patch(patch, &ctx).expect_err("patch should fail");

        assert_eq!(error, "app.txt: update hunk did not match file contents");
        assert_eq!(
            fs::read_to_string(root.join("app.txt")).expect("read file"),
            "alpha\n"
        );
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn execute_returns_tool_error_instead_of_spawning_external_binary() {
        let root = workspace();
        let ctx = ToolContext::new(&root);
        let result = ApplyPatchTool
            .execute(
                &call("*** Begin Patch\n*** Delete File: missing.txt\n*** End Patch\n"),
                &ctx,
            )
            .await
            .expect("tool execution returns a tool result");

        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.output.contains("missing.txt"));
        fs::remove_dir_all(root).ok();
    }
}
