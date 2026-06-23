#![forbid(unsafe_code)]

mod auth;
mod codec;
mod config;
mod delegation_context;
mod delegation_runner;
mod delegation_snapshot;
mod delegation_tools;
mod handoff;
mod model_metadata;
mod provider_runtime;
mod rpc_views;
mod runtime;
mod session_start;
mod state;
mod subagents;
mod types;
mod workspaces;

use crate::codec::{
    from_params, parse_assistant_message, parse_user_message, required_string, required_uuid,
    transcript_store_from_stored,
};
use crate::config::Config;
use crate::provider_runtime::{
    current_pi_template, ProviderConnectionRegistry, SessionTitleScheduler,
};
use crate::runtime::*;
use crate::state::AppState;
use crate::types::*;
use crate::workspaces::{validate_remote_branch, validate_workspace_dir, WorkspaceManager};

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use agent_core::AgentInput;
use agent_session::SessionInput;
use agent_store::{
    AcceptedInput, ActionKind, ActionStatus, ActionUpdate, CompactionTrigger, EventFrame,
    EventType, InputPriority, PostgresAgentStore, ProjectWorkspace, QueuedInputContent,
    QueuedInputStatus, SessionConfig, SubagentType, TranscriptEntryBodyMode, TranscriptEntryScope,
    WorkspaceKind,
};
use agent_tools::ToolRegistry;
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
    let workspaces = WorkspaceManager::from_default_state_dir()?;
    let prompt_root = find_prompt_root(std::env::current_dir()?)?;
    let state = AppState {
        repo,
        active: Arc::new(Mutex::new(HashMap::new())),
        session_driver_locks: Arc::new(Mutex::new(HashMap::new())),
        tasks: Arc::new(StdMutex::new(HashMap::new())),
        events,
        tools: Arc::new(ToolRegistry::with_builtin_tools()),
        provider_connections: ProviderConnectionRegistry::new(),
        session_titles: SessionTitleScheduler::default(),
        workspaces,
        prompt_root,
    };

    // Complete any delegation that crashed mid-barrier: a `running` delegation whose
    // subagents are all terminal is finished (handoff + wakeup observation)
    // exactly once via the same attempt-fenced CAS the live barrier uses.
    //
    // This runs AFTER the global stale-mark above, but that ordering is safe:
    // delegation terminality is transcript-boundary based, independent of action
    // status, so a subagent stale-marked mid-turn is still NON-terminal. The
    // sweep recovers each subagent to a boundary first, so a resumable mid-turn
    // child re-establishes live work (delegation stays running) instead of being
    // abandoned as a failure.
    delegation_runner::sweep_running_delegations_on_boot(&state).await;

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

fn find_prompt_root(start: PathBuf) -> Result<PathBuf> {
    for path in start.ancestors() {
        if path.join("PI.md").is_file() {
            return Ok(path.to_path_buf());
        }
    }
    Err(anyhow::anyhow!(
        "could not find PI.md from {}",
        start.display()
    ))
}

async fn drain_dispatch_tasks(state: &AppState) {
    let handles = take_tasks(state);
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
        RpcMethod::SessionStart => session_start::session_start(state, params).await,
        RpcMethod::SessionList => session_list(state, params).await,
        RpcMethod::SessionGet => session_get(state, params).await,
        RpcMethod::SessionSyncActiveBranch => session_sync_active_branch(state, params).await,
        RpcMethod::SessionRename => session_rename(state, params).await,
        RpcMethod::SessionConfigure => session_configure(state, params).await,
        RpcMethod::SessionDelete => session_delete(state, params).await,
        RpcMethod::ProjectList => project_list(state).await,
        RpcMethod::ProjectCreate => project_create(state, params).await,
        RpcMethod::ProjectUpdate => project_update(state, params).await,
        RpcMethod::ProjectDelete => project_delete(state, params).await,
        RpcMethod::SystemPrompt => system_prompt(state, params).await,
        RpcMethod::EventsSubscribe => {
            events_subscribe(state, subscriptions, event_high_water, params).await
        }
        RpcMethod::EventsUnsubscribe => events_unsubscribe(subscriptions, event_high_water, params),
        RpcMethod::InputFollowUp => input_user(state, params).await,
        RpcMethod::InputPromoteQueued => input_promote_queued(state, params).await,
        RpcMethod::InputUpdateQueued => input_update_queued(state, params).await,
        RpcMethod::InputCancelQueued => input_cancel_queued(state, params).await,
        RpcMethod::InputReorderQueuedFollowUps => {
            input_reorder_queued_follow_ups(state, params).await
        }
        RpcMethod::InputInterrupt => input_interrupt(state, params).await,
        RpcMethod::TranscriptIndex => transcript_index(state, params).await,
        RpcMethod::TranscriptEntries => transcript_entries(state, params).await,
        RpcMethod::TranscriptTurns => transcript_turns(state, params).await,
        RpcMethod::TranscriptTurnDetail => transcript_turn_detail(state, params).await,
        RpcMethod::HistoryTree => history_tree(state, params).await,
        RpcMethod::HistoryContext => history_context(state, params).await,
        RpcMethod::HistorySwitch => history_switch(state, params).await,
        RpcMethod::TurnResume => turn_resume(state, params).await,
        RpcMethod::ToolsList => tools_list(state, params),
        RpcMethod::CompactionRequest => compaction_request(state, params).await,
        RpcMethod::DelegationStartFull => delegation_tools::rpc_start_full(state, params).await,
        RpcMethod::DelegationStartReadonlyFanout => {
            delegation_tools::rpc_start_readonly_fanout(state, params).await
        }
        RpcMethod::DelegationStatus => delegation_tools::rpc_status(state, params).await,
        RpcMethod::DelegationCancel => delegation_tools::rpc_cancel(state, params).await,
        RpcMethod::DelegationList => delegation_tools::rpc_list(state, params).await,
        RpcMethod::DelegationReadHandoffFile => {
            delegation_tools::rpc_read_handoff_file(state, params).await
        }
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
    let tools = state
        .tools
        .provider_tools_for_provider(provider)
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
                "canonical_name": tool.canonical_name,
                "prompt_alias": tool.prompt_alias,
                "execution": tool.execution,
                "kind": "local_tool",
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "tools": tools }))
}

