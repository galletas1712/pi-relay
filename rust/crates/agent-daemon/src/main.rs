#![forbid(unsafe_code)]

mod auth;
mod codec;
mod config;
mod provider_runtime;
mod runtime;
mod state;
mod types;

use crate::codec::{
    fork_branch_before_user_message, from_params, parse_assistant_message, parse_model_context,
    parse_user_message, recover_fork_branch_tail, required_string, transcript_store_from_stored,
};
use crate::config::Config;
use crate::runtime::*;
use crate::state::AppState;
use crate::types::*;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use agent_session::{AgentInput, AgentSession, CompactionRequestId, SessionAction, SessionInput};
use agent_store::{
    AcceptedInput, ActionKind, ActionStatus, ActionUpdate, DispatchAction, EventFrame, EventType,
    InputPriority, PostgresAgentStore, ProviderConfig, QueuedInputStatus, SessionActivity,
    SessionConfig,
};
use agent_tools::{ToolContext, ToolRegistry};
use agent_vocab::TranscriptItem;
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env_and_args()?;
    let repo = Arc::new(PostgresAgentStore::connect(&config.database_url).await?);
    repo.migrate().await?;

    let (events, _) = broadcast::channel(1024);
    let state = AppState {
        repo,
        active: Arc::new(Mutex::new(HashMap::new())),
        pump_locks: Arc::new(Mutex::new(HashMap::new())),
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

    Ok(())
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
                let request: RpcRequest = serde_json::from_str(message.to_text()?)?;
                let response = match handle_request(&state, &mut subscriptions, &mut event_high_water, request).await {
                    Ok((id, value)) => RpcResponse { id, ok: true, result: Some(value), error: None },
                    Err((id, error)) => RpcResponse { id, ok: false, result: None, error: Some(error) },
                };
                writer.send(Message::Text(serde_json::to_string(&response)?.into())).await?;
            }
            event = events_rx.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
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
        RpcMethod::SessionCreate => session_create(state, params).await,
        RpcMethod::SessionStart => session_start(state, params).await,
        RpcMethod::SessionList => session_list(state, params).await,
        RpcMethod::SessionGet => session_get(state, params).await,
        RpcMethod::SessionConfigure => session_configure(state, params).await,
        RpcMethod::ConfigGet => config_get(state).await,
        RpcMethod::ConfigSet => config_set(state, params).await,
        RpcMethod::EventsSubscribe => {
            events_subscribe(state, subscriptions, event_high_water, params).await
        }
        RpcMethod::EventsUnsubscribe => events_unsubscribe(subscriptions, event_high_water, params),
        RpcMethod::InputFollowUp => input_user(state, params, InputPriority::FollowUp).await,
        RpcMethod::InputSteer => input_user(state, params, InputPriority::Steer).await,
        RpcMethod::InputPromoteQueued => input_promote_queued(state, params).await,
        RpcMethod::InputReplaceQueued => input_replace_queued(state, params).await,
        RpcMethod::InputCancelQueued => input_cancel_queued(state, params).await,
        RpcMethod::InputInterrupt => input_interrupt(state, params).await,
        RpcMethod::HistoryTree => history_tree(state, params).await,
        RpcMethod::HistoryContext => history_context(state, params).await,
        RpcMethod::HistoryRewind => history_rewind(state, params).await,
        RpcMethod::HistoryFork => history_fork(state, params).await,
        RpcMethod::ToolsList => Ok(json!({ "tools": state.tools.definitions() })),
        RpcMethod::CompactionRequest => compaction_request(state, params).await,
        RpcMethod::HarnessModelComplete => harness_model_complete(state, params).await,
        RpcMethod::HarnessModelFail => harness_model_fail(state, params).await,
        RpcMethod::HarnessCompactionComplete => harness_compaction_complete(state, params).await,
        RpcMethod::HarnessCompactionFail => harness_compaction_fail(state, params).await,
    }
}

async fn session_create(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let params: CreateSessionParams = from_params(params)?;
    let session_id = params
        .session_id
        .unwrap_or_else(|| format!("session_{}", Uuid::new_v4()));
    let config = SessionConfig {
        provider: params.provider,
        metadata: params.metadata.unwrap_or_else(|| json!({})),
    };
    let events = state
        .repo
        .create_session(&session_id, &config)
        .await
        .map_err(anyhow::Error::from)?;
    publish_events(state, events);
    Ok(json!({
        "session_id": session_id,
        "activity": SessionActivity::Idle,
    }))
}

