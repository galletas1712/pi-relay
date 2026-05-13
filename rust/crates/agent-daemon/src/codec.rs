use std::time::{SystemTime, UNIX_EPOCH};

use agent_session::{
    ModelContext, StoredSession, TranscriptStorageNode, TranscriptStore, TranscriptStoreError,
};
use agent_vocab::{AssistantMessage, ContentBlock, TranscriptItem, UserMessage};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::types::RpcError;

pub(crate) fn parse_user_message(value: Value) -> std::result::Result<UserMessage, RpcError> {
    let content: Vec<ContentBlock> = serde_json::from_value(value)
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))?;
    Ok(UserMessage::from_parts(content))
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

pub(crate) fn recover_fork_branch_tail(
    mut branch: Vec<TranscriptStorageNode>,
) -> Vec<TranscriptStorageNode> {
    let items = branch
        .iter()
        .map(|entry| entry.item.clone())
        .collect::<Vec<_>>();
    let original_len = items.len();
    let recovered = ModelContext::from_transcript_items_closing_open_turn_as_interrupted(items)
        .into_transcript_items();
    let mut parent_id = branch.last().map(|entry| entry.id.clone());
    for item in recovered.into_iter().skip(original_len) {
        let node = TranscriptStorageNode {
            id: Uuid::new_v4().to_string(),
            parent_id: parent_id.clone(),
            timestamp_ms: now_ms(),
            item,
            provider_replay: Vec::new(),
        };
        parent_id = Some(node.id.clone());
        branch.push(node);
    }
    branch
}

pub(crate) fn fork_branch_before_user_message(
    store: &TranscriptStore,
    user_leaf_id: &str,
) -> Vec<TranscriptStorageNode> {
    let branch = store.path_entries_to(user_leaf_id);
    let previous_boundary_id = branch
        .iter()
        .rev()
        .skip(1)
        .find_map(|entry| match entry.item {
            TranscriptItem::TurnFinished { .. } => Some(entry.id.as_str()),
            _ => None,
        });
    previous_boundary_id
        .map(|leaf_id| store.path_entries_to(leaf_id))
        .unwrap_or_default()
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}