async fn session_list(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let limit = params.get("limit").and_then(Value::as_i64).unwrap_or(50);
    let project_id = params
        .get("project_id")
        .and_then(Value::as_str)
        .map(|value| {
            Uuid::parse_str(value).map_err(|error| {
                RpcError::new(
                    "invalid_params",
                    format!("project_id must be a UUID: {error}"),
                )
            })
        })
        .transpose()?;
    let sessions = state.repo.list_sessions(project_id, limit).await?;
    Ok(json!({
        "sessions": sessions
            .into_iter()
            .map(rpc_views::session_summary)
            .collect::<Vec<_>>()
    }))
}

async fn session_get(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let started_at = Instant::now();
    let include_entries = params
        .get("include_entries")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let entries_scope = params
        .get("entries_scope")
        .and_then(Value::as_str)
        .unwrap_or("full_tree");
    if !matches!(entries_scope, "full_tree" | "active_branch") {
        return Err(RpcError::new(
            "invalid_params",
            "entries_scope must be 'full_tree' or 'active_branch'",
        ));
    }
    let driver = SessionDriver::acquire(state, &session_id).await;
    let acquired_ms = started_at.elapsed().as_millis();
    driver.recover_if_needed().await?;
    let recovered_ms = started_at.elapsed().as_millis();
    let snapshot = state.repo.session_snapshot(&session_id).await?;
    let snapshot_ms = started_at.elapsed().as_millis();
    let entries = if include_entries {
        let scope = if entries_scope == "active_branch" {
            TranscriptEntryScope::ActiveBranch
        } else {
            TranscriptEntryScope::FullTree
        };
        Some(
            state
                .repo
                .transcript_entries_for_scope(&session_id, scope, TranscriptEntryBodyMode::Ui)
                .await?,
        )
    } else {
        None
    };
    let entries_ms = started_at.elapsed().as_millis();
    let entry_count = entries.as_ref().map(Vec::len).unwrap_or_default();
    let value = rpc_views::session_snapshot(snapshot, entries);
    let total_ms = started_at.elapsed().as_millis();
    if perf_logging_enabled() {
        eprintln!(
            "perf session.get session={session_id} include_entries={include_entries} scope={entries_scope} entries={entry_count} acquire_ms={acquired_ms} recover_ms={} snapshot_ms={} entries_ms={} view_ms={} total_ms={total_ms}",
            recovered_ms.saturating_sub(acquired_ms),
            snapshot_ms.saturating_sub(recovered_ms),
            entries_ms.saturating_sub(snapshot_ms),
            total_ms.saturating_sub(entries_ms),
        );
    }
    Ok(value)
}

async fn session_sync_active_branch(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let base_leaf_id = params.get("base_leaf_id").and_then(Value::as_str);
    let started_at = Instant::now();
    let driver = SessionDriver::acquire(state, &session_id).await;
    let acquired_ms = started_at.elapsed().as_millis();
    driver.recover_if_needed().await?;
    let recovered_ms = started_at.elapsed().as_millis();
    let sync = state
        .repo
        .sync_active_branch(&session_id, base_leaf_id, TranscriptEntryBodyMode::Ui)
        .await?;
    let sync_ms = started_at.elapsed().as_millis();
    let overview = state.repo.session_snapshot(&session_id).await?;
    let snapshot_ms = started_at.elapsed().as_millis();
    let entry_count = sync.entries.len();
    let status = sync.status;
    let value = rpc_views::active_branch_sync(sync, overview);
    let total_ms = started_at.elapsed().as_millis();
    if perf_logging_enabled() {
        eprintln!(
            "perf session.sync_active_branch session={session_id} base_leaf_id={base_leaf_id:?} status={status} entries={entry_count} acquire_ms={acquired_ms} recover_ms={} sync_ms={} snapshot_ms={} view_ms={} total_ms={total_ms}",
            recovered_ms.saturating_sub(acquired_ms),
            sync_ms.saturating_sub(recovered_ms),
            snapshot_ms.saturating_sub(sync_ms),
            total_ms.saturating_sub(snapshot_ms),
        );
    }
    Ok(value)
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
        .rename_session_manually(&session_id, &title)
        .await?;
    let config = state.repo.load_session_config(&session_id).await?;
    replace_active_session_config(state, &session_id, config.clone()).await;
    publish_events(state, events);
    clear_event_buffer_if_idle(state, &session_id).await?;
    Ok(json!({
        "session_id": session_id,
        "title": title,
        "metadata": config.metadata,
        "activity": state.repo.activity(&session_id).await?
    }))
}

