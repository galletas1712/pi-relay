use agent_session::{StoredSession, TranscriptStore, TranscriptStoreError};
use agent_vocab::{AssistantMessage, ContentBlock, UserMessage};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::types::RpcError;

pub(crate) fn parse_user_message(value: Value) -> std::result::Result<UserMessage, RpcError> {
    let content: Vec<ContentBlock> = serde_json::from_value(value)
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))?;
    Ok(UserMessage::from_parts(content))
}

pub(crate) fn required_uuid(params: &Value, key: &str) -> std::result::Result<Uuid, RpcError> {
    let value = required_string(params, key)?;
    Uuid::parse_str(&value)
        .map_err(|error| RpcError::new("invalid_params", format!("{key} must be a UUID: {error}")))
}

pub(crate) fn parse_assistant_message(
    value: Value,
) -> std::result::Result<AssistantMessage, RpcError> {
    serde_json::from_value(value)
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))
}

pub(crate) fn from_params<T: for<'de> Deserialize<'de>>(
    params: Value,
) -> std::result::Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))
}

pub(crate) fn required_string(params: &Value, key: &str) -> std::result::Result<String, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RpcError::new("invalid_params", format!("{key} is required")))
}

pub(crate) fn transcript_store_from_stored(
    stored: &StoredSession,
) -> std::result::Result<TranscriptStore, RpcError> {
    let entries = stored
        .entries
        .iter()
        .cloned()
        .map(Into::into)
        .collect::<Vec<_>>();
    TranscriptStore::from_storage_entries(entries, stored.active_leaf_id.clone()).map_err(|error| {
        match error {
            TranscriptStoreError::NotTurnBoundary => {
                RpcError::new("not_turn_boundary", "target is not a turn boundary")
            }
            other => RpcError::new("invalid_transcript", format!("{other:?}")),
        }
    })
}
