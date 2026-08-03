use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::fmt;

use crate::ids::ToolCallId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub replayed_after_compaction: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineToolResultMessage {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub content: Vec<InlineContentBlock>,
    pub status: ToolResultStatus,
}

impl InlineToolResultMessage {
    pub fn success(
        tool_call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self::success_content(
            tool_call_id,
            tool_name,
            vec![InlineContentBlock::text(output)],
        )
    }

    pub fn success_content(
        tool_call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        content: Vec<InlineContentBlock>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content,
            status: ToolResultStatus::Success,
        }
    }

    pub fn error(
        tool_call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self::error_content(
            tool_call_id,
            tool_name,
            vec![InlineContentBlock::text(output)],
        )
    }

    pub fn error_content(
        tool_call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        content: Vec<InlineContentBlock>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content,
            status: ToolResultStatus::Error,
        }
    }

    pub fn interrupted(tool_call_id: impl Into<ToolCallId>, tool_name: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![InlineContentBlock::text("interrupted")],
            status: ToolResultStatus::Interrupted,
        }
    }

    pub fn crashed(tool_call_id: impl Into<ToolCallId>, tool_name: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![InlineContentBlock::text(
                "crashed before tool result was recorded",
            )],
            status: ToolResultStatus::Crashed,
        }
    }

    pub fn display_text(&self) -> String {
        self.content
            .iter()
            .map(|block| match block {
                InlineContentBlock::Text { text } => text.clone(),
                InlineContentBlock::Image { mime_type, .. } => format!("[image {mime_type}]"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn into_durable_text(self) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: self.tool_call_id,
            tool_name: self.tool_name,
            content: self
                .content
                .into_iter()
                .map(|block| match block {
                    InlineContentBlock::Text { text } => ContentBlock::Text { text },
                    InlineContentBlock::Image { mime_type, .. } => {
                        ContentBlock::text(format!("[unpersisted image {mime_type} omitted]"))
                    }
                })
                .collect(),
            status: self.status,
        }
    }
}

pub fn inline_content_display_text(content: &[InlineContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            InlineContentBlock::Text { text } => text.clone(),
            InlineContentBlock::Image { mime_type, .. } => format!("[image {mime_type}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_false(value: &bool) -> bool {
    !value
}

impl UserMessage {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            replayed_after_compaction: false,
        }
    }

    pub fn from_parts(content: Vec<ContentBlock>) -> Self {
        Self {
            content,
            replayed_after_compaction: false,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self.content.as_slice() {
            [ContentBlock::Text { text }] => Some(text.as_str()),
            _ => None,
        }
    }

    pub fn display_text(&self) -> String {
        content_display_text(&self.content)
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
    Image { artifact_id: ImageArtifactId },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image(artifact_id: ImageArtifactId) -> Self {
        Self::Image { artifact_id }
    }

    pub fn display_text(&self) -> String {
        match self {
            Self::Text { text } => text.clone(),
            Self::Image { artifact_id } => format!("[image {artifact_id}]"),
        }
    }
}

pub fn content_display_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(ContentBlock::display_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageArtifactId(String);

impl ImageArtifactId {
    pub const PREFIX: &'static str = "sha256:";

    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let digest = value
            .strip_prefix(Self::PREFIX)
            .ok_or_else(|| "image artifact id must start with `sha256:`".to_string())?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(
                "image artifact id digest must be 64 lowercase hexadecimal characters".to_string(),
            );
        }
        Ok(Self(value))
    }

    pub fn from_sha256_hex(digest: &str) -> Result<Self, String> {
        Self::parse(format!("{}{}", Self::PREFIX, digest))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageArtifactId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ImageArtifactId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ImageArtifactId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InlineContentBlock {
    Text { text: String },
    Image { mime_type: String, data: String },
}

impl InlineContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image(mime_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Image {
            mime_type: mime_type.into(),
            data: data.into(),
        }
    }
}

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

    pub fn text(&self) -> String {
        self.items
            .iter()
            .filter_map(|item| match item {
                AssistantItem::Text(text) => Some(text.as_str()),
                AssistantItem::ToolCall(_) => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantItem {
    Text(String),
    ToolCall(ToolCall),
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

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "type" => kind = Some(map.next_value()?),
                "text" => text = Some(map.next_value()?),
                "id" => id = Some(map.next_value()?),
                "tool_name" => tool_name = Some(map.next_value()?),
                "args_json" => args_json = Some(map.next_value()?),
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
            Some(other) => Err(de::Error::unknown_variant(other, &["text", "tool_call"])),
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

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
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
    pub content: Vec<ContentBlock>,
    pub status: ToolResultStatus,
}

impl ToolResultMessage {
    pub fn success(
        tool_call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self::success_content(tool_call_id, tool_name, vec![ContentBlock::text(output)])
    }

    pub fn success_content(
        tool_call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        content: Vec<ContentBlock>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content,
            status: ToolResultStatus::Success,
        }
    }

    pub fn error(
        tool_call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self::error_content(tool_call_id, tool_name, vec![ContentBlock::text(output)])
    }

    pub fn error_content(
        tool_call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        content: Vec<ContentBlock>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content,
            status: ToolResultStatus::Error,
        }
    }

    pub fn interrupted(tool_call_id: impl Into<ToolCallId>, tool_name: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::text("interrupted")],
            status: ToolResultStatus::Interrupted,
        }
    }

    pub fn crashed(tool_call_id: impl Into<ToolCallId>, tool_name: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::text(
                "crashed before tool result was recorded",
            )],
            status: ToolResultStatus::Crashed,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self.content.as_slice() {
            [ContentBlock::Text { text }] => Some(text.as_str()),
            _ => None,
        }
    }

    pub fn display_text(&self) -> String {
        content_display_text(&self.content)
    }

    pub fn into_inline_text(self) -> InlineToolResultMessage {
        let Self {
            tool_call_id,
            tool_name,
            content,
            status,
        } = self;
        let content = content
            .into_iter()
            .map(|block| match block {
                ContentBlock::Text { text } => Some(InlineContentBlock::Text { text }),
                ContentBlock::Image { .. } => None,
            })
            .collect::<Option<Vec<_>>>();

        match content {
            Some(content) => InlineToolResultMessage {
                tool_call_id,
                tool_name,
                content,
                status,
            },
            None => InlineToolResultMessage::error(
                tool_call_id,
                tool_name,
                "durable image result cannot cross this text-only inline bridge",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_message_compaction_replay_marker_is_backward_compatible() {
        let ordinary: UserMessage = serde_json::from_value(json!({
            "content": [{ "type": "text", "text": "hello" }]
        }))
        .expect("old user message deserializes");
        assert!(!ordinary.replayed_after_compaction);
        assert_eq!(
            serde_json::to_value(&ordinary).expect("ordinary user message serializes"),
            json!({ "content": [{ "type": "text", "text": "hello" }] })
        );

        let mut replayed = ordinary;
        replayed.replayed_after_compaction = true;
        assert_eq!(
            serde_json::to_value(&replayed).expect("replayed user message serializes"),
            json!({
                "content": [{ "type": "text", "text": "hello" }],
                "replayed_after_compaction": true
            })
        );
    }

    #[test]
    fn artifact_image_and_tool_result_serialize_new_shapes() {
        let artifact_id = ImageArtifactId::parse(format!("sha256:{}", "a".repeat(64)))
            .expect("valid artifact id");
        let image = ContentBlock::image(artifact_id);
        assert_eq!(
            serde_json::to_value(&image).expect("image serializes"),
            json!({
                "type": "image",
                "artifact_id": format!("sha256:{}", "a".repeat(64))
            })
        );

        let result = ToolResultMessage::success("call_1", "Bash", "hello");
        assert_eq!(
            serde_json::to_value(&result).expect("tool result serializes"),
            json!({
                "tool_call_id": "call_1",
                "tool_name": "Bash",
                "content": [{ "type": "text", "text": "hello" }],
                "status": "Success"
            })
        );
        assert_eq!(result.as_text(), Some("hello"));
    }

    #[test]
    fn durable_tool_results_use_an_explicit_text_only_inline_bridge() {
        let text = ToolResultMessage::success("call_1", "Bash", "hello").into_inline_text();
        assert_eq!(
            text,
            InlineToolResultMessage::success("call_1", "Bash", "hello")
        );

        let artifact_id = ImageArtifactId::parse(format!("sha256:{}", "a".repeat(64)))
            .expect("valid artifact id");
        let image = ToolResultMessage::success_content(
            "call_2",
            "ReadImage",
            vec![ContentBlock::image(artifact_id)],
        )
        .into_inline_text();
        assert_eq!(image.status, ToolResultStatus::Error);
        assert_eq!(
            image.display_text(),
            "durable image result cannot cross this text-only inline bridge"
        );
    }

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
                    }
                ]
            })
        );

        let round_trip: AssistantMessage =
            serde_json::from_value(value).expect("assistant message deserializes");
        assert_eq!(round_trip, message);
    }
}