async fn session_delete(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    if !state.repo.session_exists(&session_id).await? {
        return Err(RpcError::new("session_not_found", "session not found"));
    }

    let (child_session_ids, _drivers) =
        lock_hidden_subagent_delete_tree(state, &session_id).await?;

    let mut delete_order = child_session_ids.clone();
    delete_order.reverse();
    delete_order.push(session_id.clone());
    for candidate_session_id in &delete_order {
        state.active.lock().await.remove(candidate_session_id);
        let deleted = state.repo.delete_session(candidate_session_id).await?;
        if !deleted && candidate_session_id == &session_id {
            return Err(RpcError::new("session_not_found", "session not found"));
        }
        if let Err(error) = state
            .workspaces
            .destroy_session_workspaces(candidate_session_id)
            .await
        {
            eprintln!(
                "failed to remove session workspace state for {candidate_session_id}: {error:#}"
            );
        }
        state
            .provider_connections
            .remove_session(candidate_session_id)
            .await;
    }

    Ok(json!({
        "session_id": session_id,
        "deleted": true,
        "deleted_child_session_ids": child_session_ids,
    }))
}

async fn lock_hidden_subagent_delete_tree(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<(Vec<String>, Vec<SessionDriver>), RpcError> {
    let mut seen = BTreeSet::new();
    seen.insert(session_id.to_string());
    let mut stack = vec![session_id.to_string()];
    let mut child_session_ids = Vec::new();
    let mut drivers = Vec::new();
    while let Some(parent_session_id) = stack.pop() {
        let driver = SessionDriver::acquire(state, &parent_session_id).await;
        driver.ensure_idle_for_source_mutation().await?;
        let child_session_ids_for_parent = state
            .repo
            .list_child_session_ids(&parent_session_id)
            .await?;
        for child_session_id in child_session_ids_for_parent {
            if !seen.insert(child_session_id.clone()) {
                continue;
            }
            stack.push(child_session_id.clone());
            child_session_ids.push(child_session_id);
        }
        drivers.push(driver);
    }
    Ok((child_session_ids, drivers))
}

async fn session_configure(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    let current = state.repo.load_session_config(&session_id).await?;
    let provider = params
        .get("provider")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))?
        .unwrap_or_else(|| current.provider.clone());
    let mut metadata = params
        .get("metadata")
        .cloned()
        .unwrap_or_else(|| current.metadata.clone());
    if metadata_title(&metadata) != metadata_title(&current.metadata) {
        ensure_metadata_object(&mut metadata)
            .insert("auto_title_disabled".to_string(), json!(true));
    }
    let model_changed = provider_model_changed(&current.provider, &provider);
    let metadata_changed = metadata != current.metadata;
    if model_changed {
        driver.ensure_idle_for_source_mutation().await?;
        if state.repo.has_transcript_entries(&session_id).await? {
            return Err(RpcError::new(
                "provider_locked",
                "session model cannot be changed after the first transcript entry",
            ));
        }
    } else if metadata_changed {
        driver.ensure_idle_for_metadata_mutation().await?;
    }
    let config = SessionConfig {
        project_id: current.project_id,
        outer_cwd: current.outer_cwd.clone(),
        workspaces: current.workspaces.clone(),
        system_prompt: current.system_prompt.clone(),
        provider,
        metadata,
    };
    let events = state.repo.configure_session(&session_id, &config).await?;
    if let Some(active) = driver.active_session().await {
        active.lock().await.config = config.clone();
    }
    publish_events(state, events);
    clear_event_buffer_if_idle(state, &session_id).await?;
    Ok(json!({
        "session_id": session_id,
        "provider": config.provider,
        "metadata": config.metadata,
        "activity": state.repo.activity(&session_id).await?
    }))
}

fn provider_model_changed(previous: &ProviderConfig, next: &ProviderConfig) -> bool {
    previous.kind != next.kind || previous.model != next.model
}

fn metadata_title(metadata: &Value) -> Option<&str> {
    metadata
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
}

fn ensure_metadata_object(metadata: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !metadata.is_object() {
        *metadata = json!({});
    }
    metadata
        .as_object_mut()
        .expect("metadata was forced to object")
}

async fn project_list(state: &AppState) -> std::result::Result<Value, RpcError> {
    let projects = state.repo.list_projects().await?;
    Ok(json!({
        "projects": projects
            .into_iter()
            .map(rpc_views::project)
            .collect::<Vec<_>>()
    }))
}

async fn project_create(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let params: ProjectWriteParams = from_params(params)?;
    let project_id = params.project_id.unwrap_or_else(Uuid::new_v4);
    let name = params.name.trim();
    if name.is_empty() {
        return Err(RpcError::new("invalid_params", "project name is required"));
    }
    let workspaces = validate_project_workspaces(&params.workspaces).await?;
    let project = state
        .repo
        .create_project(
            project_id,
            name,
            &workspaces,
            params.metadata.unwrap_or_else(|| json!({})),
        )
        .await?;
    Ok(rpc_views::project(project))
}

