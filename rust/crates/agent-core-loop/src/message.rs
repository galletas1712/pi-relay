use crate::ids::EventId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreMessage {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
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
    pub id: EventId,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantMessage {
    pub id: EventId,
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
    pub id: EventId,
    pub tool_name: String,
    pub args_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolResultStatus {
    Success,
    Error,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultMessage {
    pub id: EventId,
    pub tool_call_id: EventId,
    pub tool_name: String,
    pub output: String,
    pub status: ToolResultStatus,
}

impl ToolResultMessage {
    pub fn interrupted(id: EventId, tool_call_id: EventId, tool_name: impl Into<String>) -> Self {
        Self {
            id,
            tool_call_id,
            tool_name: tool_name.into(),
            output: "interrupted".to_string(),
            status: ToolResultStatus::Interrupted,
        }
    }
}
