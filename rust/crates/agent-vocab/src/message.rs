use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::ToolCallId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Vec<ContentBlock>,
}

impl UserMessage {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn from_parts(content: Vec<ContentBlock>) -> Self {
        Self { content }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self.content.as_slice() {
            [ContentBlock::Text { text }] => Some(text.as_str()),
            _ => None,
        }
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.as_text().is_some_and(|text| text.contains(needle))
    }
}

impl From<String> for UserMessage {
    fn from(value: String) -> Self {
        Self::text(value)
    }
}

impl From<&str> for UserMessage {
    fn from(value: &str) -> Self {
        Self::text(value)
    }
}

impl PartialEq<&str> for UserMessage {
    fn eq(&self, other: &&str) -> bool {
        self.as_text() == Some(*other)
    }
}

impl PartialEq<&str> for &UserMessage {
    fn eq(&self, other: &&str) -> bool {
        self.as_text() == Some(*other)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { image: ImageContent },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image(image: ImageContent) -> Self {
        Self::Image { image }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageContent {
    pub mime_type: String,
    pub source: ImageSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ImageSource {
    Base64(String),
    Url(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub items: Vec<AssistantItem>,
}

impl AssistantMessage {
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.items.iter().filter_map(|item| match item {
            AssistantItem::ToolCall(tool_call) => Some(tool_call),
            AssistantItem::Text(_) | AssistantItem::ThinkingRedacted => None,
        })
    }

    pub fn text(&self) -> String {
        self.items
            .iter()
            .filter_map(|item| match item {
                AssistantItem::Text(text) => Some(text.as_str()),
                AssistantItem::ToolCall(_) | AssistantItem::ThinkingRedacted => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantItem {
    Text(String),
    ThinkingRedacted,
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub tool_name: String,
    pub args_json: String,
}

impl ToolCall {
    pub fn args_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.args_json)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub fn success(
        tool_call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            output: output.into(),
            status: ToolResultStatus::Success,
        }
    }

    pub fn error(
        tool_call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            output: output.into(),
            status: ToolResultStatus::Error,
        }
    }

    pub fn interrupted(tool_call_id: impl Into<ToolCallId>, tool_name: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            output: "interrupted".to_string(),
            status: ToolResultStatus::Interrupted,
        }
    }

    pub fn crashed(tool_call_id: impl Into<ToolCallId>, tool_name: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            output: "crashed before tool result was recorded".to_string(),
            status: ToolResultStatus::Crashed,
        }
    }
}