#[derive(Debug, Deserialize)]
struct CreateSessionParams {
    session_id: Option<String>,
    provider: ProviderConfig,
    metadata: Option<Value>,
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

    let pump_lock = session_pump_lock(state, &session_id).await;
    let _pump_guard = pump_lock.lock().await;

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
    let (frames, dispatches) = state
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

    state
        .active
        .lock()
        .await
        .insert(session_id.clone(), Arc::new(Mutex::new(runtime)));
    publish_events(state, frames);
    dispatch_all(state, &session_id, dispatches);

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
    Ok(json!({ "sessions": sessions }))
}

async fn session_get(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    recover_if_needed(state, &session_id).await?;
    state
        .repo
        .session_snapshot(&session_id)
        .await
        .map_err(anyhow::Error::from)
        .map_err(Into::into)
}

async fn session_configure(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let pump_lock = session_pump_lock(state, &session_id).await;
    let _pump_guard = pump_lock.lock().await;
    ensure_idle_for_source_mutation(state, &session_id).await?;
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
        .unwrap_or(current.provider);
    let metadata = params.get("metadata").cloned().unwrap_or(current.metadata);
    let config = SessionConfig { provider, metadata };
    let events = state
        .repo
        .configure_session(&session_id, &config)
        .await
        .map_err(anyhow::Error::from)?;
    publish_events(state, events);
    Ok(
        json!({ "session_id": session_id, "activity": state.repo.activity(&session_id).await.map_err(anyhow::Error::from)? }),
    )
}

async fn config_get(state: &AppState) -> std::result::Result<Value, RpcError> {
    state
        .repo
        .global_config()
        .await
        .map_err(anyhow::Error::from)
        .map_err(Into::into)
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
    recover_if_needed(state, &session_id).await?;
    let after_event_id = params.get("after_event_id").and_then(Value::as_i64);
    subscriptions.insert(session_id.clone());
    let events = state
        .repo
        .events_after(&session_id, after_event_id)
        .await
        .map_err(anyhow::Error::from)?;
    let replayed_max = events
        .iter()
        .map(|event| event.event_id)
        .max()
        .unwrap_or_else(|| after_event_id.unwrap_or_default());
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
    recover_if_needed(state, &session_id).await?;
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
            should_pump: bool,
        },
    }

    let pump_lock = session_pump_lock(state, &session_id).await;
    let outcome = {
        let _pump_guard = pump_lock.lock().await;
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
        ensure_expected_active_leaf(state, &session_id, &params).await?;
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
                should_pump: !has_running,
            }
        } else {
            ensure_active_loaded(state, &session_id).await?;
            let active = { state.active.lock().await.get(&session_id).cloned() };
            let active =
                active.ok_or_else(|| RpcError::new("session_not_found", "session not found"))?;
            {
                let mut runtime = active.lock().await;
                runtime
                    .session
                    .enqueue_input(agent_input_from_queued_priority(priority, content.clone()))
                    .map_err(|error| RpcError::new("invalid_input", error.to_string()))?;
            }
            InputOutcome::Accepted(
                persist_active_outputs(
                    state,
                    &session_id,
                    active,
                    None,
                    None,
                    Some(AcceptedInput {
                        priority,
                        content: content.clone(),
                        client_input_id: client_input_id.clone(),
                    }),
                )
                .await?,
            )
        }
    };

    match outcome {
        InputOutcome::Accepted(dispatches) => {
            dispatch_all(state, &session_id, dispatches);
            Ok(json!({ "accepted": true, "queued": false }))
        }
        InputOutcome::Queued {
            input_id,
            event,
            should_pump,
        } => {
            if let Some(event) = event {
                publish_events(state, vec![event]);
            }
            if should_pump {
                pump_session(state, &session_id).await?;
            }
            Ok(json!({ "input_id": input_id, "accepted": true, "queued": true }))
        }
    }
}

async fn input_replace_queued(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let input_id = required_string(&params, "input_id")?;
    recover_if_needed(state, &session_id).await?;
    let content_value = params
        .get("content")
        .cloned()
        .ok_or_else(|| RpcError::new("invalid_params", "content is required"))?;
    let content = parse_user_message(content_value)?;
    let event = state
        .repo
        .replace_queued_input(&session_id, &input_id, &content)
        .await
        .map_err(map_queued_mutation_error)?;
    publish_events(state, vec![event]);
    Ok(json!({ "input_id": input_id, "replaced": true }))
}