async fn project_update(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let project_id = required_uuid(&params, "project_id")?;
    let current = state.repo.get_project(project_id).await?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or(&current.name);
    if name.is_empty() {
        return Err(RpcError::new("invalid_params", "project name is required"));
    }
    let workspaces = if params.get("workspaces").is_some() {
        let workspaces = params
            .get("workspaces")
            .cloned()
            .map(serde_json::from_value::<Vec<ProjectWorkspace>>)
            .transpose()
            .map_err(|error| RpcError::new("invalid_params", error.to_string()))?
            .unwrap_or_default();
        validate_project_workspaces(&workspaces).await?
    } else {
        current.workspaces
    };
    let project = state
        .repo
        .update_project(project_id, name, &workspaces)
        .await?;
    if let Err(error) = state
        .workspaces
        .reconcile_project_bases(project_id, &project.workspaces)
        .await
    {
        eprintln!("failed to reconcile workspace bases for project {project_id}: {error:#}");
    }
    Ok(rpc_views::project(project))
}

async fn project_delete(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let project_id = required_uuid(&params, "project_id")?;
    let deleted = state.repo.delete_empty_project(project_id).await?;
    if !deleted {
        return Err(RpcError::new(
            "project_not_empty",
            "project was not found or still has sessions",
        ));
    }
    if let Err(error) = state.workspaces.remove_project_bases(project_id).await {
        eprintln!("failed to remove workspace bases for project {project_id}: {error:#}");
    }
    Ok(json!({ "project_id": project_id, "deleted": true }))
}

#[derive(Debug, Deserialize)]
struct ProjectWriteParams {
    project_id: Option<Uuid>,
    name: String,
    #[serde(default)]
    workspaces: Vec<ProjectWorkspace>,
    metadata: Option<Value>,
}

async fn validate_project_workspaces(
    workspaces: &[ProjectWorkspace],
) -> std::result::Result<Vec<ProjectWorkspace>, RpcError> {
    if workspaces.is_empty() {
        return Err(RpcError::new(
            "invalid_workspace",
            "projects require at least one workspace",
        ));
    }
    let mut seen_dirs = BTreeSet::new();
    let mut checked_workspaces = Vec::new();
    for workspace in workspaces {
        let workspace_dir = workspace.workspace_dir.trim();
        validate_workspace_dir(workspace_dir)
            .map_err(|error| RpcError::new("invalid_workspace", error.to_string()))?;
        if !seen_dirs.insert(workspace_dir.to_string()) {
            return Err(RpcError::new(
                "invalid_workspace",
                format!("duplicate workspace_dir: {workspace_dir}"),
            ));
        }
        match workspace.kind {
            WorkspaceKind::Git => {
                let remote_url = workspace.remote_url.as_deref().unwrap_or("").trim();
                let remote_branch = workspace.remote_branch.as_deref().unwrap_or("").trim();
                validate_remote_branch(remote_url, remote_branch)
                    .await
                    .map_err(|error| RpcError::new("invalid_workspace", error.to_string()))?;
                checked_workspaces.push(ProjectWorkspace::git(
                    workspace_dir,
                    remote_url,
                    remote_branch,
                ));
            }
            WorkspaceKind::Local => {
                let source_path = workspace.source_path.as_deref().unwrap_or("").trim();
                if source_path.is_empty() {
                    return Err(RpcError::new(
                        "invalid_workspace",
                        "local workspace source_path is required",
                    ));
                }
                let source_path = PathBuf::from(source_path);
                if !source_path.is_dir() {
                    return Err(RpcError::new(
                        "invalid_workspace",
                        format!(
                            "local workspace source_path is not a directory: {}",
                            source_path.display()
                        ),
                    ));
                }
                checked_workspaces.push(ProjectWorkspace::local(
                    workspace_dir,
                    source_path.to_string_lossy().into_owned(),
                ));
            }
        }
    }
    Ok(checked_workspaces)
}

async fn system_prompt(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("session_required", "system.prompt requires session_id"))?;
    let config = state.repo.load_session_config(session_id).await?;
    state
        .workspaces
        .ensure_session(session_id, &config.outer_cwd, &config.workspaces)
        .await?;
    Ok(json!({
        "template": current_pi_template(state)?,
        "rendered": config.system_prompt,
    }))
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
        let current = state.repo.last_event_id(&session_id).await?;
        event_high_water.insert(session_id, current);
        return Ok(json!({ "replayed": [] }));
    };
    event_high_water.insert(session_id.clone(), after_event_id);
    let events = state
        .repo
        .events_after(&session_id, Some(after_event_id))
        .await?;
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

