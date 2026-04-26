use crate::ids::ToolCallId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub items: Vec<AssistantItem>,
}

impl AssistantMessage {
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.items.iter().filter_map(|item| match item {
            AssistantItem::ToolCall(tool_call) => Some(tool_call),
            AssistantItem::Text(_) => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum AssistantItem {
    Text(String),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub tool_name: String,
    pub args_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Success,
    Error,
    Interrupted,
    Crashed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub output: String,
    pub status: ToolResultStatus,
}

impl ToolResultMessage {
    pub fn interrupted(tool_call_id: ToolCallId, tool_name: impl Into<String>) -> Self {
        Self {
            tool_call_id,
            tool_name: tool_name.into(),
            output: "interrupted".to_string(),
            status: ToolResultStatus::Interrupted,
        }
    }

    pub fn crashed(tool_call_id: ToolCallId, tool_name: impl Into<String>) -> Self {
        Self {
            tool_call_id,
            tool_name: tool_name.into(),
            output: "crashed before tool result was recorded".to_string(),
            status: ToolResultStatus::Crashed,
        }
    }
}