async fn input_promote_queued(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let input_id = required_string(&params, "input_id")?;
    recover_if_needed(state, &session_id).await?;
    let event = state
        .repo
        .promote_queued_input(&session_id, &input_id)
        .await
        .map_err(map_queued_mutation_error)?;
    publish_events(state, vec![event]);
    Ok(json!({ "input_id": input_id, "promoted": true }))
}

async fn input_cancel_queued(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let input_id = required_string(&params, "input_id")?;
    recover_if_needed(state, &session_id).await?;
    let event = state
        .repo
        .cancel_queued_input(&session_id, &input_id)
        .await
        .map_err(map_queued_mutation_error)?;
    publish_events(state, vec![event]);
    Ok(json!({ "input_id": input_id, "cancelled": true }))
}

async fn input_interrupt(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    recover_if_needed(state, &session_id).await?;
    let active = { state.active.lock().await.get(&session_id).cloned() };
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
        return Ok(json!({ "ignored": true }));
    };
    let dispatches = apply_agent_input(
        state,
        &session_id,
        active,
        AgentInput::Interrupt,
        None,
        None,
    )
    .await?;
    dispatch_all(state, &session_id, dispatches);
    pump_session(state, &session_id).await?;
    Ok(json!({ "interrupted": true }))
}

async fn history_tree(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    recover_if_needed(state, &session_id).await?;
    state
        .repo
        .history_tree(&session_id)
        .await
        .map_err(anyhow::Error::from)
        .map_err(Into::into)
}

