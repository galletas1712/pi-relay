#![forbid(unsafe_code)]

mod auth;
mod codec;
mod config;
mod delegation_context;
mod delegation_runner;
mod delegation_snapshot;
mod delegation_tools;
mod handoff;
mod history;
mod history_fork;
mod mcp_auth;
mod provider_runtime;
mod rpc_views;
mod runtime;
mod runtime_hosts;
mod session_start;
mod state;
mod subagents;
mod types;
mod workspace_selection;

use crate::codec::{
    from_params, parse_assistant_message, parse_user_message, required_string, required_uuid,
    transcript_store_from_stored,
};
use crate::config::Config;
use crate::provider_runtime::{
    current_pi_template, effective_prompt_profile, mcp_snapshot_for_session,
    provider_tools_for_session, PromptProfile, ProviderConnectionRegistry, SessionTitleScheduler,
};
use crate::runtime::*;
use crate::runtime_hosts::RuntimeRegistry;
use crate::state::AppState;
use crate::types::*;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, Arc, Mutex as StdMutex};
use std::time::Instant;

use agent_core::AgentInput;
use agent_session::SessionInput;
use agent_store::{
    ActionKind, ActionStatus, ActionUpdate, CompactionTrigger, EventType, InputPriority,
    PostgresAgentStore, ProjectWorkspace, QueuedInputStatus, SessionConfig, SubagentType,
    TranscriptEntryBodyMode, TranscriptEntryScope,
};
use agent_tools::ToolRegistry;
use agent_vocab::{ActionId, ProviderConfig, ProviderKind, TranscriptItem, TurnId, TurnOutcome};
use anyhow::Result;
use futures_util::{stream, SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

const BOOT_RECOVERY_CONCURRENCY: usize = 4;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env_and_args()?;
    let prompt_root = find_prompt_root(std::env::current_dir()?)?;
    let repo = Arc::new(PostgresAgentStore::connect(&config.database_url).await?);
    repo.migrate().await?;

    let (events, _) = broadcast::channel(1024);
    let runtime_hosts = RuntimeRegistry::new(repo.clone());
    tokio::spawn(runtime_hosts.clone().listen(config.runtime_bind.clone()));
    let state = AppState {
        repo,
        active: Arc::new(Mutex::new(HashMap::new())),
        session_driver_locks: Arc::new(Mutex::new(HashMap::new())),
        tasks: Arc::new(StdMutex::new(HashMap::new())),
        auxiliary_tasks: Arc::new(StdMutex::new(Vec::new())),
        task_registration_lock: Arc::new(StdMutex::new(())),
        post_compaction_recovery_scheduled: Arc::new(AtomicBool::new(false)),
        post_compaction_recovery_notify: Arc::new(tokio::sync::Notify::new()),
        post_compaction_recovery_task: Arc::new(StdMutex::new(None)),
        shutting_down: Arc::new(AtomicBool::new(false)),
        events,
        tools: Arc::new(ToolRegistry::with_builtin_tools()),
        provider_connections: ProviderConnectionRegistry::new(),
        session_titles: SessionTitleScheduler::default(),
        runtime_hosts: runtime_hosts.clone(),
        prompt_root,
        daemon_config: config.daemon_config,
        #[cfg(test)]
        pause_subagent_control_after_commit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        #[cfg(test)]
        subagent_control_committed: Arc::new(tokio::sync::Notify::new()),
        #[cfg(test)]
        fail_subagent_control_reload_after_commit: Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )),
    };

    // Combined controls capture an exact unfinished child action generation.
    // Reconcile them before post-compaction recovery so a committed interrupt
    // cannot race a newly registered resumed model runner. Each worker runs
    // under the exact child's SessionDriver; failures remain durable for the
    // periodic reconciler below.
    match state
        .repo
        .sessions_with_recoverable_subagent_controls()
        .await
    {
        Ok(session_ids) => {
            if !session_ids.is_empty() {
                eprintln!(
                    "reconciling {} session(s) with recoverable subagent control(s) before stale marking",
                    session_ids.len()
                );
            }
            stream::iter(session_ids.into_iter().map(|session_id| {
                let state = state.clone();
                async move {
                    let driver = SessionDriver::acquire(&state, &session_id).await;
                    (
                        session_id,
                        driver.reconcile_pending_subagent_controls().await,
                    )
                }
            }))
            .buffer_unordered(BOOT_RECOVERY_CONCURRENCY)
            .for_each(|(session_id, result)| async move {
                if let Err(error) = result {
                    eprintln!(
                        "boot combined-control reconciliation failed session={session_id}: {}: {}",
                        error.code, error.message
                    );
                }
            })
            .await;
        }
        Err(error) => eprintln!("failed to sweep recoverable subagent controls on boot: {error:#}"),
    }

    // Recover post-compaction intents only after exact-child controls have had
    // the opportunity to cancel their captured action generation. The stale
    // sweep follows both recovery passes and protects any durable work that
    // remains retryable after a transient recovery failure.
    match recover_post_compaction_dispatches_on_boot(&state).await {
        Ok(resumed_compactions) if resumed_compactions > 0 => {
            eprintln!(
                "recovered {resumed_compactions} committed post-compaction dispatch intent(s)"
            );
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!(
                "initial post-compaction dispatch recovery failed; watchdog will retry: {error:#}"
            );
        }
    }

    let stale_actions = state.repo.mark_all_unfinished_actions_stale().await?;
    if stale_actions > 0 {
        eprintln!("marked {stale_actions} abandoned action(s) stale");
    }

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
    spawn_pending_subagent_control_sweeper(&state);
    match state.repo.sessions_with_active_queued_inputs().await {
        Ok(session_ids) => {
            if !session_ids.is_empty() {
                eprintln!(
                    "resuming {} session(s) with active queued input(s)",
                    session_ids.len()
                );
            }
            for session_id in session_ids {
                spawn_drive_until_blocked(&state, session_id, "boot.active_queued_input");
            }
        }
        Err(error) => eprintln!("failed to sweep queued inputs on boot: {error:#}"),
    }

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

