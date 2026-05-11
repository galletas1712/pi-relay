#![forbid(unsafe_code)]

mod postgres;

use std::fmt;
use std::str::FromStr;

use agent_session::{SessionAction, UserMessage};
pub use agent_session::{StoredSession, StoredTranscriptEntry};
pub use postgres::PostgresAgentStore;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

macro_rules! text_enum {
    ($(
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($variant:ident => $wire:literal),+ $(,)?
        }
    )+) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
            pub enum $name {
                $($variant),+
            }

            impl $name {
                pub fn as_str(self) -> &'static str {
                    match self {
                        $(Self::$variant => $wire),+
                    }
                }
            }

            impl fmt::Display for $name {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str(self.as_str())
                }
            }

            impl FromStr for $name {
                type Err = String;

                fn from_str(value: &str) -> Result<Self, Self::Err> {
                    match value {
                        $($wire => Ok(Self::$variant),)+
                        other => Err(format!(
                            "unknown {}: {other}",
                            stringify!($name),
                        )),
                    }
                }
            }

            impl Serialize for $name {
                fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                where
                    S: Serializer,
                {
                    serializer.serialize_str(self.as_str())
                }
            }

            impl<'de> Deserialize<'de> for $name {
                fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                where
                    D: Deserializer<'de>,
                {
                    let value = String::deserialize(deserializer)?;
                    Self::from_str(&value).map_err(D::Error::custom)
                }
            }
        )+
    };
}

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

text_enum! {
    pub enum InputPriority {
        FollowUp => "follow_up",
        Steer => "steer",
    }

    pub enum QueuedInputStatus {
        Queued => "queued",
        Consuming => "consuming",
        Consumed => "consumed",
        Cancelled => "cancelled",
    }

    pub enum ActionKind {
        Model => "model",
        Tool => "tool",
        Compaction => "compaction",
        Cancel => "cancel",
    }

    pub enum ActionStatus {
        Pending => "pending",
        Running => "running",
        Completed => "completed",
        Error => "error",
        Interrupted => "interrupted",
        Stale => "stale",
    }

    pub enum SessionActivity {
        Idle => "idle",
        Queued => "queued",
        Running => "running",
    }

    pub enum EventType {
        SessionCreated => "session.created",
        SessionConfigured => "session.configured",
        SessionRecovered => "session.recovered",
        SessionIdle => "session.idle",
        SessionWorkCancelled => "session.work_cancelled",
        InputQueued => "input.queued",
        InputPromoted => "input.promoted",
        InputReplaced => "input.replaced",
        InputCancelled => "input.cancelled",
        InputConsumed => "input.consumed",
        InputAccepted => "input.accepted",
        InputIgnored => "input.ignored",
        HistoryRewound => "history.rewound",
        HistoryForked => "history.forked",
        HistoryCompacted => "history.compacted",
        ActionRequested => "action.requested",
        ModelRequested => "model.requested",
        ModelCompleted => "model.completed",
        ModelError => "model.error",
        ToolRequested => "tool.requested",
        ToolStarted => "tool.started",
        ToolCompleted => "tool.completed",
        ToolError => "tool.error",
        CompactionRequested => "compaction.requested",
        CompactionCompleted => "compaction.completed",
        CompactionError => "compaction.error",
        TranscriptAppended => "transcript.appended",
        TurnStarted => "turn.started",
        TurnFinished => "turn.finished",
        AssistantMessage => "assistant.message",
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

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub provider: ProviderConfig,
    pub metadata: Value,
}

impl SessionConfig {
    pub fn harness(&self) -> bool {
        self.metadata
            .get("harness")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventFrame {
    pub event_id: i64,
    pub event: EventType,
    pub session_id: String,
    pub data: Value,
}

#[derive(Debug, Clone)]
pub struct ActionUpdate {
    pub row_id: String,
    pub attempt_id: String,
    pub status: ActionStatus,
    pub result: Value,
}

#[derive(Debug, Clone)]
pub struct DispatchAction {
    pub row_id: String,
    pub attempt_id: String,
    pub action: SessionAction,
    pub config: SessionConfig,
}

pub struct EnqueueUserInputResult {
    pub input_id: String,
    pub event: Option<EventFrame>,
}

#[derive(Debug, Clone)]
pub struct InputRecord {
    pub input_id: String,
    pub status: QueuedInputStatus,
}

#[derive(Debug, Clone)]
pub struct AcceptedInput {
    pub priority: InputPriority,
    pub content: UserMessage,
    pub client_input_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueuedInput {
    pub id: String,
    pub priority: InputPriority,
    pub content: UserMessage,
    pub client_input_id: Option<String>,
    pub claim_id: String,
}

#[derive(Debug, Clone)]
pub struct StoredAction {
    pub kind: ActionKind,
    pub action_id: i64,
    pub turn_id: Option<i64>,
    pub attempt_id: String,
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

    #[test]
    fn input_priority_round_trips_as_wire_string() {
        assert_eq!(
            serde_json::to_value(InputPriority::FollowUp).unwrap(),
            json!("follow_up")
        );
        assert_eq!(
            serde_json::from_value::<InputPriority>(json!("steer")).unwrap(),
            InputPriority::Steer
        );
    }

    #[test]
    fn invalid_storage_vocab_is_rejected() {
        let error = serde_json::from_value::<ActionStatus>(json!("done"))
            .expect_err("invalid action status should fail");

        assert!(error.to_string().contains("unknown ActionStatus"));
    }
}
