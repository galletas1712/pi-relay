#![forbid(unsafe_code)]

mod auth;
mod codec;
mod config;
mod provider_runtime;
mod rpc_views;
mod runtime;
mod state;
mod types;

use crate::codec::{
    fork_branch_before_user_message, from_params, parse_assistant_message, parse_user_message,
    recover_fork_branch_tail, required_string, transcript_store_from_stored,
};
use crate::config::Config;
use crate::runtime::*;
use crate::state::AppState;
use crate::types::*;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex as StdMutex};

use agent_core::AgentInput;
use agent_session::AgentSession;
use agent_store::{
    AcceptedInput, ActionKind, ActionStatus, ActionUpdate, EventFrame, EventType, InputPriority,
    PostgresAgentStore, QueuedInputStatus, SessionConfig,
};
use agent_tools::{ToolContext, ToolRegistry};
use agent_vocab::{ActionId, ProviderConfig, ProviderKind, TranscriptItem, TurnId, TurnOutcome};
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env_and_args()?;
    let repo = Arc::new(PostgresAgentStore::connect(&config.database_url).await?);
    repo.migrate().await?;
    let stale_actions = repo.mark_all_unfinished_actions_stale().await?;
    if stale_actions > 0 {
        eprintln!("marked {stale_actions} abandoned action(s) stale");
    }

    let (events, _) = broadcast::channel(1024);
    let state = AppState {
        repo,
        active: Arc::new(Mutex::new(HashMap::new())),
        session_driver_locks: Arc::new(Mutex::new(HashMap::new())),
        dispatch_tasks: Arc::new(StdMutex::new(Vec::new())),
        events,
        tools: Arc::new(ToolRegistry::with_builtin_tools()),
        tool_context: ToolContext::new(config.workspace),
    };

    let listener = TcpListener::bind(&config.bind).await?;
    println!("pi-agentd listening on ws://{}", config.bind);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_socket(state, stream).await {
                        eprintln!("websocket error: {error:#}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
        }
    }

    drain_dispatch_tasks(&state).await;
    state.repo.close().await;
    Ok(())
}

async fn drain_dispatch_tasks(state: &AppState) {
    let handles = std::mem::take(
        &mut *state
            .dispatch_tasks
            .lock()
            .expect("dispatch task list lock poisoned"),
    );
    if handles.is_empty() {
        return;
    }
    let drain = async {
        for handle in handles {
            if let Err(error) = handle.await {
                eprintln!("dispatch task join error: {error}");
            }
        }
    };
    if timeout(Duration::from_secs(15), drain).await.is_err() {
        eprintln!("timed out waiting for dispatch tasks during shutdown");
    }
}

async fn handle_socket(state: AppState, stream: TcpStream) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut writer, mut reader) = ws.split();
    let mut events_rx = state.events.subscribe();
    let mut subscriptions = BTreeSet::<String>::new();
    let mut event_high_water = BTreeMap::<String, i64>::new();

    loop {
        tokio::select! {
            maybe_msg = reader.next() => {
                let Some(message) = maybe_msg else { break; };
                let message = message?;
                if !message.is_text() {
                    if message.is_close() { break; }
                    continue;
                }
                let text = message.to_text()?;
                let request: RpcRequest = match serde_json::from_str(text) {
                    Ok(request) => request,
                    Err(error) => {
                        let response = RpcResponse {
                            id: Value::Null,
                            ok: false,
                            result: None,
                            error: Some(RpcErrorBody {
                                code: "invalid_json".to_string(),
                                message: error.to_string(),
                                data: json!({}),
                            }),
                        };
                        writer.send(Message::Text(serde_json::to_string(&response)?.into())).await?;
                        continue;
                    }
                };
                let response = match handle_request(&state, &mut subscriptions, &mut event_high_water, request).await {
                    Ok((id, value)) => RpcResponse { id, ok: true, result: Some(value), error: None },
                    Err((id, error)) => RpcResponse { id, ok: false, result: None, error: Some(error) },
                };
                writer.send(Message::Text(serde_json::to_string(&response)?.into())).await?;
            }
            event = events_rx.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        for session_id in subscriptions.clone() {
                            let after = event_high_water.get(&session_id).copied();
                            let missed = state.repo.events_after(&session_id, after).await?;
                            for event in missed {
                                if event.event_id <= event_high_water.get(&session_id).copied().unwrap_or_default() {
                                    continue;
                                }
                                event_high_water.insert(session_id.clone(), event.event_id);
                                writer.send(Message::Text(serde_json::to_string(&event)?.into())).await?;
                            }
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if subscriptions.contains(&event.session_id) {
                    let last_sent = event_high_water
                        .get(&event.session_id)
                        .copied()
                        .unwrap_or_default();
                    if event.event_id <= last_sent {
                        continue;
                    }
                    event_high_water.insert(event.session_id.clone(), event.event_id);
                    writer.send(Message::Text(serde_json::to_string(&event)?.into())).await?;
                }
            }
        }
    }

    Ok(())
}