fn spawn_pending_subagent_control_sweeper(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut failures = HashMap::<String, (u32, Instant)>::new();
        let mut query_failure = (0_u32, Instant::now());
        loop {
            interval.tick().await;
            match state
                .repo
                .sessions_with_recoverable_subagent_controls()
                .await
            {
                Ok(session_ids) => {
                    query_failure = (0, Instant::now());
                    let pending = session_ids.iter().cloned().collect::<BTreeSet<_>>();
                    failures.retain(|session_id, _| pending.contains(session_id));
                    for session_id in session_ids {
                        let Some(result) =
                            reconcile_recoverable_subagent_control_session(&state, &session_id)
                                .await
                        else {
                            continue;
                        };
                        match result {
                            Ok(()) => {
                                failures.remove(&session_id);
                            }
                            Err(error) => {
                                let now = Instant::now();
                                let (count, next_report) =
                                    failures.entry(session_id.clone()).or_insert((0, now));
                                *count = count.saturating_add(1);
                                if now >= *next_report {
                                    eprintln!(
                                        "pending-control sweep failed session={session_id} attempt={count}: {}: {}",
                                        error.code, error.message
                                    );
                                    let delay =
                                        1_u64.checked_shl((*count).min(6)).unwrap_or(64).min(60);
                                    *next_report = now + Duration::from_secs(delay);
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    let now = Instant::now();
                    query_failure.0 = query_failure.0.saturating_add(1);
                    if now >= query_failure.1 {
                        eprintln!(
                            "failed to discover recoverable subagent controls attempt={}: {error:#}",
                            query_failure.0
                        );
                        let delay = 1_u64
                            .checked_shl(query_failure.0.min(6))
                            .unwrap_or(64)
                            .min(60);
                        query_failure.1 = now + Duration::from_secs(delay);
                    }
                }
            }
        }
    });
}

/// Reconcile one session discovered by the bounded live control query. A held
/// driver is skipped rather than queued behind, so ticks and replay nudges
/// coalesce around the current owner.
async fn reconcile_recoverable_subagent_control_session(
    state: &AppState,
    session_id: &str,
) -> Option<std::result::Result<(), RpcError>> {
    let driver = SessionDriver::try_acquire(state, session_id).await?;
    Some(
        async {
            driver.reconcile_pending_subagent_controls().await?;
            driver.recover_if_needed().await?;
            if state.repo.has_queued_inputs(session_id).await?
                && !state.repo.has_unfinished_actions(session_id).await?
            {
                driver.drive_until_blocked().await?;
            }
            Ok(())
        }
        .await,
    )
}

#[cfg(test)]
async fn sweep_pending_subagent_controls_once(
    state: &AppState,
) -> std::result::Result<(), RpcError> {
    for session_id in state
        .repo
        .sessions_with_recoverable_subagent_controls()
        .await?
    {
        if let Some(result) =
            reconcile_recoverable_subagent_control_session(state, &session_id).await
        {
            result?;
        }
    }
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
    let mut handles = take_tasks(state);
    if handles.is_empty() {
        return;
    }
    let drain = async {
        for handle in &mut handles {
            if let Err(error) = handle.await {
                eprintln!("dispatch task join error: {error}");
            }
        }
    };
    if timeout(Duration::from_secs(15), drain).await.is_err() {
        eprintln!("timed out waiting for dispatch tasks during shutdown");
        for handle in handles {
            handle.abort();
        }
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
                            let mut cursor = after;
                            loop {
                                let page = state
                                    .repo
                                    .events_after_page(&session_id, cursor, None)
                                    .await?;
                                let next_cursor = page.next_after_event_id;
                                for event in page.events {
                                    if event.event_id
                                        <= event_high_water
                                            .get(&session_id)
                                            .copied()
                                            .unwrap_or_default()
                                    {
                                        continue;
                                    }
                                    event_high_water.insert(session_id.clone(), event.event_id);
                                    writer
                                        .send(Message::Text(
                                            serde_json::to_string(&event)?.into(),
                                        ))
                                        .await?;
                                }
                                if !page.has_more {
                                    break;
                                }
                                cursor = Some(
                                    next_cursor
                                        .expect("event replay page with more rows has a cursor"),
                                );
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
        RpcMethod::RuntimeList => runtime_list(state).await,
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
        RpcMethod::HistoryTargets => history_targets(state, params).await,
        RpcMethod::HistoryTree => history_tree(state, params).await,
        RpcMethod::HistoryContext => history_context(state, params).await,
        RpcMethod::HistorySwitch => history::switch(state, params).await,
        RpcMethod::HistoryFork => history_fork::fork(state, params).await,
        RpcMethod::TurnResume => turn_resume(state, params).await,
        RpcMethod::McpInventory => mcp_inventory(state, params).await,
        RpcMethod::McpStatus => mcp_auth::status(state, params).await,
        RpcMethod::McpLogin => mcp_auth::login(state, params).await,
        RpcMethod::McpComplete => mcp_auth::complete(state, params).await,
        RpcMethod::McpCancel => mcp_auth::cancel(state, params).await,
        RpcMethod::McpLogout => mcp_auth::logout(state, params).await,
        RpcMethod::ToolsList => tools_list(state, params).await,
        RpcMethod::CompactionRequest => compaction_request(state, params).await,
        RpcMethod::DelegationStartFull => delegation_tools::rpc_start_full(state, params).await,
        RpcMethod::DelegationStartReadonlyFanout => {
            delegation_tools::rpc_start_readonly_fanout(state, params).await
        }
        RpcMethod::DelegationStatus => delegation_tools::rpc_status(state, params).await,
        RpcMethod::DelegationCancel => delegation_tools::rpc_cancel(state, params).await,
        RpcMethod::DelegationSteerSubagent => {
            delegation_tools::rpc_steer_subagent(state, params).await
        }
        RpcMethod::DelegationList => delegation_tools::rpc_list(state, params).await,
        RpcMethod::DelegationReadHandoffFile => {
            delegation_tools::rpc_read_handoff_file(state, params).await
        }
        RpcMethod::HarnessModelComplete => harness_model_complete(state, params).await,
        RpcMethod::HarnessModelFail => harness_model_fail(state, params).await,
    }
}

async fn tools_list(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let provider = required_string(&params, "provider")?;
    let provider = provider.parse::<ProviderKind>().map_err(|error| {
        RpcError::new(
            "invalid_provider",
            format!("invalid provider for tools.list: {error}"),
        )
    })?;
    let profile = tools_list_profile(state, &params).await?;
    let mut tools = provider_tools_for_session(state, provider, profile)
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
    let Some(session_id) = params.get("session_id").and_then(Value::as_str) else {
        return Ok(json!({ "tools": tools }));
    };
    let config = state.repo.load_session_config(session_id).await?;
    let snapshot = mcp_snapshot_for_session(&config).map_err(|error| {
        RpcError::new(
            "corrupt_mcp_manifest",
            format!("stored MCP manifest failed validation: {error:#}"),
        )
    })?;
    // Per-tool rows are built from the runtime's live views (server, raw_name,
    // health, contract_fingerprint). If the runtime is offline the view map is
    // empty, so the MCP tools drop out of this listing entirely rather than
    // failing tools.list — an unreachable runtime can't execute them anyway, and
    // the non-MCP (first-party) rows still list normally.
    let views = state
        .runtime_hosts
        .mcp_tool_views(&config.runtime_id, snapshot.manifest().clone())
        .await
        .unwrap_or_default();
    let views = views
        .into_iter()
        .map(|view| (view.exposed_name.clone(), view))
        .collect::<BTreeMap<_, _>>();
    tools.extend(
        snapshot
            .manifest()
            .provider_tools(provider)
            .iter()
            .filter(|tool| snapshot.manifest().tool(&tool.name).is_some())
            .filter_map(|tool| views.get(&tool.name).map(|view| (tool, view)))
            .map(|(tool, view)| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                    "canonical_name": tool.canonical_name,
                    "prompt_alias": tool.prompt_alias,
                    "execution": tool.execution,
                    "kind": "mcp_tool",
                    "source": "mcp",
                    "server": view.server,
                    "raw_name": view.raw_name,
                    "manifest_fingerprint": snapshot.manifest_fingerprint(),
                    "contract_fingerprint": view.contract_fingerprint,
                    "health": view.health,
                })
            }),
    );
    Ok(json!({ "tools": tools }))
}

