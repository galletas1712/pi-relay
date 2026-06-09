use agent_store::TranscriptEntryBodyMode;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::codec::from_params;
use crate::rpc_views;
use crate::runtime::SessionDriver;
use crate::state::AppState;
use crate::subagents::{subagent_list, subagent_send_for_parent, subagent_spawn};
use crate::types::RpcError;
use crate::workflows::{
    workflow_await, workflow_context_send, workflow_var_read, workflow_var_write,
    workflow_vars_list,
};

const DEFAULT_READ_LIMIT: u32 = 20;
const MAX_READ_LIMIT: u32 = 100;

pub(crate) async fn work_spawn_for_source(
    state: &AppState,
    source_session_id: &str,
    args: Value,
    excluded_action_row_id: Option<&str>,
) -> std::result::Result<Value, RpcError> {
    let mut params = object_args(args)?;
    let kind = params
        .remove("kind")
        .and_then(|value| value.as_str().map(str::to_string));
    if let Some(kind) = kind.as_deref().filter(|kind| *kind != "subagent") {
        return Err(RpcError::new(
            "invalid_params",
            format!("kind must be `subagent`, got `{kind}`"),
        ));
    }

    params.insert("parent_session_id".to_string(), json!(source_session_id));
    if excluded_action_row_id.is_some() {
        crate::subagents::subagent_spawn_for_parent(
            state,
            source_session_id,
            Value::Object(params),
            excluded_action_row_id,
        )
        .await
    } else {
        subagent_spawn(state, Value::Object(params)).await
    }
}

pub(crate) async fn work_await_for_session(
    state: &AppState,
    session_id: &str,
    args: Value,
) -> std::result::Result<Value, RpcError> {
    let mut params = object_args(args)?;
    params.insert("session_id".to_string(), json!(session_id));
    if let Some(value) = params.remove("vars") {
        params.insert("variable_names".to_string(), value);
    }
    if let Some(value) = params.remove("sessions") {
        params.insert("session_ids".to_string(), value);
    }
    if let Some(idle) = params.remove("idle").and_then(|value| value.as_bool()) {
        params.insert(
            "session_condition".to_string(),
            json!(if idle { "idle" } else { "none" }),
        );
    }
    workflow_await(state, Value::Object(params)).await
}

pub(crate) async fn work_write_for_session(
    state: &AppState,
    session_id: &str,
    action_row_id: Option<&str>,
    args: Value,
) -> std::result::Result<Value, RpcError> {
    let mut params = object_args(args)?;
    params.insert("session_id".to_string(), json!(session_id));
    if let Some(value) = params.remove("var") {
        params.insert("name".to_string(), value);
    }
    if let Some(action_row_id) = action_row_id {
        crate::workflows::workflow_var_write_tool(
            state,
            session_id,
            action_row_id,
            Value::Object(params),
        )
        .await
    } else {
        workflow_var_write(state, Value::Object(params)).await
    }
}

pub(crate) async fn work_send_for_session(
    state: &AppState,
    session_id: &str,
    args: Value,
) -> std::result::Result<Value, RpcError> {
    let mut params = object_args(args)?;
    let to = params
        .remove("to")
        .or_else(|| params.remove("child_session_id"))
        .ok_or_else(|| RpcError::new("invalid_params", "to is required"))?;
    params.insert("child_session_id".to_string(), to);
    if let Some(template) = params.remove("template") {
        params.insert("parent_session_id".to_string(), json!(session_id));
        params.insert("template".to_string(), template);
        workflow_context_send(state, Value::Object(params)).await
    } else {
        if let Some(message) = params.remove("message") {
            params.insert(
                "content".to_string(),
                json!([{"type":"text","text": message}]),
            );
        }
        // Keep tool-call shape compatible with the lower-level subagent tool.
        if let Some(content) = params.remove("content") {
            let message = content_text(content)?;
            params.insert("message".to_string(), json!(message));
        }
        subagent_send_for_parent(state, session_id, Value::Object(params)).await
    }
}

