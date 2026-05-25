use std::path::Path;

use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::context::{workspace_path, ToolContext};
use crate::error::{ToolError, ToolResult};
use crate::output::limit_tool_output;
use crate::registry::AgentTool;

#[derive(Debug, Clone, Copy)]
pub struct TextEditorTool;

#[derive(Debug, Deserialize)]
struct TextEditorArgs {
    command: String,
    path: String,
    #[serde(default)]
    file_text: Option<String>,
    #[serde(default)]
    old_str: Option<String>,
    #[serde(default)]
    new_str: Option<String>,
    #[serde(default)]
    insert_line: Option<usize>,
    #[serde(default)]
    view_range: Option<Vec<usize>>,
}

#[async_trait]
impl AgentTool for TextEditorTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "Edit",
            "View and edit text files under the session current working directory with Claude's text editor schema.".to_string(),
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "path": { "type": "string" },
                    "file_text": { "type": "string" },
                    "old_str": { "type": "string" },
                    "new_str": { "type": "string" },
                    "insert_line": { "type": "integer" },
                    "view_range": {
                        "type": "array",
                        "items": { "type": "integer" }
                    }
                },
                "required": ["command", "path"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult<ToolResultMessage> {
        let args: TextEditorArgs = serde_json::from_str(&call.args_json)?;
        let path = workspace_path(ctx, &args.path);
        let output = match args.command.as_str() {
            "view" => text_editor_view(&path, args.view_range).await?,
            "create" => {
                let Some(file_text) = args.file_text else {
                    return Err(ToolError::InvalidInput(
                        "create requires file_text".to_string(),
                    ));
                };
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&path, file_text).await?;
                "created".to_string()
            }
            "str_replace" => {
                let Some(old_str) = args.old_str else {
                    return Err(ToolError::InvalidInput(
                        "str_replace requires old_str".to_string(),
                    ));
                };
                let new_str = args.new_str.unwrap_or_default();
                let original = tokio::fs::read_to_string(&path).await?;
                if !original.contains(&old_str) {
                    return Err(ToolError::EditTargetNotFound);
                }
                tokio::fs::write(&path, original.replacen(&old_str, &new_str, 1)).await?;
                "edited".to_string()
            }
            "insert" => {
                let Some(insert_line) = args.insert_line else {
                    return Err(ToolError::InvalidInput(
                        "insert requires insert_line".to_string(),
                    ));
                };
                let Some(new_str) = args.new_str else {
                    return Err(ToolError::InvalidInput(
                        "insert requires new_str".to_string(),
                    ));
                };
                let original = tokio::fs::read_to_string(&path).await?;
                let mut lines = original.lines().map(str::to_string).collect::<Vec<_>>();
                let index = insert_line.min(lines.len());
                lines.insert(index, new_str);
                tokio::fs::write(&path, format!("{}\n", lines.join("\n"))).await?;
                "inserted".to_string()
            }
            other => {
                return Err(ToolError::InvalidInput(format!(
                    "unsupported text editor command: {other}"
                )))
            }
        };
        Ok(ToolResultMessage::success(
            call.id.clone(),
            &call.tool_name,
            limit_tool_output(output),
        ))
    }
}

async fn text_editor_view(path: &Path, view_range: Option<Vec<usize>>) -> ToolResult<String> {
    let metadata = tokio::fs::metadata(path).await?;
    if metadata.is_dir() {
        let mut entries = tokio::fs::read_dir(path).await?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        return Ok(names.join("\n"));
    }
    let text = tokio::fs::read_to_string(path).await?;
    if let Some(range) = view_range.filter(|range| range.len() == 2) {
        let start = range[0].max(1);
        let end = range[1].max(start);
        let lines = text
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                let line_number = index + 1;
                (line_number >= start && line_number <= end)
                    .then(|| format!("{line_number}: {line}"))
            })
            .collect::<Vec<_>>();
        return Ok(lines.join("\n"));
    }
    Ok(text)
}