pub(crate) async fn input_user(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let priority = params
        .get("priority")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))?
        .unwrap_or(InputPriority::FollowUp);
    // Server-side guard: a read-only subagent must never be steered. The legacy
    // client-only check is not authoritative, so reject a Steer-priority input
    // targeting a read_only subagent here. Follow-up inputs (and the barrier's
    // top-level parent wakeup observation, which targets a parent session) are
    // unaffected.
    if priority == InputPriority::Steer
        && state.repo.session_subagent_type(&session_id).await? == Some(SubagentType::ReadOnly)
    {
        return Err(RpcError::new(
            "cannot_steer_read_only_subagent",
            "a read-only subagent cannot be steered",
        ));
    }
    let client_input_id = params
        .get("client_input_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let base_leaf_id = params
        .get("base_leaf_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let content_value = params
        .get("content")
        .cloned()
        .ok_or_else(|| RpcError::new("invalid_params", "content is required"))?;
    let content = parse_user_message(content_value)?;
    enqueue_session_input(
        state,
        SessionInputRequest {
            session_id,
            priority,
            content,
            client_input_id,
            base_leaf_id,
            expected_active_leaf_id: params.get("expected_active_leaf_id").cloned(),
        },
    )
    .await
}

pub(crate) struct SessionInputRequest {
    pub(crate) session_id: String,
    pub(crate) priority: InputPriority,
    pub(crate) content: agent_vocab::UserMessage,
    pub(crate) client_input_id: Option<String>,
    pub(crate) base_leaf_id: Option<String>,
    pub(crate) expected_active_leaf_id: Option<Value>,
}

pub(crate) async fn enqueue_session_input(
    state: &AppState,
    request: SessionInputRequest,
) -> std::result::Result<Value, RpcError> {
    let SessionInputRequest {
        session_id,
        priority,
        content,
        client_input_id,
        base_leaf_id,
        expected_active_leaf_id,
    } = request;
    enum InputOutcome {
        Accepted {
            dispatches: Vec<DispatchAction>,
            active_branch_sync: Value,
            publish_subagent_running: bool,
        },
        Queued {
            input_id: String,
            event: Option<EventFrame>,
            queue: Option<Value>,
            should_drive: bool,
            publish_subagent_running: bool,
        },
    }

    let started_at = Instant::now();
    let driver = SessionDriver::acquire(state, &session_id).await;
    let acquired_ms = started_at.elapsed().as_millis();
    driver.recover_if_needed().await?;
    let recovered_ms = started_at.elapsed().as_millis();
    if priority == InputPriority::Steer
        && state.repo.session_parent_id(&session_id).await?.is_some()
        && state.repo.active_leaf_is_turn_boundary(&session_id).await?
    {
        return Err(RpcError::new(
            "subagent_terminal",
            "cannot steer a subagent that is already terminal",
        ));
    }
    let mut expected_params = json!({});
    if let Some(expected_active_leaf_id) = expected_active_leaf_id {
        expected_params["expected_active_leaf_id"] = expected_active_leaf_id;
    }
    let outcome = {
        if let Some(client_input_id) = client_input_id.as_deref() {
            if let Some(record) = state
                .repo
                .find_client_input(&session_id, client_input_id)
                .await?
            {
                let queue = state
                    .repo
                    .queue_state(&session_id)
                    .await
                    .map(rpc_views::queue_state)?;
                if perf_logging_enabled() {
                    let total_ms = started_at.elapsed().as_millis();
                    eprintln!(
                        "perf input.follow_up session={session_id} priority={priority} replay=true acquire_ms={acquired_ms} recover_ms={} total_ms={total_ms}",
                        recovered_ms.saturating_sub(acquired_ms),
                    );
                }
                return Ok(json!({
                    "input_id": record.input_id,
                    "accepted": record.status == QueuedInputStatus::Consumed,
                    "queued": matches!(
                        record.status,
                        QueuedInputStatus::Queued | QueuedInputStatus::Consuming
                    ),
                    "replayed": true,
                    "queue": queue,
                }));
            }
        }
        let has_running = state.repo.has_unfinished_actions(&session_id).await?;
        let has_queued = state.repo.has_queued_inputs(&session_id).await?;
        if has_running || has_queued {
            let queued = state
                .repo
                .enqueue_user_input(&session_id, priority, &content, client_input_id.as_deref())
                .await?;
            InputOutcome::Queued {
                input_id: queued.input_id,
                event: queued.event,
                queue: queued.queue.map(rpc_views::queue_state),
                should_drive: !has_running,
                publish_subagent_running: !has_running,
            }
        } else {
            ensure_expected_active_leaf(state, &session_id, &expected_params).await?;
            driver.ensure_active_loaded().await?;
            let active = driver
                .require_active_session("session_not_found", "session not found")
                .await?;
            {
                let mut runtime = active.lock().await;
                runtime
                    .session
                    .enqueue_input(agent_input_from_queued_priority(
                        priority,
                        QueuedInputContent::user_message(content.clone()),
                    ))
                    .map_err(|error| RpcError::new("invalid_input", error.to_string()))?;
            }
            let dispatches = driver
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
                .await?;
            let sync = state
                .repo
                .sync_active_branch(
                    &session_id,
                    base_leaf_id.as_deref(),
                    TranscriptEntryBodyMode::Ui,
                )
                .await?;
            let snapshot = state.repo.session_snapshot(&session_id).await?;
            InputOutcome::Accepted {
                dispatches,
                active_branch_sync: rpc_views::active_branch_sync(sync, snapshot),
                publish_subagent_running: true,
            }
        }
    };

    match outcome {
        InputOutcome::Accepted {
            dispatches,
            active_branch_sync,
            publish_subagent_running,
        } => {
            if publish_subagent_running {
                subagents::publish_subagent_parent_running_if_child(state, &session_id).await;
            }
            driver.dispatch(dispatches).await?;
            if perf_logging_enabled() {
                let total_ms = started_at.elapsed().as_millis();
                eprintln!(
                    "perf input.follow_up session={session_id} priority={priority} queued=false acquire_ms={acquired_ms} recover_ms={} total_ms={total_ms}",
                    recovered_ms.saturating_sub(acquired_ms),
                );
            }
            Ok(
                json!({ "accepted": true, "queued": false, "active_branch_sync": active_branch_sync }),
            )
        }
        InputOutcome::Queued {
            input_id,
            event,
            queue,
            should_drive,
            publish_subagent_running,
        } => {
            if let Some(event) = event {
                publish_events(state, vec![event]);
            }
            if should_drive {
                if publish_subagent_running {
                    subagents::publish_subagent_parent_running_if_child(state, &session_id).await;
                }
                driver.drive_until_blocked().await?;
            }
            if perf_logging_enabled() {
                let total_ms = started_at.elapsed().as_millis();
                eprintln!(
                    "perf input.follow_up session={session_id} priority={priority} queued=true should_drive={should_drive} acquire_ms={acquired_ms} recover_ms={} total_ms={total_ms}",
                    recovered_ms.saturating_sub(acquired_ms),
                );
            }
            Ok(json!({ "input_id": input_id, "accepted": true, "queued": true, "queue": queue }))
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
        "queue": rpc_views::queue_state(result.queue),
    }))
}