async fn handle_request(
    state: &AppState,
    subscriptions: &mut BTreeSet<String>,
    event_high_water: &mut BTreeMap<String, i64>,
    request: RpcRequest,
) -> std::result::Result<(Value, Value), (Value, RpcErrorBody)> {
    let id = request.id;
    match dispatch_request(
        state,
        subscriptions,
        event_high_water,
        request.method,
        request.params,
    )
    .await
    {
        Ok(result) => Ok((id, result)),
        Err(error) => Err((
            id,
            RpcErrorBody {
                code: error.code,
                message: error.message,
                data: error.data,
            },
        )),
    }
}

async fn dispatch_request(
    state: &AppState,
    subscriptions: &mut BTreeSet<String>,
    event_high_water: &mut BTreeMap<String, i64>,
    method: String,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let Some(method) = RpcMethod::parse(&method) else {
        return Err(RpcError::new(
            "unknown_method",
            format!("unknown websocket RPC method: {method}"),
        ));
    };
    match method {
        RpcMethod::SessionStart => session_start(state, params).await,
        RpcMethod::SessionList => session_list(state, params).await,
        RpcMethod::SessionGet => session_get(state, params).await,
        RpcMethod::SessionRename => session_rename(state, params).await,
        RpcMethod::SessionConfigure => session_configure(state, params).await,
        RpcMethod::ConfigGet => config_get(state).await,
        RpcMethod::ConfigSet => config_set(state, params).await,
        RpcMethod::EventsSubscribe => {
            events_subscribe(state, subscriptions, event_high_water, params).await
        }
        RpcMethod::EventsUnsubscribe => events_unsubscribe(subscriptions, event_high_water, params),
        RpcMethod::InputFollowUp => input_user(state, params, InputPriority::FollowUp).await,
        RpcMethod::InputPromoteQueued => input_promote_queued(state, params).await,
        RpcMethod::InputInterrupt => input_interrupt(state, params).await,
        RpcMethod::HistoryTree => history_tree(state, params).await,
        RpcMethod::HistoryContext => history_context(state, params).await,
        RpcMethod::HistoryRewind => history_rewind(state, params).await,
        RpcMethod::HistoryFork => history_fork(state, params).await,
        RpcMethod::TurnResume => turn_resume(state, params).await,
        RpcMethod::ToolsList => tools_list(state, params),
        RpcMethod::CompactionRequest => compaction_request(state, params).await,
        RpcMethod::HarnessModelComplete => harness_model_complete(state, params).await,
        RpcMethod::HarnessModelFail => harness_model_fail(state, params).await,
    }
}

fn tools_list(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let provider = required_string(&params, "provider")?;
    let provider = provider.parse::<ProviderKind>().map_err(|error| {
        RpcError::new(
            "invalid_provider",
            format!("invalid provider for tools.list: {error}"),
        )
    })?;
    Ok(json!({ "tools": state.tools.listings_for_provider(provider) }))
}

