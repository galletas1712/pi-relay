use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Opaque provider identifier (e.g. `"openai"`, `"claude"`, `"openai-codex"`,
/// `"nvidia-inference"`).  The daemon treats this as an opaque string —
/// provider-specific resolution happens in the pi-ai sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderKind(pub String);

impl ProviderKind {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Convenience constructor for the OpenAI provider.
    pub fn openai() -> Self {
        Self("openai".to_string())
    }

    /// Convenience constructor for the Anthropic/Claude provider.
    pub fn claude() -> Self {
        Self("claude".to_string())
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderKind {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.to_string()))
    }
}

impl From<String> for ProviderKind {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ProviderKind {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl AsRef<str> for ProviderKind {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for ProviderKind {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ProviderKind {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
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
        String::deserialize(deserializer).map(Self)
    }
}

text_enum! {
    #[derive(Default)]
    pub enum ReasoningEffort {
        None => "none",
        Minimal => "minimal",
        Low => "low",
        #[default]
        Medium => "medium",
        High => "high",
        XHigh => "xhigh",
        Max => "max",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub provider: ProviderKind,
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
    fn provider_config_rejects_catalog_only_ultra_reasoning_effort() {
        let error = serde_json::from_value::<ProviderConfig>(json!({
            "provider": "openai",
            "model": "gpt-5.6-sol",
            "reasoning_effort": "ultra",
        }))
        .expect_err("catalog-only Codex metadata is not a public wire effort");

        assert!(error.to_string().contains("unknown ReasoningEffort: ultra"));
    }

    #[test]
    fn provider_config_accepts_arbitrary_provider_strings() {
        let config = serde_json::from_value::<ProviderConfig>(json!({
            "provider": "nvidia-inference",
            "model": "nvidia/zai-org/glm-5.2",
        }))
        .expect("arbitrary provider string deserializes");
        assert_eq!(config.provider.as_str(), "nvidia-inference");
        assert_eq!(config.model, "nvidia/zai-org/glm-5.2");
    }

    #[test]
    fn provider_config_accepts_openai_codex_provider() {
        let config = serde_json::from_value::<ProviderConfig>(json!({
            "provider": "openai-codex",
            "model": "gpt-5.6-sol",
        }))
        .expect("openai-codex provider deserializes");
        assert_eq!(config.provider.as_str(), "openai-codex");
    }

    #[test]
    fn provider_kind_serializes_as_plain_string() {
        let kind = ProviderKind::new("anthropic");
        let value = serde_json::to_value(&kind).unwrap();
        assert_eq!(value, json!("anthropic"));
        let round_trip: ProviderKind = serde_json::from_value(value).unwrap();
        assert_eq!(round_trip, kind);
    }

    #[test]
    fn provider_replay_display_is_explicit() {
        let replay = ProviderReplayItem::new_with_display(
            ProviderKind::claude(),
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