async fn input_update_queued(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let input_id = required_string(&params, "input_id")?;
    let expected_queue_revision = params
        .get("expected_queue_revision")
        .and_then(Value::as_i64);
    let content_value = params
        .get("content")
        .cloned()
        .ok_or_else(|| RpcError::new("invalid_params", "content is required"))?;
    let content = parse_user_message(content_value)?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let result = state
        .repo
        .update_queued_input(&session_id, &input_id, &content, expected_queue_revision)
        .await
        .map_err(map_queued_mutation_error)?;
    if let Some(event) = result.event {
        publish_events(state, vec![event]);
    }
    Ok(json!({
        "input_id": result.input_id,
        "updated": result.updated,
        "reason": result.reason,
        "priority": result.priority,
        "status": result.status,
        "queue": rpc_views::queue_state(result.queue),
    }))
}

async fn input_cancel_queued(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let input_id = required_string(&params, "input_id")?;
    let expected_queue_revision = params
        .get("expected_queue_revision")
        .and_then(Value::as_i64);
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let result = state
        .repo
        .cancel_queued_input(&session_id, &input_id, expected_queue_revision)
        .await
        .map_err(map_queued_mutation_error)?;
    if let Some(event) = result.event {
        publish_events(state, vec![event]);
    }
    Ok(json!({
        "input_id": result.input_id,
        "cancelled": result.cancelled,
        "reason": result.reason,
        "priority": result.priority,
        "status": result.status,
        "queue": rpc_views::queue_state(result.queue),
    }))
}

async fn input_reorder_queued_follow_ups(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let expected_queue_revision = params
        .get("expected_queue_revision")
        .and_then(Value::as_i64);
    let input_ids = params
        .get("input_ids")
        .cloned()
        .ok_or_else(|| RpcError::new("invalid_params", "input_ids is required"))
        .and_then(|value| {
            serde_json::from_value::<Vec<String>>(value)
                .map_err(|error| RpcError::new("invalid_params", error.to_string()))
        })?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let result = state
        .repo
        .reorder_queued_follow_ups(&session_id, &input_ids, expected_queue_revision)
        .await
        .map_err(map_queued_mutation_error)?;
    if let Some(event) = result.event {
        publish_events(state, vec![event]);
    }
    Ok(json!({
        "reordered": result.reordered,
        "reason": result.reason,
        "input_ids": result.input_ids,
        "queue": rpc_views::queue_state(result.queue),
    }))
}

async fn input_interrupt(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    interrupt_session(state, &session_id).await
}