async fn mcp_inventory(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let runtime_id = required_string(&params, "runtime_id")?;
    let provider = required_string(&params, "provider")?;
    let provider = provider.parse::<ProviderKind>().map_err(|error| {
        RpcError::new(
            "invalid_provider",
            format!("invalid provider for mcp.inventory: {error}"),
        )
    })?;
    let inventory = state
        .runtime_hosts
        .mcp_inventory(
            &runtime_id,
            provider,
            crate::provider_runtime::first_party_toolsets(state, PromptProfile::Parent),
        )
        .await
        .map_err(crate::mcp_auth::map_runtime_mcp_error)?;
    serde_json::to_value(inventory)
        .map_err(anyhow::Error::from)
        .map_err(RpcError::from)
}

async fn tools_list_profile(
    state: &AppState,
    params: &Value,
) -> std::result::Result<PromptProfile, RpcError> {
    if let Some(session_id) = params.get("session_id").and_then(Value::as_str) {
        return prompt_profile_for_session(state, session_id).await;
    }
    Ok(match params.get("prompt_profile").and_then(Value::as_str) {
        Some("subagent") => PromptProfile::Subagent,
        _ => PromptProfile::Parent,
    })
}

async fn prompt_profile_for_session(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<PromptProfile, RpcError> {
    let config = state.repo.load_session_config(session_id).await?;
    Ok(effective_prompt_profile(state, &config, session_id).await?)
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
    if !state.repo.session_exists(&session_id).await? {
        return Err(RpcError::new("session_not_found", "session not found"));
    }
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
        let deleted = state
            .repo
            .delete_session(candidate_session_id)
            .await
            .map_err(map_source_mutation_error)?;
        if !deleted && candidate_session_id == &session_id {
            return Err(RpcError::new("session_not_found", "session not found"));
        }
        if let Err(error) = state
            .runtime_hosts
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
    if let Some(subagent_type) = state.repo.session_subagent_type(&session_id).await? {
        preserve_subagent_metadata(&mut metadata, &current.metadata, subagent_type);
    }
    let model_changed = provider_model_changed(&current.provider, &provider);
    let metadata_changed = metadata != current.metadata;
    if model_changed {
        driver.ensure_idle_for_source_mutation().await?;
    } else if metadata_changed {
        driver.ensure_idle_for_metadata_mutation().await?;
    }
    let config = SessionConfig {
        project_id: current.project_id,
        runtime_id: current.runtime_id.clone(),
        workspace_id: current.workspace_id.clone(),
        workspaces: current.workspaces.clone(),
        system_prompt: current.system_prompt.clone(),
        provider,
        metadata,
        mcp_manifest: current.mcp_manifest.clone(),
    };
    let events = state.repo.configure_session(&session_id, &config).await?;
    // Refresh non-provider session state while the active runtime retains the
    // immutable route captured for its open turn.
    replace_active_session_config(state, &session_id, config.clone()).await;
    if model_changed {
        state.provider_connections.remove_session(&session_id).await;
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

fn preserve_subagent_metadata(metadata: &mut Value, current: &Value, subagent_type: SubagentType) {
    let map = ensure_metadata_object(metadata);
    map.insert("prompt_profile".to_string(), json!("subagent"));
    map.insert("subagent".to_string(), json!(true));
    map.insert("hidden".to_string(), json!(true));
    map.insert("subagent_type".to_string(), json!(subagent_type.as_str()));
    if let Some(value) = current.get("role_name") {
        map.insert("role_name".to_string(), value.clone());
    } else {
        map.remove("role_name");
    }
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

async fn runtime_list(state: &AppState) -> std::result::Result<Value, RpcError> {
    Ok(json!({ "runtimes": state.runtime_hosts.list().await? }))
}

async fn project_create(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let params: ProjectWriteParams = from_params(params)?;
    let project_id = params.project_id.unwrap_or_else(Uuid::new_v4);
    let name = params.name.trim();
    if name.is_empty() {
        return Err(RpcError::new("invalid_params", "project name is required"));
    }
    state
        .runtime_hosts
        .require_available(&params.runtime_id)
        .await?;
    state
        .runtime_hosts
        .execute(
            &params.runtime_id,
            agent_runtime_protocol::RuntimeCommand::ValidateProject {
                workspaces: params.workspaces.clone(),
            },
        )
        .await?;
    let workspaces = params.workspaces;
    let project = state
        .repo
        .create_project(
            project_id,
            &params.runtime_id,
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
        state
            .runtime_hosts
            .execute(
                &current.runtime_id,
                agent_runtime_protocol::RuntimeCommand::ValidateProject {
                    workspaces: workspaces.clone(),
                },
            )
            .await?;
        workspaces
    } else {
        current.workspaces
    };
    let project = state
        .repo
        .update_project(project_id, name, &workspaces)
        .await?;
    if let Err(error) = state
        .runtime_hosts
        .reconcile_project_bases(project_id, &project.workspaces)
        .await
    {
        eprintln!("failed to reconcile workspace bases for project {project_id}: {error:#}");
    }
    Ok(rpc_views::project(project))
}

async fn project_delete(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let project_id = required_uuid(&params, "project_id")?;
    let project = state.repo.get_project(project_id).await?;
    let deleted = state.repo.delete_empty_project(project_id).await?;
    if !deleted {
        return Err(RpcError::new(
            "project_not_empty",
            "project was not found or still has sessions",
        ));
    }
    if let Err(error) = state
        .runtime_hosts
        .remove_project_bases(&project.runtime_id, project_id)
        .await
    {
        eprintln!("failed to remove workspace bases for project {project_id}: {error:#}");
    }
    Ok(json!({ "project_id": project_id, "deleted": true }))
}

#[derive(Debug, Deserialize)]
struct ProjectWriteParams {
    project_id: Option<Uuid>,
    name: String,
    runtime_id: String,
    #[serde(default)]
    workspaces: Vec<ProjectWorkspace>,
    metadata: Option<Value>,
}

async fn system_prompt(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("session_required", "system.prompt requires session_id"))?;
    let config = state.repo.load_session_config(session_id).await?;
    state
        .runtime_hosts
        .ensure_session(session_id, &config.workspace_id, &config.workspaces)
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
    let started_at = Instant::now();
    let driver = SessionDriver::acquire(state, &session_id).await;
    let acquired_ms = started_at.elapsed().as_millis();
    driver.recover_if_needed().await?;
    let recovered_ms = started_at.elapsed().as_millis();
    let after_event_id = params.get("after_event_id").and_then(Value::as_i64);
    subscriptions.insert(session_id.clone());
    let Some(after_event_id) = after_event_id else {
        let current = state.repo.last_event_id(&session_id).await?;
        let loaded_ms = started_at.elapsed().as_millis();
        event_high_water.insert(session_id.clone(), current);
        if perf_logging_enabled() {
            eprintln!(
                "perf events.subscribe session={session_id} after_event_id=None replayed=0 acquire_ms={acquired_ms} recover_ms={} load_ms={} view_ms=0 total_ms={loaded_ms}",
                recovered_ms.saturating_sub(acquired_ms),
                loaded_ms.saturating_sub(recovered_ms),
            );
        }
        return Ok(json!({
            "replayed": [],
            "has_more": false,
            "next_after_event_id": null,
        }));
    };
    let current_high_water = event_high_water
        .get(&session_id)
        .copied()
        .unwrap_or_default();
    event_high_water.insert(session_id.clone(), current_high_water.max(after_event_id));
    let page = state
        .repo
        .events_after_page(&session_id, Some(after_event_id), None)
        .await?;
    let loaded_ms = started_at.elapsed().as_millis();
    let replayed_count = page.events.len();
    let replayed_max = page
        .events
        .iter()
        .map(|event| event.event_id)
        .max()
        .unwrap_or(after_event_id);
    let current_high_water = event_high_water
        .get(&session_id)
        .copied()
        .unwrap_or_default();
    event_high_water.insert(session_id.clone(), current_high_water.max(replayed_max));
    let value = json!({
        "replayed": page.events,
        "has_more": page.has_more,
        "next_after_event_id": page.next_after_event_id,
    });
    let total_ms = started_at.elapsed().as_millis();
    if perf_logging_enabled() {
        eprintln!(
            "perf events.subscribe session={session_id} after_event_id={after_event_id} replayed={replayed_count} acquire_ms={acquired_ms} recover_ms={} load_ms={} view_ms={} total_ms={total_ms}",
            recovered_ms.saturating_sub(acquired_ms),
            loaded_ms.saturating_sub(recovered_ms),
            total_ms.saturating_sub(loaded_ms),
        );
    }
    Ok(value)
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

pub(crate) fn spawn_drive_until_blocked(
    state: &AppState,
    session_id: String,
    reason: &'static str,
) {
    let state = state.clone();
    let task_state = state.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        let driver = SessionDriver::acquire(&state, &session_id).await;
        let drive_result = async {
            driver.reconcile_pending_subagent_controls().await?;
            driver.recover_if_needed().await?;
            if state.repo.has_queued_inputs(&session_id).await?
                && !state.repo.has_unfinished_actions(&session_id).await?
            {
                driver.drive_until_blocked().await?;
            }
            Ok::<(), RpcError>(())
        }
        .await;
        if let Err(error) = drive_result {
            eprintln!(
                "background drive failed session={session_id} reason={reason}: {}: {}",
                error.code, error.message
            );
            match state
                .repo
                .insert_event(
                    &session_id,
                    EventType::ModelError,
                    json!({
                        "error": error.message,
                        "reason": reason,
                    }),
                )
                .await
            {
                Ok(event) => publish_events(&state, vec![event]),
                Err(event_error) => eprintln!(
                    "failed to record background drive failure {session_id}: {event_error:#}"
                ),
            }
        }
    });
    let _ = crate::runtime::register_auxiliary_task(&task_state, handle, start_tx);
}

/// Best-effort control recovery nudge that never queues behind another owner.
/// Periodic reconciliation remains the durable retry path when the lock is busy.
pub(crate) fn spawn_try_drive_until_blocked(
    state: &AppState,
    session_id: String,
    reason: &'static str,
) {
    let state = state.clone();
    tokio::spawn(async move {
        let Some(driver) = SessionDriver::try_acquire(&state, &session_id).await else {
            return;
        };
        let drive_result = async {
            driver.reconcile_pending_subagent_controls().await?;
            driver.recover_if_needed().await?;
            if state.repo.has_queued_inputs(&session_id).await?
                && !state.repo.has_unfinished_actions(&session_id).await?
            {
                driver.drive_until_blocked().await?;
            }
            Ok::<(), RpcError>(())
        }
        .await;
        if let Err(error) = drive_result {
            eprintln!(
                "background nonwaiting drive failed session={session_id} reason={reason}: {}: {}",
                error.code, error.message
            );
        }
    });
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
        base_leaf_id: _base_leaf_id,
        expected_active_leaf_id,
    } = request;

    let started_at = Instant::now();
    if priority == InputPriority::Steer
        && state.repo.session_parent_id(&session_id).await?.is_some()
    {
        return Err(RpcError::new(
            "subagent_steer_requires_parent_scope",
            "steer_subagent must be used to steer delegation subagents",
        ));
    }
    let mut expected_params = json!({});
    if let Some(expected_active_leaf_id) = expected_active_leaf_id {
        expected_params["expected_active_leaf_id"] = expected_active_leaf_id;
    }

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
                    "perf input.follow_up session={session_id} priority={priority} replay=true total_ms={total_ms}",
                );
            }
            if matches!(
                record.status,
                QueuedInputStatus::Queued | QueuedInputStatus::Consuming
            ) && !state.repo.has_unfinished_actions(&session_id).await?
            {
                subagents::publish_subagent_parent_running_if_child(state, &session_id).await;
                spawn_drive_until_blocked(state, session_id.clone(), "input.follow_up.replay");
            }
            return Ok(json!({
                "input_id": record.input_id,
                "accepted": matches!(
                    record.status,
                    QueuedInputStatus::Queued
                        | QueuedInputStatus::Consuming
                        | QueuedInputStatus::Consumed
                ),
                "queued": matches!(
                    record.status,
                    QueuedInputStatus::Queued | QueuedInputStatus::Consuming
                ),
                "replayed": true,
                "queue": queue,
            }));
        }
    }

    let expected_active_leaf_id = parse_expected_active_leaf_id(&expected_params)?;
    let queued = state
        .repo
        .enqueue_user_input(
            &session_id,
            priority,
            &content,
            client_input_id.as_deref(),
            expected_active_leaf_id,
        )
        .await
        .map_err(map_queued_mutation_error)?;
    if let Some(event) = queued.event {
        publish_events(state, vec![event]);
    }
    let has_running = state.repo.has_unfinished_actions(&session_id).await?;
    if !has_running {
        subagents::publish_subagent_parent_running_if_child(state, &session_id).await;
        spawn_drive_until_blocked(state, session_id.clone(), "input.follow_up");
    }
    if perf_logging_enabled() {
        let total_ms = started_at.elapsed().as_millis();
        eprintln!(
            "perf input.follow_up session={session_id} priority={priority} queued=true background_drive={} total_ms={total_ms}",
            !has_running,
        );
    }
    Ok(json!({
        "input_id": queued.input_id,
        "accepted": true,
        "queued": true,
        "queue": queued.queue.map(rpc_views::queue_state),
    }))
}

