use crate::ids::ToolCallId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactMessage {
    User(UserMessage),
    Assistant(AssistantMessage),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInput {
    pub text: String,
}

impl From<&str> for UserInput {
    fn from(value: &str) -> Self {
        Self {
            text: value.to_string(),
        }
    }
}

impl From<String> for UserInput {
    fn from(value: String) -> Self {
        Self { text: value }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessage {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantItem {
    Text(String),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub tool_name: String,
    pub args_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolResultStatus {
    Success,
    Error,
    Interrupted,
    Crashed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
