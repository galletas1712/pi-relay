use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::ids::ToolCallId;
use crate::provider::ProviderKind;

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
            AssistantItem::Text(_) | AssistantItem::ProviderReplayRecord(_) => None,
        })
    }

    pub fn text(&self) -> String {
        self.items
            .iter()
            .filter_map(|item| match item {
                AssistantItem::Text(text) => Some(text.as_str()),
                AssistantItem::ToolCall(_) | AssistantItem::ProviderReplayRecord(_) => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn replay_records(&self) -> impl Iterator<Item = &ProviderReplayRecord> {
        self.items.iter().filter_map(|item| match item {
            AssistantItem::ProviderReplayRecord(record) => Some(record),
            AssistantItem::Text(_) | AssistantItem::ToolCall(_) => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantItem {
    Text(String),
    ToolCall(ToolCall),
    ProviderReplayRecord(ProviderReplayRecord),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderReplayRecord {
    pub provider: ProviderKind,
    pub record_type: String,
    pub raw_json: String,
}

impl ProviderReplayRecord {
    pub fn new(
        provider: ProviderKind,
        record_type: impl Into<String>,
        raw: &Value,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            provider,
            record_type: record_type.into(),
            raw_json: serde_json::to_string(raw)?,
        })
    }

    pub fn raw_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.raw_json)
    }
}

impl Serialize for AssistantItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Text(text) => {
                let mut state = serializer.serialize_struct("AssistantItem", 2)?;
                state.serialize_field("type", "text")?;
                state.serialize_field("text", text)?;
                state.end()
            }
            Self::ToolCall(call) => {
                let mut state = serializer.serialize_struct("AssistantItem", 4)?;
                state.serialize_field("type", "tool_call")?;
                state.serialize_field("id", &call.id)?;
                state.serialize_field("tool_name", &call.tool_name)?;
                state.serialize_field("args_json", &call.args_json)?;
                state.end()
            }
            Self::ProviderReplayRecord(record) => {
                let mut state = serializer.serialize_struct("AssistantItem", 4)?;
                state.serialize_field("type", "provider_replay_record")?;
                state.serialize_field("provider", &record.provider)?;
                state.serialize_field("record_type", &record.record_type)?;
                state.serialize_field("raw_json", &record.raw_json)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for AssistantItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(AssistantItemVisitor)
    }
}

struct AssistantItemVisitor;

impl<'de> Visitor<'de> for AssistantItemVisitor {
    type Value = AssistantItem;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an assistant item object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut kind: Option<String> = None;
        let mut text: Option<String> = None;
        let mut id: Option<ToolCallId> = None;
        let mut tool_name: Option<String> = None;
        let mut args_json: Option<String> = None;
        let mut provider: Option<ProviderKind> = None;
        let mut record_type: Option<String> = None;
        let mut raw_json: Option<String> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "type" => kind = Some(map.next_value()?),
                "text" => text = Some(map.next_value()?),
                "id" => id = Some(map.next_value()?),
                "tool_name" => tool_name = Some(map.next_value()?),
                "args_json" => args_json = Some(map.next_value()?),
                "provider" => provider = Some(map.next_value()?),
                "record_type" => record_type = Some(map.next_value()?),
                "raw_json" => raw_json = Some(map.next_value()?),
                _ => {
                    let _ = map.next_value::<de::IgnoredAny>()?;
                }
            }
        }

        match kind.as_deref() {
            Some("text") => Ok(AssistantItem::Text(text.unwrap_or_default())),
            Some("tool_call") => Ok(AssistantItem::ToolCall(ToolCall {
                id: id.ok_or_else(|| de::Error::missing_field("id"))?,
                tool_name: tool_name.ok_or_else(|| de::Error::missing_field("tool_name"))?,
                args_json: args_json.unwrap_or_else(|| "{}".to_string()),
            })),
            Some("provider_replay_record") => {
                Ok(AssistantItem::ProviderReplayRecord(ProviderReplayRecord {
                    provider: provider.ok_or_else(|| de::Error::missing_field("provider"))?,
                    record_type: record_type
                        .ok_or_else(|| de::Error::missing_field("record_type"))?,
                    raw_json: raw_json.ok_or_else(|| de::Error::missing_field("raw_json"))?,
                }))
            }
            Some(other) => Err(de::Error::unknown_variant(
                other,
                &["text", "tool_call", "provider_replay_record"],
            )),
            None => Err(de::Error::missing_field("type")),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assistant_item_serializes_as_tagged_objects() {
        let message = AssistantMessage {
            items: vec![
                AssistantItem::Text("hello".to_string()),
                AssistantItem::ToolCall(ToolCall {
                    id: ToolCallId::new("call_1"),
                    tool_name: "read".to_string(),
                    args_json: "{\"path\":\"README.md\"}".to_string(),
                }),
                AssistantItem::ProviderReplayRecord(ProviderReplayRecord {
                    provider: ProviderKind::OpenAi,
                    record_type: "reasoning".to_string(),
                    raw_json: "{\"type\":\"reasoning\",\"encrypted_content\":\"opaque\"}"
                        .to_string(),
                }),
            ],
        };

        let value = serde_json::to_value(&message).expect("assistant message serializes");
        assert_eq!(
            value,
            json!({
                "items": [
                    { "type": "text", "text": "hello" },
                    {
                        "type": "tool_call",
                        "id": "call_1",
                        "tool_name": "read",
                        "args_json": "{\"path\":\"README.md\"}",
                    },
                    {
                        "type": "provider_replay_record",
                        "provider": "openai",
                        "record_type": "reasoning",
                        "raw_json": "{\"type\":\"reasoning\",\"encrypted_content\":\"opaque\"}",
                    }
                ]
            })
        );

        let round_trip: AssistantMessage =
            serde_json::from_value(value).expect("assistant message deserializes");
        assert_eq!(round_trip, message);
    }
}