async fn session_start(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let params: StartSessionParams = from_params(params)?;
    let session_id = params
        .session_id
        .unwrap_or_else(|| format!("session_{}", Uuid::new_v4()));
    let priority = params.priority.unwrap_or(InputPriority::FollowUp);
    let content = parse_user_message(params.content)?;
    let config = SessionConfig {
        provider: params.provider,
        metadata: params.metadata.unwrap_or_else(|| json!({})),
    };

    let driver = SessionDriver::acquire(state, &session_id).await;

    if state
        .repo
        .session_exists(&session_id)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Ok(json!({
            "session_id": session_id,
            "activity": state.repo.activity(&session_id).await.map_err(anyhow::Error::from)?,
            "replayed": true,
        }));
    }

    let mut session = AgentSession::new();
    session
        .enqueue_input(agent_input_from_queued_priority(priority, content.clone()))
        .map_err(|error| RpcError::new("invalid_input", error.to_string()))?;
    let mut runtime = RuntimeSession { session, config };
    let (entries, events, actions, active_leaf_id) = collect_runtime_outputs(&mut runtime);
    let config = runtime.config.clone();
    let (frames, persisted_actions) = state
        .repo
        .start_session_outputs(
            &session_id,
            &config,
            &entries,
            active_leaf_id.as_deref(),
            &events,
            &actions,
            priority,
            &content,
            params.client_input_id.as_deref(),
        )
        .await
        .map_err(anyhow::Error::from)?;

    if frames.is_empty() {
        return Ok(json!({
            "session_id": session_id,
            "activity": state.repo.activity(&session_id).await.map_err(anyhow::Error::from)?,
            "replayed": true,
        }));
    }
    let dispatches = attach_dispatch_config(persisted_actions, &config);

    state
        .active
        .lock()
        .await
        .insert(session_id.clone(), Arc::new(Mutex::new(runtime)));
    publish_events(state, frames);
    driver.dispatch(dispatches);

    Ok(json!({
        "session_id": session_id,
        "activity": state.repo.activity(&session_id).await.map_err(anyhow::Error::from)?,
        "replayed": false,
    }))
}

#[derive(Debug, Deserialize)]
struct StartSessionParams {
    session_id: Option<String>,
    provider: ProviderConfig,
    metadata: Option<Value>,
    client_input_id: Option<String>,
    priority: Option<InputPriority>,
    content: Value,
}

async fn session_list(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let limit = params.get("limit").and_then(Value::as_i64).unwrap_or(50);
    let sessions = state
        .repo
        .list_sessions(limit)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(json!({
        "sessions": sessions
            .into_iter()
            .map(rpc_views::session_summary)
            .collect::<Vec<_>>()
    }))
}

async fn session_get(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let include_entries = params
        .get("include_entries")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let snapshot = state
        .repo
        .session_snapshot(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let entries = if include_entries {
        Some(
            state
                .repo
                .history_tree(&session_id)
                .await
                .map_err(anyhow::Error::from)?
                .entries,
        )
    } else {
        None
    };
    Ok(rpc_views::session_snapshot(snapshot, entries))
}

async fn session_rename(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let title = required_string(&params, "title")?.trim().to_string();
    if title.is_empty() {
        return Err(RpcError::new("invalid_params", "session title is required"));
    }
    let _driver = SessionDriver::acquire(state, &session_id).await;
    let events = state
        .repo
        .rename_session(&session_id, &title)
        .await
        .map_err(anyhow::Error::from)?;
    publish_events(state, events);
    clear_event_buffer_if_idle(state, &session_id).await?;
    Ok(json!({
        "session_id": session_id,
        "title": title,
        "activity": state.repo.activity(&session_id).await.map_err(anyhow::Error::from)?
    }))
}

async fn session_configure(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    let current = state
        .repo
        .load_session_config(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let provider = params
        .get("provider")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))?
        .unwrap_or_else(|| current.provider.clone());
    let metadata = params
        .get("metadata")
        .cloned()
        .unwrap_or_else(|| current.metadata.clone());
    let model_changed = provider_model_changed(&current.provider, &provider);
    let metadata_changed = metadata != current.metadata;
    if model_changed || metadata_changed {
        driver.ensure_idle_for_source_mutation().await?;
    }
    if model_changed {
        let stored = state
            .repo
            .load_stored_session(&session_id)
            .await
            .map_err(anyhow::Error::from)?;
        if !stored.entries.is_empty() {
            return Err(RpcError::new(
                "provider_locked",
                "session model cannot be changed after the first transcript entry",
            ));
        }
    }
    let config = SessionConfig { provider, metadata };
    let events = state
        .repo
        .configure_session(&session_id, &config)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(active) = driver.active_session().await {
        active.lock().await.config = config.clone();
    }
    publish_events(state, events);
    clear_event_buffer_if_idle(state, &session_id).await?;
    Ok(
        json!({ "session_id": session_id, "activity": state.repo.activity(&session_id).await.map_err(anyhow::Error::from)? }),
    )
}

