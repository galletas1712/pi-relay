use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    OpenAi,
    Claude,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Claude => "claude",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai" => Ok(Self::OpenAi),
            "claude" | "anthropic" => Ok(Self::Claude),
            other => Err(format!("unsupported provider kind: {other}")),
        }
    }
}

impl Serialize for ProviderKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Default for ReasoningEffort {
    fn default() -> Self {
        Self::Medium
    }
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            other => Err(format!("unsupported reasoning effort: {other}")),
        }
    }
}

impl Serialize for ReasoningEffort {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReasoningEffort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<Value>,
}

impl ProviderConfig {
    /// The configured prompt-cache key, if any. Callers append their own scope
    /// suffix (e.g. `:compaction`) where needed.
    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.prompt_cache.as_ref()?.get("key")?.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderReplayItem {
    pub provider: ProviderKind,
    pub raw_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ReplayDisplay>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayDisplay {
    pub kind: ReplayDisplayKind,
    pub pretty_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_summary: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayDisplayKind {
    LocalTool,
    HostedTool,
}

impl ProviderReplayItem {
    pub fn new(provider: ProviderKind, raw: &Value) -> Result<Self, serde_json::Error> {
        Self::new_with_display(provider, raw, None)
    }

    pub fn new_with_display(
        provider: ProviderKind,
        raw: &Value,
        display: Option<ReplayDisplay>,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            provider,
            raw_json: serde_json::to_string(raw)?,
            display,
        })
    }

    pub fn raw_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.raw_json)
    }

    pub fn raw_type(&self) -> Option<String> {
        self.raw_value().ok().and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_kind_accepts_legacy_anthropic_alias() {
        let config: ProviderConfig = serde_json::from_value(json!({
            "kind": "anthropic",
            "model": "claude-sonnet-4-5",
        }))
        .expect("legacy provider kind should deserialize");

        assert_eq!(config.kind, ProviderKind::Claude);
        assert_eq!(config.reasoning_effort, ReasoningEffort::Medium);
        assert_eq!(serde_json::to_value(config.kind).unwrap(), json!("claude"));
    }

    #[test]
    fn provider_kind_does_not_accept_codex_as_provider_name() {
        let error = serde_json::from_value::<ProviderConfig>(json!({
            "kind": "codex",
            "model": "gpt-5.5",
        }))
        .expect_err("codex is an auth transport, not a provider name");

        assert!(error.to_string().contains("unsupported provider kind"));
    }

    #[test]
    fn provider_replay_display_is_explicit() {
        let replay = ProviderReplayItem::new_with_display(
            ProviderKind::Claude,
            &json!({
                "type": "server_tool_use",
                "id": "srv_1",
                "name": "web_fetch",
                "input": { "url": "https://example.com" },
            }),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::HostedTool,
                pretty_name: "Web fetch".to_string(),
                input_summary: Some("https://example.com".to_string()),
            }),
        )
        .unwrap();
        assert_eq!(
            replay.display,
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::HostedTool,
                pretty_name: "Web fetch".to_string(),
                input_summary: Some("https://example.com".to_string()),
            })
        );
    }
}