pub(crate) async fn work_read_for_session(
    state: &AppState,
    session_id: &str,
    args: Value,
) -> std::result::Result<Value, RpcError> {
    let params: WorkReadParams = from_params(args)?;
    let view = params.view.unwrap_or(WorkReadView::Var);
    match view {
        WorkReadView::Var => read_var(state, session_id, params).await,
        WorkReadView::Vars => read_vars(state, session_id, params).await,
        WorkReadView::Sessions => read_sessions(state, session_id, params).await,
        WorkReadView::Overview => read_session_overview(state, session_id, params).await,
        WorkReadView::Turns | WorkReadView::Recent => {
            read_session_turns(state, session_id, params).await
        }
        WorkReadView::Turn => read_session_turn_detail(state, session_id, params).await,
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkReadView {
    Var,
    Vars,
    Sessions,
    Overview,
    Recent,
    Turns,
    Turn,
}

#[derive(Debug, Deserialize)]
struct WorkReadParams {
    view: Option<WorkReadView>,
    workflow_id: Option<String>,
    var: Option<String>,
    name: Option<String>,
    session_id: Option<String>,
    scope: Option<String>,
    project_id: Option<uuid::Uuid>,
    limit: Option<u32>,
    before_entry_id: Option<String>,
    card_id: Option<String>,
    active_leaf_id: Option<String>,
    start_sequence: Option<i64>,
    end_sequence: Option<i64>,
}

async fn read_var(
    state: &AppState,
    session_id: &str,
    params: WorkReadParams,
) -> std::result::Result<Value, RpcError> {
    let workflow_id = required_param(params.workflow_id, "workflow_id")?;
    let name = params
        .var
        .or(params.name)
        .ok_or_else(|| RpcError::new("invalid_params", "var is required"))?;
    workflow_var_read(
        state,
        json!({
            "session_id": session_id,
            "workflow_id": workflow_id,
            "name": name,
        }),
    )
    .await
}

async fn read_vars(
    state: &AppState,
    session_id: &str,
    params: WorkReadParams,
) -> std::result::Result<Value, RpcError> {
    let workflow_id = required_param(params.workflow_id, "workflow_id")?;
    workflow_vars_list(
        state,
        json!({
            "session_id": session_id,
            "workflow_id": workflow_id,
            "limit": bounded_limit(params.limit),
        }),
    )
    .await
}

async fn read_sessions(
    state: &AppState,
    session_id: &str,
    params: WorkReadParams,
) -> std::result::Result<Value, RpcError> {
    match params.scope.as_deref().unwrap_or("mine") {
        "mine" => {
            let subagents =
                subagent_list(state, json!({ "parent_session_id": session_id })).await?;
            Ok(json!({
                "session_id": session_id,
                "scope": "mine",
                "subagents": subagents["subagents"].clone(),
            }))
        }
        "project" => {
            let config = state
                .repo
                .load_session_config(session_id)
                .await
                .map_err(anyhow::Error::from)?;
            let project_id = params.project_id.or(config.project_id);
            let sessions = state
                .repo
                .list_sessions(project_id, i64::from(bounded_limit(params.limit)))
                .await
                .map_err(anyhow::Error::from)?;
            Ok(json!({
                "session_id": session_id,
                "scope": "project",
                "project_id": project_id,
                "sessions": sessions.into_iter().map(rpc_views::session_summary).collect::<Vec<_>>(),
            }))
        }
        other => Err(RpcError::new(
            "invalid_params",
            format!("scope must be `mine` or `project`, got `{other}`"),
        )),
    }
}

async fn read_session_overview(
    state: &AppState,
    requester_session_id: &str,
    params: WorkReadParams,
) -> std::result::Result<Value, RpcError> {
    let target_session_id = required_param(params.session_id, "session_id")?;
    ensure_read_allowed(state, requester_session_id, &target_session_id).await?;
    let driver = SessionDriver::acquire(state, &target_session_id).await;
    driver.recover_if_needed().await?;
    let snapshot = state
        .repo
        .session_snapshot(&target_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let relationship = state
        .repo
        .session_relationship_for_child(&target_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(json!({
        "session": rpc_views::session_snapshot(snapshot, None),
        "relationship": relationship.as_ref().map(rpc_views::session_relationship),
        "access": "read_only",
    }))
}

async fn read_session_turns(
    state: &AppState,
    requester_session_id: &str,
    params: WorkReadParams,
) -> std::result::Result<Value, RpcError> {
    let target_session_id = required_param(params.session_id, "session_id")?;
    ensure_read_allowed(state, requester_session_id, &target_session_id).await?;
    let driver = SessionDriver::acquire(state, &target_session_id).await;
    driver.recover_if_needed().await?;
    let result = state
        .repo
        .transcript_turns(
            &target_session_id,
            params.before_entry_id.as_deref(),
            Some(i64::from(bounded_limit(params.limit))),
        )
        .await
        .map(rpc_views::transcript_turns)
        .map_err(anyhow::Error::from)?;
    Ok(json!({
        "session_id": target_session_id,
        "view": "turns",
        "access": "read_only",
        "transcript": result,
    }))
}

async fn read_session_turn_detail(
    state: &AppState,
    requester_session_id: &str,
    params: WorkReadParams,
) -> std::result::Result<Value, RpcError> {
    let target_session_id = required_param(params.session_id, "session_id")?;
    ensure_read_allowed(state, requester_session_id, &target_session_id).await?;
    let card_id = required_param(params.card_id, "card_id")?;
    let active_leaf_id = required_param(params.active_leaf_id, "active_leaf_id")?;
    let start_sequence = params
        .start_sequence
        .ok_or_else(|| RpcError::new("invalid_params", "start_sequence is required"))?;
    let end_sequence = params
        .end_sequence
        .ok_or_else(|| RpcError::new("invalid_params", "end_sequence is required"))?;
    let driver = SessionDriver::acquire(state, &target_session_id).await;
    driver.recover_if_needed().await?;
    let result = state
        .repo
        .transcript_turn_detail(
            &target_session_id,
            &card_id,
            &active_leaf_id,
            start_sequence,
            end_sequence,
            TranscriptEntryBodyMode::Ui,
        )
        .await
        .map(rpc_views::transcript_turn_detail)
        .map_err(anyhow::Error::from)?;
    Ok(json!({
        "session_id": target_session_id,
        "view": "turn",
        "access": "read_only",
        "turn": result,
    }))
}

async fn ensure_read_allowed(
    state: &AppState,
    requester_session_id: &str,
    target_session_id: &str,
) -> std::result::Result<(), RpcError> {
    if requester_session_id == target_session_id {
        return Ok(());
    }
    let requester_config = state
        .repo
        .load_session_config(requester_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let target_config = state
        .repo
        .load_session_config(target_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let target_relationship = state
        .repo
        .session_relationship_for_child(target_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(relationship) = &target_relationship {
        if relationship.parent_session_id == requester_session_id {
            return Ok(());
        }
        return Err(RpcError::new(
            "access_denied",
            "subagents are readable only by their parent",
        ));
    }
    if target_config
        .metadata
        .get("hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(RpcError::new(
            "access_denied",
            "hidden sessions are not readable from this session",
        ));
    }
    if requester_config.project_id == target_config.project_id {
        return Ok(());
    }
    Err(RpcError::new(
        "access_denied",
        "sessions are readable only within the same project or parent/root lineage",
    ))
}

fn object_args(value: Value) -> std::result::Result<serde_json::Map<String, Value>, RpcError> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(RpcError::new(
            "invalid_params",
            "workflow tool arguments must be a JSON object",
        )),
    }
}

fn required_param(value: Option<String>, field: &str) -> std::result::Result<String, RpcError> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RpcError::new("invalid_params", format!("{field} is required")))
}

fn bounded_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_READ_LIMIT).clamp(1, MAX_READ_LIMIT)
}

fn content_text(value: Value) -> std::result::Result<String, RpcError> {
    match value {
        Value::String(message) => Ok(message),
        Value::Array(items) => {
            let mut text = String::new();
            for item in items {
                let Some(item_text) = item.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(item_text);
            }
            if text.is_empty() {
                Err(RpcError::new(
                    "invalid_params",
                    "content must contain at least one text item",
                ))
            } else {
                Ok(text)
            }
        }
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RpcError::new("invalid_params", "content.text is required")),
        _ => Err(RpcError::new(
            "invalid_params",
            "message or textual content is required",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_limit_clamps() {
        assert_eq!(bounded_limit(None), DEFAULT_READ_LIMIT);
        assert_eq!(bounded_limit(Some(0)), 1);
        assert_eq!(bounded_limit(Some(10)), 10);
        assert_eq!(bounded_limit(Some(1000)), MAX_READ_LIMIT);
    }

    #[test]
    fn content_text_extracts_text_items() {
        assert_eq!(content_text(json!("hello")).unwrap(), "hello");
        assert_eq!(
            content_text(json!([
                {"type":"text","text":"one"},
                {"type":"text","text":"two"}
            ]))
            .unwrap(),
            "one\ntwo"
        );
    }
}
