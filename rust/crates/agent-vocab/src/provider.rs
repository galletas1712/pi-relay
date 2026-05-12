use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    OpenAi,
    Codex,
    Claude,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    pub fn is_codex(self) -> bool {
        matches!(self, Self::Codex)
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
            "codex" => Ok(Self::Codex),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<Value>,
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
        assert_eq!(serde_json::to_value(config.kind).unwrap(), json!("claude"));
    }
}