fn parse_expected_active_leaf_id(
    params: &Value,
) -> std::result::Result<Option<Option<&str>>, RpcError> {
    let Some(expected) = params.get("expected_active_leaf_id") else {
        return Ok(None);
    };
    match expected {
        Value::Null => Ok(Some(None)),
        Value::String(value) => Ok(Some(Some(value.as_str()))),
        _ => Err(RpcError::new(
            "invalid_params",
            "expected_active_leaf_id must be a string or null",
        )),
    }
}

async fn input_promote_queued(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let session_id = required_string(&params, "session_id")?;
    if state.repo.session_parent_id(&session_id).await?.is_some() {
        return Err(RpcError::new(
            "subagent_steer_requires_parent_scope",
            "steer_subagent must be used to steer delegation subagents",
        ));
    }
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
    interrupt_session_with_driver(state, session_id, &driver, None).await
}

/// Canonical exact-session interrupt with an already-held driver.
///
/// Parent-scoped child controls call this form so they cannot deadlock by
/// reacquiring the same child driver. It intentionally contains no
/// parent/delegation traversal: task abort, durable interruption, and queue
/// driving all target `session_id` exactly.
pub(crate) async fn interrupt_session_with_driver(
    state: &AppState,
    session_id: &str,
    driver: &SessionDriver,
    _control_input_id: Option<&str>,
) -> std::result::Result<Value, RpcError> {
    let active = driver.active_session().await;
    let Some(active) = active else {
        let events = state
            .repo
            .cancel_unfinished_session_work(session_id, "session interrupted")
            .await?;
        if !events.is_empty() {
            // Durable action interruption must be followed by exact task abort
            // even if combined-control bookkeeping later fails.
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
    let dispatches = driver
        .apply_agent_input(active, AgentInput::Interrupt, None)
        .await?;
    let events = state
        .repo
        .cancel_unfinished_session_work(session_id, "session interrupted")
        .await?;
    // The driver lock prevents the task's completion handler from persisting
    // concurrently, so make the interrupt boundary durable before aborting the
    // exact session's runtime futures. Bookkeeping for a combined control must
    // not be able to skip that abort.
    let aborted_tasks = abort_session_tasks(state, session_id);
    publish_events(state, events);
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
    let started_at = Instant::now();
    let driver = SessionDriver::acquire(state, &session_id).await;
    let acquired_ms = started_at.elapsed().as_millis();
    driver.recover_if_needed().await?;
    let recovered_ms = started_at.elapsed().as_millis();
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
    let loaded_ms = started_at.elapsed().as_millis();
    let entry_count = result.entries.len();
    let value = rpc_views::transcript_turn_detail(result);
    let total_ms = started_at.elapsed().as_millis();
    if perf_logging_enabled() {
        eprintln!(
            "perf transcript.turn_detail session={session_id} card={card_id} leaf={leaf_id} start_sequence={start_sequence} end_sequence={end_sequence} entries={entry_count} acquire_ms={acquired_ms} recover_ms={} load_ms={} view_ms={} total_ms={total_ms}",
            recovered_ms.saturating_sub(acquired_ms),
            loaded_ms.saturating_sub(recovered_ms),
            total_ms.saturating_sub(loaded_ms),
        );
    }
    Ok(value)
}

async fn history_targets(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let params = history::parse_history_targets(params)?;
    let session_id = params.session_id;
    let driver = SessionDriver::acquire(state, &session_id).await;
    driver.recover_if_needed().await?;
    let result = state
        .repo
        .history_targets(&session_id, params.before_sequence, params.limit)
        .await?;
    Ok(rpc_views::history_targets(result))
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
    let stored = state.repo.load_stored_session_ui(&session_id).await?;
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
        .runtime_hosts
        .ensure_session(&session_id, &config.workspace_id, &config.workspaces)
        .await?;
    let created = state
        .repo
        .create_compaction_action(&session_id, CompactionTrigger::Manual)
        .await?;
    publish_events(state, created.events);
    let action_row_id = created.job.action_row_id.clone();
    spawn_compaction(state, session_id, created.job, config)
        .map_err(|_| RpcError::new("shutting_down", "daemon is shutting down"))?;
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
    let mut action = state
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
    if action.post_compaction_dispatch_context_leaf_id.is_some() {
        if action.post_compaction_dispatch_lease.is_none() {
            let claimed = state
                .repo
                .claim_post_compaction_model_action(
                    &agent_store::PostCompactionDispatchIntent {
                        session_id: session_id.clone(),
                        row_id: action_row_id.clone(),
                        attempt_id: action.attempt_id.clone(),
                    },
                    agent_store::POST_COMPACTION_DISPATCH_LEASE_DURATION,
                )
                .await
                .map_err(|error| RpcError::new("stale_action", error.to_string()))?
                .ok_or_else(|| {
                    RpcError::new(
                        "stale_action",
                        "post-compaction model action has a live owner",
                    )
                })?;
            action.post_compaction_dispatch_lease = Some(claimed.lease);
        }
    } else {
        state
            .repo
            .claim_pending_model_action(&session_id, &action_row_id, &action.attempt_id)
            .await?;
    }
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
                post_compaction_dispatch_lease: action.post_compaction_dispatch_lease,
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
    let mut action = state
        .repo
        .load_harness_model_action(&session_id, &action_row_id)
        .await
        .map_err(|error| RpcError::new("stale_action", error.to_string()))?;
    if action.post_compaction_dispatch_context_leaf_id.is_some() {
        if action.post_compaction_dispatch_lease.is_none() {
            let claimed = state
                .repo
                .claim_post_compaction_model_action(
                    &agent_store::PostCompactionDispatchIntent {
                        session_id: session_id.clone(),
                        row_id: action_row_id.clone(),
                        attempt_id: action.attempt_id.clone(),
                    },
                    agent_store::POST_COMPACTION_DISPATCH_LEASE_DURATION,
                )
                .await
                .map_err(|error| RpcError::new("stale_action", error.to_string()))?
                .ok_or_else(|| {
                    RpcError::new(
                        "stale_action",
                        "post-compaction model action has a live owner",
                    )
                })?;
            action.post_compaction_dispatch_lease = Some(claimed.lease);
        }
    } else {
        state
            .repo
            .claim_pending_model_action(&session_id, &action_row_id, &action.attempt_id)
            .await?;
    }
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
                post_compaction_dispatch_lease: action.post_compaction_dispatch_lease,
                status: ActionStatus::Error,
                result: json!({ "error": error }),
            }),
        )
        .await?;
    driver.dispatch(dispatches).await?;
    driver.drive_until_blocked().await?;
    Ok(json!({ "failed": true }))
}