pub(crate) async fn interrupt_session(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<Value, RpcError> {
    let driver = SessionDriver::acquire(state, session_id).await;
    driver.recover_if_needed().await?;
    let active = driver.active_session().await;
    let Some(active) = active else {
        let events = state
            .repo
            .cancel_unfinished_session_work(session_id, "session interrupted")
            .await?;
        if !events.is_empty() {
            let aborted_tasks = abort_session_tasks(state, session_id);
            publish_events(state, events);
            driver.drive_until_blocked().await?;
            return Ok(json!({ "interrupted": true, "aborted_task_kinds": aborted_tasks }));
        }
        let event = state
            .repo
            .insert_event(
                session_id,
                EventType::InputIgnored,
                json!({ "kind": "interrupt" }),
            )
            .await?;
        publish_events(state, vec![event]);
        clear_event_buffer_if_idle(state, session_id).await?;
        return Ok(json!({ "ignored": true }));
    };
    let aborted_tasks = abort_session_tasks(state, session_id);
    let dispatches = driver
        .apply_agent_input(active, AgentInput::Interrupt, None)
        .await?;
    driver.dispatch(dispatches).await?;
    driver.drive_until_blocked().await?;
    Ok(json!({ "interrupted": true, "aborted_task_kinds": aborted_tasks }))
}

async fn transcript_index(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let after_sequence = params.get("after_sequence").and_then(Value::as_i64);
    let limit = params.get("limit").and_then(Value::as_i64);
    let started_at = Instant::now();
    let driver = SessionDriver::acquire(state, &session_id).await;
    let acquired_ms = started_at.elapsed().as_millis();
    driver.recover_if_needed().await?;
    let recovered_ms = started_at.elapsed().as_millis();
    let index = state
        .repo
        .transcript_tree_index(&session_id, after_sequence, limit)
        .await?;
    let load_ms = started_at.elapsed().as_millis();
    let node_count = index.nodes.len();
    let complete = index.complete;
    let value = rpc_views::transcript_tree_index(index);
    let total_ms = started_at.elapsed().as_millis();
    if perf_logging_enabled() {
        eprintln!(
            "perf transcript.index session={session_id} after_sequence={after_sequence:?} limit={limit:?} nodes={node_count} complete={complete} acquire_ms={acquired_ms} recover_ms={} load_ms={} view_ms={} total_ms={total_ms}",
            recovered_ms.saturating_sub(acquired_ms),
            load_ms.saturating_sub(recovered_ms),
            total_ms.saturating_sub(load_ms),
        );
    }
    Ok(value)
}

async fn transcript_entries(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let entry_ids = required_string_vec(&params, "entry_ids")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let result = state
        .repo
        .transcript_entries_by_id(&session_id, &entry_ids, TranscriptEntryBodyMode::Ui)
        .await?;
    Ok(rpc_views::transcript_entries(result))
}

async fn transcript_turns(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let before_entry_id = params.get("before_entry_id").and_then(Value::as_str);
    let limit = params.get("limit").and_then(Value::as_i64);
    let started_at = Instant::now();
    let driver = SessionDriver::acquire(state, &session_id).await;
    let acquired_ms = started_at.elapsed().as_millis();
    driver.recover_if_needed().await?;
    let recovered_ms = started_at.elapsed().as_millis();
    let result = state
        .repo
        .transcript_turns(&session_id, before_entry_id, limit)
        .await?;
    let loaded_ms = started_at.elapsed().as_millis();
    let card_count = result.cards.len();
    let value = rpc_views::transcript_turns(result);
    let total_ms = started_at.elapsed().as_millis();
    if perf_logging_enabled() {
        eprintln!(
            "perf transcript.turns session={session_id} before_entry_id={before_entry_id:?} limit={limit:?} cards={card_count} acquire_ms={acquired_ms} recover_ms={} load_ms={} view_ms={} total_ms={total_ms}",
            recovered_ms.saturating_sub(acquired_ms),
            loaded_ms.saturating_sub(recovered_ms),
            total_ms.saturating_sub(loaded_ms),
        );
    }
    Ok(value)
}

async fn transcript_turn_detail(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let card_id = required_string(&params, "card_id")?;
    let leaf_id = required_string(&params, "leaf_id")?;
    let start_sequence = required_i64(&params, "start_sequence")?;
    let end_sequence = required_i64(&params, "end_sequence")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let result = state
        .repo
        .transcript_turn_detail(
            &session_id,
            &card_id,
            &leaf_id,
            start_sequence,
            end_sequence,
            TranscriptEntryBodyMode::Ui,
        )
        .await?;
    Ok(rpc_views::transcript_turn_detail(result))
}

async fn history_tree(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let started_at = Instant::now();
    let driver = SessionDriver::acquire(state, &session_id).await;
    let acquired_ms = started_at.elapsed().as_millis();
    driver.recover_if_needed().await?;
    let recovered_ms = started_at.elapsed().as_millis();
    let tree = state.repo.history_tree(&session_id).await?;
    let loaded_ms = started_at.elapsed().as_millis();
    let entry_count = tree.entries.len();
    let value = rpc_views::history_tree(tree);
    let total_ms = started_at.elapsed().as_millis();
    if perf_logging_enabled() {
        eprintln!(
            "perf history.tree session={session_id} entries={entry_count} acquire_ms={acquired_ms} recover_ms={} load_ms={} view_ms={} total_ms={total_ms}",
            recovered_ms.saturating_sub(acquired_ms),
            loaded_ms.saturating_sub(recovered_ms),
            total_ms.saturating_sub(loaded_ms),
        );
    }
    Ok(value)
}

fn perf_logging_enabled() -> bool {
    std::env::var_os("PI_RELAY_PERF").is_some()
}

async fn history_context(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let leaf_id = params.get("leaf_id").and_then(Value::as_str);
    let stored = state.repo.load_stored_session(&session_id).await?;
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

async fn history_switch(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let started_at = Instant::now();
    let driver = SessionDriver::acquire(state, &session_id).await;
    let acquired_ms = started_at.elapsed().as_millis();
    driver.ensure_idle_for_source_mutation().await?;
    let idle_ms = started_at.elapsed().as_millis();
    let leaf_id = params.get("leaf_id").and_then(Value::as_str);
    let active_leaf_id = state.repo.active_leaf_id(&session_id).await?;
    ensure_expected_active_leaf_matches(&active_leaf_id, &params)?;
    let expected_ms = started_at.elapsed().as_millis();
    if !state
        .repo
        .transcript_leaf_is_turn_boundary(&session_id, leaf_id)
        .await?
    {
        return Err(RpcError::new(
            "not_turn_boundary",
            "history.switch requires a turn boundary",
        ));
    }
    let boundary_ms = started_at.elapsed().as_millis();
    let return_active_branch = params
        .get("return_active_branch")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let expected_transcript_revision = params
        .get("expected_transcript_revision")
        .and_then(Value::as_i64);
    let active_branch_entry_ids = optional_string_vec(&params, "active_branch_entry_ids")?;
    let missing_body_ids = optional_string_vec(&params, "missing_body_ids")?;
    let result = state
        .repo
        .switch_active_leaf(
            &session_id,
            leaf_id,
            return_active_branch,
            expected_transcript_revision,
            active_branch_entry_ids.as_deref(),
            missing_body_ids.as_deref(),
        )
        .await
        .map_err(history_switch_error_to_rpc)?;
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
    clear_event_buffer_if_idle(state, &session_id).await?;
    let publish_ms = started_at.elapsed().as_millis();
    let value = rpc_views::switch_active_leaf(result);
    let total_ms = started_at.elapsed().as_millis();
    if perf_logging_enabled() {
        eprintln!(
            "perf history.switch session={session_id} leaf_id={leaf_id:?} return_active_branch={return_active_branch} branch_ids={returned_id_count} bodies={returned_body_count} acquire_ms={acquired_ms} idle_ms={} expected_ms={} boundary_ms={} switch_ms={} publish_ms={} view_ms={} total_ms={total_ms}",
            idle_ms.saturating_sub(acquired_ms),
            expected_ms.saturating_sub(idle_ms),
            boundary_ms.saturating_sub(expected_ms),
            switch_ms.saturating_sub(boundary_ms),
            publish_ms.saturating_sub(switch_ms),
            total_ms.saturating_sub(publish_ms),
        );
    }
    Ok(value)
}

fn required_string_vec(params: &Value, key: &str) -> std::result::Result<Vec<String>, RpcError> {
    params
        .get(key)
        .cloned()
        .ok_or_else(|| RpcError::new("invalid_params", format!("{key} is required")))
        .and_then(|value| {
            serde_json::from_value::<Vec<String>>(value)
                .map_err(|error| RpcError::new("invalid_params", error.to_string()))
        })
}

fn required_i64(params: &Value, key: &str) -> std::result::Result<i64, RpcError> {
    params
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| RpcError::new("invalid_params", format!("{key} is required")))
}

fn optional_string_vec(
    params: &Value,
    key: &str,
) -> std::result::Result<Option<Vec<String>>, RpcError> {
    params
        .get(key)
        .cloned()
        .map(|value| {
            serde_json::from_value::<Vec<String>>(value)
                .map_err(|error| RpcError::new("invalid_params", error.to_string()))
        })
        .transpose()
}

fn history_switch_error_to_rpc(error: anyhow::Error) -> RpcError {
    let message = error.to_string();
    if message.starts_with("history_changed:") {
        return RpcError::new("history_changed", message);
    }
    error.into()
}

async fn turn_resume(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.ensure_idle_for_source_mutation().await?;

    let stored = state.repo.load_stored_session(&session_id).await?;
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
        .await?
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
        .resume_model_turn(&action.context_leaf_id, action.turn_id, action.action_id)
        .await?;
    driver.dispatch(dispatches).await?;
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
    let config = state.repo.load_session_config(&session_id).await?;
    state
        .workspaces
        .ensure_session(&session_id, &config.outer_cwd, &config.workspaces)
        .await?;
    let created = state
        .repo
        .create_compaction_action(&session_id, CompactionTrigger::Manual)
        .await?;
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
        .load_harness_model_action(&session_id, &action_row_id)
        .await
        .map_err(|error| RpcError::new("stale_action", error.to_string()))?;
    if action.kind != ActionKind::Model {
        return Err(RpcError::new(
            "invalid_action",
            "action is not a model action",
        ));
    }
    state
        .repo
        .claim_pending_model_action(&session_id, &action_row_id, &action.attempt_id)
        .await?;
    let driver = SessionDriver::acquire(state, &session_id).await;
    let active = driver
        .require_active_session("stale_action", "session is not active")
        .await?;
    let dispatches = driver
        .apply_session_input(
            active,
            SessionInput::ModelCompleted {
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
            Vec::new(),
        )
        .await?;
    driver.dispatch(dispatches).await?;
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
        .load_harness_model_action(&session_id, &action_row_id)
        .await
        .map_err(|error| RpcError::new("stale_action", error.to_string()))?;
    state
        .repo
        .claim_pending_model_action(&session_id, &action_row_id, &action.attempt_id)
        .await?;
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
        )
        .await?;
    driver.dispatch(dispatches).await?;
    driver.drive_until_blocked().await?;
    Ok(json!({ "failed": true }))
}