fn provider_model_changed(previous: &ProviderConfig, next: &ProviderConfig) -> bool {
    previous.kind != next.kind || previous.model != next.model
}

async fn config_get(state: &AppState) -> std::result::Result<Value, RpcError> {
    let config = state
        .repo
        .global_config()
        .await
        .map_err(anyhow::Error::from)?;
    Ok(rpc_views::global_config(config))
}

async fn config_set(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let system_prompt = match params.get("system_prompt") {
        Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => {
            return Err(RpcError::new(
                "invalid_params",
                "system_prompt must be a string or null",
            ))
        }
        None => return Err(RpcError::new("invalid_params", "system_prompt is required")),
    };
    state
        .repo
        .set_global_system_prompt(system_prompt.as_deref())
        .await
        .map_err(anyhow::Error::from)?;
    config_get(state).await
}

async fn events_subscribe(
    state: &AppState,
    subscriptions: &mut BTreeSet<String>,
    event_high_water: &mut BTreeMap<String, i64>,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let after_event_id = params.get("after_event_id").and_then(Value::as_i64);
    subscriptions.insert(session_id.clone());
    let Some(after_event_id) = after_event_id else {
        let current = state
            .repo
            .last_event_id(&session_id)
            .await
            .map_err(anyhow::Error::from)?;
        event_high_water.insert(session_id, current);
        return Ok(json!({ "replayed": [] }));
    };
    event_high_water.insert(session_id.clone(), after_event_id);
    let events = state
        .repo
        .events_after(&session_id, Some(after_event_id))
        .await
        .map_err(anyhow::Error::from)?;
    let replayed_max = events
        .iter()
        .map(|event| event.event_id)
        .max()
        .unwrap_or(after_event_id);
    event_high_water.insert(session_id.clone(), replayed_max);
    Ok(json!({ "replayed": events }))
}

fn events_unsubscribe(
    subscriptions: &mut BTreeSet<String>,
    event_high_water: &mut BTreeMap<String, i64>,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    subscriptions.remove(&session_id);
    event_high_water.remove(&session_id);
    Ok(json!({ "session_id": session_id }))
}

