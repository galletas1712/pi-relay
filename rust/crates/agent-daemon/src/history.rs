use std::time::Instant;

use agent_store::{HistoryTarget, SwitchActiveLeafRequest};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::codec::from_params;
use crate::rpc_views;
use crate::runtime::{
    clear_event_buffer_after_commit, map_source_mutation_error, publish_events, SessionDriver,
};
use crate::state::AppState;
use crate::types::RpcError;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RequiredNullableString {
    String(String),
    Null(()),
}

impl RequiredNullableString {
    fn into_option(self) -> Option<String> {
        match self {
            Self::String(value) => Some(value),
            Self::Null(()) => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct HistoryTargetParams {
    session_id: String,
    leaf_id: RequiredNullableString,
    #[serde(default, deserialize_with = "deserialize_present_nullable")]
    expected_active_leaf_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    expected_transcript_revision: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    active_branch_entry_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct SwitchOnlyParams {
    #[serde(default, deserialize_with = "deserialize_present")]
    return_active_branch: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_present")]
    missing_body_ids: Option<Vec<String>>,
}

#[derive(Debug)]
pub(crate) struct ParsedHistoryTarget {
    pub(crate) session_id: String,
    pub(crate) leaf_id: Option<String>,
    pub(crate) expected_active_leaf_id: Option<Option<String>>,
    pub(crate) expected_transcript_revision: Option<i64>,
    pub(crate) active_branch_entry_ids: Option<Vec<String>>,
}

impl ParsedHistoryTarget {
    pub(crate) fn as_store_target(&self) -> HistoryTarget<'_> {
        HistoryTarget {
            leaf_id: self.leaf_id.as_deref(),
            expected_active_leaf_id: self
                .expected_active_leaf_id
                .as_ref()
                .map(|expected| expected.as_deref()),
            expected_transcript_revision: self.expected_transcript_revision,
            expected_active_branch_entry_ids: self.active_branch_entry_ids.as_deref(),
        }
    }
}

pub(crate) fn parse_history_target(
    params: &Value,
    operation_fields: &[&str],
) -> Result<ParsedHistoryTarget, RpcError> {
    let object = params
        .as_object()
        .ok_or_else(|| RpcError::new("invalid_params", "params must be an object"))?;
    const COMMON_FIELDS: &[&str] = &[
        "session_id",
        "leaf_id",
        "expected_active_leaf_id",
        "expected_transcript_revision",
        "active_branch_entry_ids",
    ];
    // Common and operation-specific fields share one flat wire object, so
    // validate their union before deserializing the common view.
    if let Some(field) = object.keys().find(|field| {
        !COMMON_FIELDS.contains(&field.as_str()) && !operation_fields.contains(&field.as_str())
    }) {
        return Err(RpcError::new(
            "invalid_params",
            format!("unknown field `{field}`"),
        ));
    }
    let params: HistoryTargetParams = from_params(params.clone())?;
    Ok(ParsedHistoryTarget {
        session_id: params.session_id,
        leaf_id: params.leaf_id.into_option(),
        expected_active_leaf_id: params.expected_active_leaf_id,
        expected_transcript_revision: params.expected_transcript_revision,
        active_branch_entry_ids: params.active_branch_entry_ids,
    })
}

pub(crate) async fn prepare_history_target(
    state: &AppState,
    target: &ParsedHistoryTarget,
) -> Result<(), RpcError> {
    let active_leaf_id = state.repo.active_leaf_id(&target.session_id).await?;
    match &target.expected_active_leaf_id {
        Some(expected) if active_leaf_id.as_ref() != expected.as_ref() => {
            return Err(RpcError::new(
                "history_changed",
                "session active leaf changed before the request was applied",
            ));
        }
        None | Some(_) => {}
    }
    if !state
        .repo
        .transcript_leaf_is_turn_boundary(&target.session_id, target.leaf_id.as_deref())
        .await?
    {
        return Err(RpcError::new(
            "not_turn_boundary",
            "history operation requires a turn boundary",
        ));
    }
    Ok(())
}

pub(crate) async fn ensure_history_source_idle(
    state: &AppState,
    driver: &SessionDriver,
    session_id: &str,
) -> Result<(), RpcError> {
    driver.ensure_idle_for_source_mutation().await?;
    if state.repo.parent_has_running_delegation(session_id).await? {
        return Err(RpcError::new(
            "session_busy",
            "history operations require all delegations to be idle",
        ));
    }
    Ok(())
}

pub(crate) async fn switch(state: &AppState, params: Value) -> Result<Value, RpcError> {
    let target = parse_history_target(&params, &["return_active_branch", "missing_body_ids"])?;
    let switch: SwitchOnlyParams = from_params(params)?;
    let started_at = Instant::now();
    let driver = SessionDriver::acquire(state, &target.session_id).await;
    let acquired_ms = started_at.elapsed().as_millis();
    ensure_history_source_idle(state, &driver, &target.session_id).await?;
    let idle_ms = started_at.elapsed().as_millis();
    prepare_history_target(state, &target).await?;
    let boundary_ms = started_at.elapsed().as_millis();
    let return_active_branch = switch.return_active_branch.unwrap_or(false);
    let result = state
        .repo
        .switch_active_leaf(SwitchActiveLeafRequest {
            session_id: &target.session_id,
            target: target.as_store_target(),
            return_active_branch,
            missing_body_ids: switch.missing_body_ids.as_deref(),
        })
        .await
        .map_err(map_source_mutation_error)?;
    let switch_ms = started_at.elapsed().as_millis();
    let returned_body_count = result
        .active_branch_entries
        .as_ref()
        .map(Vec::len)
        .unwrap_or_default();
    let returned_id_count = result
        .active_branch_entry_ids
        .as_ref()
        .map(Vec::len)
        .unwrap_or_default();
    publish_events(state, result.events.clone());
    clear_event_buffer_after_commit(state, &target.session_id, "history.switch").await;
    let publish_ms = started_at.elapsed().as_millis();
    let value = rpc_views::switch_active_leaf(result);
    let total_ms = started_at.elapsed().as_millis();
    if crate::perf_logging_enabled() {
        eprintln!(
            "perf history.switch session={} leaf_id={:?} return_active_branch={return_active_branch} branch_ids={returned_id_count} bodies={returned_body_count} acquire_ms={acquired_ms} idle_ms={} boundary_ms={} switch_ms={} publish_ms={} view_ms={} total_ms={total_ms}",
            target.session_id,
            target.leaf_id,
            idle_ms.saturating_sub(acquired_ms),
            boundary_ms.saturating_sub(idle_ms),
            switch_ms.saturating_sub(boundary_ms),
            publish_ms.saturating_sub(switch_ms),
            total_ms.saturating_sub(publish_ms),
        );
    }
    Ok(value)
}

fn deserialize_present_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    T::deserialize(deserializer).map(Some)
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