async fn history_context(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    recover_if_needed(state, &session_id).await?;
    let leaf_id = params.get("leaf_id").and_then(Value::as_str);
    let stored = state
        .repo
        .load_stored_session(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let store = transcript_store_from_stored(&stored)?;
    let entries = store.branch_entries(leaf_id);
    let items: Vec<_> = entries.into_iter().map(|entry| entry.item).collect();
    Ok(json!({ "items": items }))
}

async fn history_rewind(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let pump_lock = session_pump_lock(state, &session_id).await;
    let _pump_guard = pump_lock.lock().await;
    ensure_idle_for_source_mutation(state, &session_id).await?;
    let leaf_id = params.get("leaf_id").and_then(Value::as_str);
    let stored = state
        .repo
        .load_stored_session(&session_id)
        .await
        .map_err(anyhow::Error::from)?;
    ensure_expected_active_leaf_matches(&stored.active_leaf_id, &params)?;
    let store = transcript_store_from_stored(&stored)?;
    if !store.is_turn_boundary_leaf(leaf_id) {
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
    let branch = match placement {
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
        }
        ForkPlacement::At => recover_fork_branch_tail(store.branch_entries(Some(leaf_id))),
    };
    let active_leaf_id = branch.last().map(|entry| entry.id.clone());
    let child_active_leaf_id = active_leaf_id.clone();
    let events = state
        .repo
        .create_fork(
            &session_id,
            &new_session_id,
            &config,
            &branch,
            leaf_id,
            active_leaf_id,
        )
        .await
        .map_err(anyhow::Error::from)?;
    publish_events(state, events);
    Ok(json!({
        "session_id": new_session_id,
        "source_leaf_id": leaf_id,
        "placement": placement.as_str(),
        "active_leaf_id": child_active_leaf_id,
    }))
}

async fn compaction_request(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let pump_lock = session_pump_lock(state, &session_id).await;
    let _pump_guard = pump_lock.lock().await;
    ensure_idle_for_source_mutation(state, &session_id).await?;
    ensure_active_loaded(state, &session_id).await?;
    let active = { state.active.lock().await.get(&session_id).cloned() };
    let active = active.ok_or_else(|| RpcError::new("session_not_found", "session not found"))?;
    {
        let mut runtime = active.lock().await;
        runtime.session.compact();
    }
    let dispatch = persist_active_outputs(state, &session_id, active, None, None, None).await?;
    dispatch_all(state, &session_id, dispatch.clone());
    let request_id = dispatch.into_iter().find_map(|action| match action.action {
        SessionAction::RequestCompaction { .. } => Some(action.row_id),
        _ => None,
    });
    Ok(json!({ "action_row_id": request_id }))
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
    let active = { state.active.lock().await.get(&session_id).cloned() };
    let active = active.ok_or_else(|| RpcError::new("stale_action", "session is not active"))?;
    let dispatches = apply_agent_input(
        state,
        &session_id,
        active,
        AgentInput::ModelCompleted {
            action_id: agent_session::ActionId(action.action_id as u64),
            turn_id: agent_session::TurnId(action.turn_id.unwrap_or_default() as u64),
            assistant,
        },
        Some(ActionUpdate {
            row_id: action_row_id,
            attempt_id: action.attempt_id,
            status: ActionStatus::Completed,
            result: json!({ "source": "harness" }),
        }),
        None,
    )
    .await?;
    dispatch_all(state, &session_id, dispatches);
    pump_session(state, &session_id).await?;
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
    let active = { state.active.lock().await.get(&session_id).cloned() };
    let active = active.ok_or_else(|| RpcError::new("stale_action", "session is not active"))?;
    let dispatches = apply_agent_input(
        state,
        &session_id,
        active,
        AgentInput::ModelFailed {
            action_id: agent_session::ActionId(action.action_id as u64),
            turn_id: agent_session::TurnId(action.turn_id.unwrap_or_default() as u64),
            error: error.clone(),
        },
        Some(ActionUpdate {
            row_id: action_row_id,
            attempt_id: action.attempt_id,
            status: ActionStatus::Error,
            result: json!({ "error": error }),
        }),
        None,
    )
    .await?;
    dispatch_all(state, &session_id, dispatches);
    pump_session(state, &session_id).await?;
    Ok(json!({ "failed": true }))
}

async fn harness_compaction_complete(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let action_row_id = required_string(&params, "action_row_id")?;
    let action = state
        .repo
        .load_action(&session_id, &action_row_id)
        .await
        .map_err(|error| RpcError::new("stale_action", error.to_string()))?;
    if action.kind != ActionKind::Compaction {
        return Err(RpcError::new(
            "invalid_action",
            "action is not a compaction action",
        ));
    }
    let replacement = parse_model_context(
        params
            .get("replacement")
            .cloned()
            .ok_or_else(|| RpcError::new("invalid_params", "replacement is required"))?,
    )?;
    let active = { state.active.lock().await.get(&session_id).cloned() };
    let active = active.ok_or_else(|| RpcError::new("stale_action", "session is not active"))?;
    {
        let mut runtime = active.lock().await;
        runtime
            .session
            .enqueue_session_input(SessionInput::CompactionCompleted {
                request_id: CompactionRequestId(action.action_id as u64),
                replacement,
                context_tokens: None,
            })
            .map_err(|error| RpcError::new("invalid_input", error.to_string()))?;
    }
    let dispatches = persist_active_outputs(
        state,
        &session_id,
        active.clone(),
        Some(ActionUpdate {
            row_id: action_row_id,
            attempt_id: action.attempt_id,
            status: ActionStatus::Completed,
            result: json!({ "source": "harness" }),
        }),
        None,
        None,
    )
    .await?;
    dispatch_all(state, &session_id, dispatches);
    pump_session(state, &session_id).await?;
    Ok(json!({ "completed": true }))
}

async fn harness_compaction_fail(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let action_row_id = required_string(&params, "action_row_id")?;
    let error = params
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("compaction failed")
        .to_string();
    let action = state
        .repo
        .load_action(&session_id, &action_row_id)
        .await
        .map_err(|error| RpcError::new("stale_action", error.to_string()))?;
    let active = { state.active.lock().await.get(&session_id).cloned() };
    let active = active.ok_or_else(|| RpcError::new("stale_action", "session is not active"))?;
    {
        let mut runtime = active.lock().await;
        runtime
            .session
            .enqueue_session_input(SessionInput::CompactionFailed {
                request_id: CompactionRequestId(action.action_id as u64),
                error: error.clone(),
            })
            .map_err(|error| RpcError::new("invalid_input", error.to_string()))?;
    }
    let dispatches = persist_active_outputs(
        state,
        &session_id,
        active.clone(),
        Some(ActionUpdate {
            row_id: action_row_id,
            attempt_id: action.attempt_id,
            status: ActionStatus::Error,
            result: json!({ "error": error }),
        }),
        None,
        None,
    )
    .await?;
    dispatch_all(state, &session_id, dispatches);
    pump_session(state, &session_id).await?;
    Ok(json!({ "failed": true }))
}