async fn input_user(
    state: &AppState,
    params: Value,
    priority: InputPriority,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let client_input_id = params
        .get("client_input_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let content_value = params
        .get("content")
        .cloned()
        .ok_or_else(|| RpcError::new("invalid_params", "content is required"))?;
    let content = parse_user_message(content_value)?;

    enum InputOutcome {
        Accepted(Vec<DispatchAction>),
        Queued {
            input_id: String,
            event: Option<EventFrame>,
            should_drive: bool,
        },
    }

    let outcome = {
        if let Some(client_input_id) = client_input_id.as_deref() {
            if let Some(record) = state
                .repo
                .find_client_input(&session_id, client_input_id)
                .await
                .map_err(anyhow::Error::from)?
            {
                return Ok(json!({
                    "input_id": record.input_id,
                    "accepted": record.status == QueuedInputStatus::Consumed,
                    "queued": matches!(
                        record.status,
                        QueuedInputStatus::Queued | QueuedInputStatus::Consuming
                    ),
                    "replayed": true,
                }));
            }
        }
        let has_running = state
            .repo
            .has_unfinished_actions(&session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let has_queued = state
            .repo
            .has_queued_inputs(&session_id)
            .await
            .map_err(anyhow::Error::from)?;
        if has_running || has_queued {
            let queued = state
                .repo
                .enqueue_user_input(&session_id, priority, &content, client_input_id.as_deref())
                .await
                .map_err(anyhow::Error::from)?;
            InputOutcome::Queued {
                input_id: queued.input_id,
                event: queued.event,
                should_drive: !has_running,
            }
        } else {
            ensure_expected_active_leaf(state, &session_id, &params).await?;
            driver.ensure_active_loaded().await?;
            let active = driver
                .require_active_session("session_not_found", "session not found")
                .await?;
            {
                let mut runtime = active.lock().await;
                runtime
                    .session
                    .enqueue_input(agent_input_from_queued_priority(priority, content.clone()))
                    .map_err(|error| RpcError::new("invalid_input", error.to_string()))?;
            }
            InputOutcome::Accepted(
                driver
                    .persist_active_outputs(
                        active,
                        None,
                        None,
                        Some(AcceptedInput {
                            priority,
                            content: content.clone(),
                            client_input_id: client_input_id.clone(),
                        }),
                        Vec::new(),
                    )
                    .await?,
            )
        }
    };

    match outcome {
        InputOutcome::Accepted(dispatches) => {
            driver.dispatch(dispatches);
            Ok(json!({ "accepted": true, "queued": false }))
        }
        InputOutcome::Queued {
            input_id,
            event,
            should_drive,
        } => {
            if let Some(event) = event {
                publish_events(state, vec![event]);
            }
            if should_drive {
                driver.drive_until_blocked().await?;
            }
            Ok(json!({ "input_id": input_id, "accepted": true, "queued": true }))
        }
    }
}

async fn input_promote_queued(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let input_id = required_string(&params, "input_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let result = state
        .repo
        .promote_queued_input(&session_id, &input_id)
        .await
        .map_err(map_queued_mutation_error)?;
    if let Some(event) = result.event {
        publish_events(state, vec![event]);
    }
    Ok(json!({
        "input_id": result.input_id,
        "priority": result.priority,
        "status": result.status,
        "promoted": result.promoted,
    }))
}

async fn input_interrupt(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let active = driver.active_session().await;
    let Some(active) = active else {
        let event = state
            .repo
            .insert_event(
                &session_id,
                EventType::InputIgnored,
                json!({ "kind": "interrupt" }),
            )
            .await
            .map_err(anyhow::Error::from)?;
        publish_events(state, vec![event]);
        clear_event_buffer_if_idle(state, &session_id).await?;
        return Ok(json!({ "ignored": true }));
    };
    let dispatches = driver
        .apply_agent_input(active, AgentInput::Interrupt, None, None, Vec::new())
        .await?;
    driver.dispatch(dispatches);
    driver.drive_until_blocked().await?;
    Ok(json!({ "interrupted": true }))
}

async fn history_tree(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let tree = state
        .repo
        .history_tree(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(rpc_views::history_tree(tree))
}

async fn history_context(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let leaf_id = params.get("leaf_id").and_then(Value::as_str);
    let stored = state
        .repo
        .load_stored_session(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let store = transcript_store_from_stored(&stored)?;
    let items = leaf_id
        .map(|leaf_id| {
            store
                .path_entries_to(leaf_id)
                .into_iter()
                .map(|entry| entry.item)
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| store.model_context().into_transcript_items());
    Ok(json!({ "items": items }))
}

async fn history_rewind(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.ensure_idle_for_source_mutation().await?;
    let leaf_id = params.get("leaf_id").and_then(Value::as_str);
    let stored = state
        .repo
        .load_stored_session(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    ensure_expected_active_leaf_matches(&stored.active_leaf_id, &params)?;
    let store = transcript_store_from_stored(&stored)?;
    if !store.is_turn_boundary_at(leaf_id) {
        return Err(RpcError::new(
            "not_turn_boundary",
            "history.rewind requires a turn boundary",
        ));
    }
    let events = state
        .repo
        .set_active_leaf(&session_id, leaf_id)
        .await
        .map_err(anyhow::Error::from)?;
    publish_events(state, events);
    clear_event_buffer_if_idle(state, &session_id).await?;
    Ok(json!({ "session_id": session_id, "active_leaf_id": leaf_id }))
}

async fn history_fork(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let Some(leaf_id) = params.get("leaf_id").and_then(Value::as_str) else {
        return Err(RpcError::new(
            "missing_leaf_id",
            "history.fork requires an explicit transcript entry",
        ));
    };
    let placement = params
        .get("placement")
        .and_then(Value::as_str)
        .map(|value| {
            ForkPlacement::parse(value).ok_or_else(|| {
                RpcError::new(
                    "invalid_placement",
                    "history.fork placement must be 'at' or 'before'",
                )
            })
        })
        .transpose()?
        .unwrap_or(ForkPlacement::At);
    let new_session_id = params
        .get("new_session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("session_{}", Uuid::new_v4()));
    let config = state
        .repo
        .load_session_config(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let stored = state
        .repo
        .load_stored_session(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let store = transcript_store_from_stored(&stored)?;
    if !store.contains_entry(leaf_id) {
        return Err(RpcError::new(
            "entry_not_found",
            "history.fork target is not in the transcript",
        ));
    }
    let mut fork_entries = stored
        .entries
        .iter()
        .cloned()
        .map(Into::into)
        .collect::<Vec<_>>();
    let active_leaf_id = match placement {
        ForkPlacement::Before => {
            let Some(target) = store.get_entry(leaf_id) else {
                return Err(RpcError::new(
                    "entry_not_found",
                    "history.fork target is not in the transcript",
                ));
            };
            if !matches!(target.item, TranscriptItem::UserMessage(_)) {
                return Err(RpcError::new(
                    "invalid_placement",
                    "placement='before' is only valid for user messages",
                ));
            }
            fork_branch_before_user_message(&store, leaf_id)
                .last()
                .map(|entry| entry.id.clone())
        }
        ForkPlacement::At => {
            let branch = store.path_entries_to(leaf_id);
            let original_len = branch.len();
            let recovered = recover_fork_branch_tail(branch);
            let active_leaf_id = recovered.last().map(|entry| entry.id.clone());
            fork_entries.extend(recovered.into_iter().skip(original_len));
            active_leaf_id
        }
    };
    let child_active_leaf_id = active_leaf_id.clone();
    let events = state
        .repo
        .create_fork(
            &session_id,
            &new_session_id,
            &config,
            &fork_entries,
            leaf_id,
            active_leaf_id,
        )
        .await
        .map_err(anyhow::Error::from)?;
    publish_events(state, events);
    clear_event_buffer_if_idle(state, &session_id).await?;
    clear_event_buffer_if_idle(state, &new_session_id).await?;
    Ok(json!({
        "session_id": new_session_id,
        "source_leaf_id": leaf_id,
        "placement": placement.as_str(),
        "active_leaf_id": child_active_leaf_id,
    }))
}

async fn turn_resume(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.ensure_idle_for_source_mutation().await?;

    let stored = state
        .repo
        .load_stored_session(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    ensure_expected_active_leaf_matches(&stored.active_leaf_id, &params)?;
    let leaf_id = params
        .get("leaf_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| stored.active_leaf_id.clone())
        .ok_or_else(|| {
            RpcError::new("no_terminal_turn", "session has no terminal turn to resume")
        })?;
    if stored.active_leaf_id.as_deref() != Some(leaf_id.as_str()) {
        return Err(RpcError::new(
            "history_changed",
            "turn.resume only resumes the active terminal turn",
        ));
    }

    let store = transcript_store_from_stored(&stored)?;
    let Some(entry) = store.get_entry(&leaf_id) else {
        return Err(RpcError::new(
            "entry_not_found",
            "active transcript entry not found",
        ));
    };
    let (turn_id, outcome) = match &entry.item {
        TranscriptItem::TurnFinished { turn_id, outcome } => (*turn_id, *outcome),
        _ => {
            return Err(RpcError::new(
                "not_terminal_turn",
                "turn.resume requires an interrupted or crashed terminal turn",
            ))
        }
    };
    if !matches!(outcome, TurnOutcome::Interrupted | TurnOutcome::Crashed) {
        return Err(RpcError::new(
            "not_resumable",
            "only crashed or interrupted turns can be resumed",
        ));
    }

    let action = state
        .repo
        .find_resumable_model_action(&session_id, turn_id)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| {
            RpcError::new(
                "not_resumable",
                "this turn cannot be resumed because its terminal work was not a model request",
            )
        })?;
    if !store.contains_entry(&action.context_leaf_id) {
        return Err(RpcError::new(
            "invalid_resume_checkpoint",
            "model resume checkpoint is not in the transcript",
        ));
    }

    let dispatches = driver
        .resume_model_turn(
            &action.context_leaf_id,
            action.turn_id,
            action.action_id,
            action.context_tokens,
        )
        .await?;
    driver.dispatch(dispatches);
    Ok(json!({
        "session_id": session_id,
        "turn_id": turn_id.0,
        "outcome": outcome,
        "prior_action_status": action.status,
        "checkpoint_leaf_id": action.context_leaf_id,
    }))
}

async fn compaction_request(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.ensure_idle_for_source_mutation().await?;
    let config = state
        .repo
        .load_session_config(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let created = state
        .repo
        .create_compaction_action(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    publish_events(state, created.events);
    let action_row_id = created.job.action_row_id.clone();
    spawn_compaction(state, session_id, created.job, config);
    Ok(json!({ "action_row_id": action_row_id }))
}

async fn harness_model_complete(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let action_row_id = required_string(&params, "action_row_id")?;
    let assistant = parse_assistant_message(
        params
            .get("assistant")
            .cloned()
            .ok_or_else(|| RpcError::new("invalid_params", "assistant is required"))?,
    )?;
    let action = state
        .repo
        .load_action(&session_id, &action_row_id)
        .await
        .map_err(|error| RpcError::new("stale_action", error.to_string()))?;
    if action.kind != ActionKind::Model {
        return Err(RpcError::new(
            "invalid_action",
            "action is not a model action",
        ));
    }
    let driver = SessionDriver::acquire(state, &session_id).await;
    let active = driver
        .require_active_session("stale_action", "session is not active")
        .await?;
    let dispatches = driver
        .apply_agent_input(
            active,
            AgentInput::ModelCompleted {
                action_id: ActionId(action.action_id as u64),
                turn_id: TurnId(action.turn_id.unwrap_or_default() as u64),
                assistant,
            },
            Some(ActionUpdate {
                row_id: action_row_id,
                attempt_id: action.attempt_id,
                status: ActionStatus::Completed,
                result: json!({ "source": "harness" }),
            }),
            None,
            Vec::new(),
        )
        .await?;
    driver.dispatch(dispatches);
    driver.drive_until_blocked().await?;
    Ok(json!({ "completed": true }))
}

async fn harness_model_fail(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let action_row_id = required_string(&params, "action_row_id")?;
    let error = params
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("model failed")
        .to_string();
    let action = state
        .repo
        .load_action(&session_id, &action_row_id)
        .await
        .map_err(|error| RpcError::new("stale_action", error.to_string()))?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    let active = driver
        .require_active_session("stale_action", "session is not active")
        .await?;
    let dispatches = driver
        .apply_agent_input(
            active,
            AgentInput::ModelFailed {
                action_id: ActionId(action.action_id as u64),
                turn_id: TurnId(action.turn_id.unwrap_or_default() as u64),
                error: error.clone(),
            },
            Some(ActionUpdate {
                row_id: action_row_id,
                attempt_id: action.attempt_id,
                status: ActionStatus::Error,
                result: json!({ "error": error }),
            }),
            None,
            Vec::new(),
        )
        .await?;
    driver.dispatch(dispatches);
    driver.drive_until_blocked().await?;
    Ok(json!({ "failed": true }))
}
