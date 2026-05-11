use std::sync::Arc;

use agent_session::{
    AgentInput, AgentSession, SessionAction, SessionEvent, SessionInput, ToolResultMessage,
    TranscriptStorageNode, UserMessage,
};
use agent_store::{
    AcceptedInput, ActionStatus, ActionUpdate, DispatchAction, EventFrame, EventType,
    InputPriority, QueuedInput,
};
use anyhow::Context;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::codec::transcript_store_from_stored;
use crate::provider_runtime::run_model;
use crate::state::AppState;
use crate::types::{RpcError, RuntimeSession};

pub(crate) async fn ensure_idle_for_source_mutation(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<(), RpcError> {
    recover_if_needed(state, session_id).await?;
    if state.active.lock().await.contains_key(session_id)
        || state
            .repo
            .has_unfinished_actions(session_id)
            .await
            .map_err(anyhow::Error::from)?
        || state
            .repo
            .has_queued_inputs(session_id)
            .await
            .map_err(anyhow::Error::from)?
    {
        return Err(RpcError::new(
            "session_busy",
            "source-mutating history operations require an idle session",
        ));
    }
    Ok(())
}

pub(crate) async fn ensure_expected_active_leaf(
    state: &AppState,
    session_id: &str,
    params: &Value,
) -> std::result::Result<(), RpcError> {
    if params.get("expected_active_leaf_id").is_none() {
        return Ok(());
    }
    let stored = state
        .repo
        .load_stored_session(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    ensure_expected_active_leaf_matches(&stored.active_leaf_id, params)
}

pub(crate) fn ensure_expected_active_leaf_matches(
    current: &Option<String>,
    params: &Value,
) -> std::result::Result<(), RpcError> {
    let Some(expected) = params.get("expected_active_leaf_id") else {
        return Ok(());
    };
    let expected = match expected {
        Value::Null => None,
        Value::String(value) => Some(value.as_str()),
        _ => {
            return Err(RpcError::new(
                "invalid_params",
                "expected_active_leaf_id must be a string or null",
            ))
        }
    };
    if current.as_deref() != expected {
        return Err(RpcError::new(
            "history_changed",
            "session active leaf changed before the request was applied",
        ));
    }
    Ok(())
}

pub(crate) async fn ensure_active_loaded(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<(), RpcError> {
    if state.active.lock().await.contains_key(session_id) {
        return Ok(());
    }
    let config = state
        .repo
        .load_session_config(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let stored = state
        .repo
        .load_stored_session(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let session = AgentSession::from_stored_session(stored)
        .map_err(|error| RpcError::new("invalid_transcript", format!("{error:?}")))?;
    state.active.lock().await.insert(
        session_id.to_string(),
        Arc::new(Mutex::new(RuntimeSession { session, config })),
    );
    Ok(())
}

pub(crate) async fn recover_if_needed(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<(), RpcError> {
    if state.active.lock().await.contains_key(session_id) {
        return Ok(());
    }
    state
        .repo
        .reset_abandoned_consuming_inputs(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let stored = state
        .repo
        .load_stored_session(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let store = transcript_store_from_stored(&stored)?;
    if store.is_turn_boundary()
        && !state
            .repo
            .has_unfinished_actions(session_id)
            .await
            .map_err(anyhow::Error::from)?
    {
        return Ok(());
    }
    let recovered = AgentSession::from_stored_session(stored.clone())
        .map_err(|error| RpcError::new("invalid_transcript", format!("{error:?}")))?;
    let recovered_stored = recovered.to_stored_session(session_id);
    let new_entries = recovered_stored
        .entries
        .iter()
        .skip(stored.entries.len())
        .cloned()
        .collect::<Vec<_>>();
    let events = state
        .repo
        .recover_session(
            session_id,
            &new_entries,
            recovered_stored.active_leaf_id.as_deref(),
        )
        .await
        .map_err(anyhow::Error::from)?;
    publish_events(state, events);
    Ok(())
}

pub(crate) async fn session_pump_lock(state: &AppState, session_id: &str) -> Arc<Mutex<()>> {
    let mut locks = state.pump_locks.lock().await;
    locks
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

pub(crate) fn agent_input_from_queued_priority(
    priority: InputPriority,
    content: UserMessage,
) -> AgentInput {
    match priority {
        InputPriority::Steer => AgentInput::steer_message(content),
        InputPriority::FollowUp => AgentInput::follow_up_message(content),
    }
}

pub(crate) fn collect_runtime_outputs(
    runtime: &mut RuntimeSession,
) -> (
    Vec<TranscriptStorageNode>,
    Vec<SessionEvent>,
    Vec<SessionAction>,
    Option<String>,
) {
    runtime.session.drive();
    let events = runtime.session.drain_events();
    let actions = runtime.session.drain_actions();
    let mut entries = Vec::new();
    for event in &events {
        if let SessionEvent::TranscriptItemAppended { entry_id, .. } = event {
            if let Some(entry) = runtime.session.transcript_store().get_entry(entry_id) {
                entries.push(entry.clone());
            }
        }
    }
    let active_leaf_id = runtime
        .session
        .transcript_store()
        .leaf_id()
        .map(str::to_string);
    (entries, events, actions, active_leaf_id)
}

pub(crate) fn map_queued_mutation_error(error: anyhow::Error) -> RpcError {
    if error
        .to_string()
        .contains("queued input is no longer editable")
    {
        RpcError::new("input_already_consuming", error.to_string())
    } else {
        error.into()
    }
}

pub(crate) async fn pump_session(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<Vec<DispatchAction>, RpcError> {
    let pump_lock = session_pump_lock(state, session_id).await;
    let _pump_guard = pump_lock.lock().await;
    ensure_active_loaded(state, session_id).await?;
    let mut dispatched_all = Vec::new();
    loop {
        let active = { state.active.lock().await.get(session_id).cloned() };
        let Some(active) = active else { break };
        let dispatched =
            persist_active_outputs(state, session_id, active.clone(), None, None, None).await?;
        let has_dispatched_work = !dispatched.is_empty();
        dispatched_all.extend(dispatched.clone());
        dispatch_all(state, session_id, dispatched);
        if has_dispatched_work {
            break;
        }

        if state
            .repo
            .has_unfinished_actions(session_id)
            .await
            .map_err(anyhow::Error::from)?
        {
            break;
        }

        let maybe_input = state
            .repo
            .take_next_queued_input(session_id)
            .await
            .map_err(anyhow::Error::from)?;
        if let Some(queued) = maybe_input {
            let agent_input =
                agent_input_from_queued_priority(queued.priority, queued.content.clone());
            let active = { state.active.lock().await.get(session_id).cloned() };
            if let Some(active) = active {
                let enqueue_result = {
                    let mut runtime = active.lock().await;
                    runtime.session.enqueue_input(agent_input)
                };
                if let Err(error) = enqueue_result {
                    state
                        .repo
                        .reset_consuming_input(session_id, &queued.id, &queued.claim_id)
                        .await
                        .map_err(anyhow::Error::from)?;
                    return Err(RpcError::new("invalid_input", error.to_string()));
                }
                let dispatched =
                    persist_active_outputs(state, session_id, active, None, Some(queued), None)
                        .await?;
                let has_dispatched_work = !dispatched.is_empty();
                dispatched_all.extend(dispatched.clone());
                dispatch_all(state, session_id, dispatched);
                if has_dispatched_work {
                    break;
                }
            }
            continue;
        }

        state.active.lock().await.remove(session_id);
        let event = state
            .repo
            .insert_event(session_id, EventType::SessionIdle, json!({}))
            .await
            .map_err(anyhow::Error::from)?;
        publish_events(state, vec![event]);
        break;
    }
    Ok(dispatched_all)
}

pub(crate) async fn apply_agent_input(
    state: &AppState,
    session_id: &str,
    active: Arc<Mutex<RuntimeSession>>,
    input: AgentInput,
    action_update: Option<ActionUpdate>,
    context_tokens: Option<usize>,
) -> std::result::Result<Vec<DispatchAction>, RpcError> {
    if let Some(update) = &action_update {
        if !state
            .repo
            .action_can_complete(session_id, &update.row_id, &update.attempt_id)
            .await
            .map_err(anyhow::Error::from)
            .context("check action can complete")?
        {
            return Err(RpcError::new(
                "stale_action",
                "action attempt is no longer running",
            ));
        }
    }
    {
        let mut runtime = active.lock().await;
        match input {
            AgentInput::ModelCompleted {
                action_id,
                turn_id,
                assistant,
            } => runtime
                .session
                .enqueue_session_input(SessionInput::ModelCompleted {
                    action_id,
                    turn_id,
                    assistant,
                    context_tokens,
                })
                .map_err(|error| RpcError::new("invalid_input", error.to_string()))?,
            other => runtime
                .session
                .enqueue_input(other)
                .map_err(|error| RpcError::new("invalid_input", error.to_string()))?,
        }
    }
    persist_active_outputs(state, session_id, active, action_update, None, None).await
}

pub(crate) async fn persist_active_outputs(
    state: &AppState,
    session_id: &str,
    active: Arc<Mutex<RuntimeSession>>,
    action_update: Option<ActionUpdate>,
    consumed_input: Option<QueuedInput>,
    accepted_input: Option<AcceptedInput>,
) -> std::result::Result<Vec<DispatchAction>, RpcError> {
    let (entries, events, actions, active_leaf_id, config) = {
        let mut runtime = active.lock().await;
        let (entries, events, actions, active_leaf_id) = collect_runtime_outputs(&mut runtime);
        (
            entries,
            events,
            actions,
            active_leaf_id,
            runtime.config.clone(),
        )
    };
    let persisted = state
        .repo
        .persist_outputs(
            session_id,
            &entries,
            active_leaf_id.as_deref(),
            &events,
            &actions,
            action_update,
            consumed_input,
            accepted_input,
            &config,
        )
        .await;
    let (frames, dispatch) = match persisted {
        Ok(persisted) => persisted,
        Err(error) => {
            state.active.lock().await.remove(session_id);
            return Err(anyhow::Error::from(error).into());
        }
    };
    publish_events(state, frames);
    Ok(dispatch)
}

pub(crate) fn dispatch_all(state: &AppState, session_id: &str, dispatches: Vec<DispatchAction>) {
    for dispatch in dispatches {
        spawn_dispatch(state.clone(), session_id.to_string(), dispatch);
    }
}

fn spawn_dispatch(state: AppState, session_id: String, dispatch: DispatchAction) {
    match dispatch.action.clone() {
        SessionAction::RequestModel {
            action_id,
            turn_id,
            model_context,
            ..
        } => {
            if dispatch.config.harness() {
                return;
            }
            tokio::spawn(async move {
                let result = run_model(&state, &dispatch.config, model_context).await;
                let active = { state.active.lock().await.get(&session_id).cloned() };
                let Some(active) = active else {
                    let _ = state
                        .repo
                        .mark_action_stale(&session_id, &dispatch.row_id)
                        .await;
                    return;
                };
                match result {
                    Ok(assistant) => {
                        if let Ok(dispatches) = apply_agent_input(
                            &state,
                            &session_id,
                            active,
                            AgentInput::ModelCompleted {
                                action_id,
                                turn_id,
                                assistant,
                            },
                            Some(ActionUpdate {
                                row_id: dispatch.row_id.clone(),
                                attempt_id: dispatch.attempt_id.clone(),
                                status: ActionStatus::Completed,
                                result: json!({ "source": "provider" }),
                            }),
                            None,
                        )
                        .await
                        {
                            dispatch_all(&state, &session_id, dispatches);
                            let _ = pump_session(&state, &session_id).await;
                        }
                    }
                    Err(error) => {
                        if let Ok(dispatches) = apply_agent_input(
                            &state,
                            &session_id,
                            active,
                            AgentInput::ModelFailed {
                                action_id,
                                turn_id,
                                error: error.to_string(),
                            },
                            Some(ActionUpdate {
                                row_id: dispatch.row_id.clone(),
                                attempt_id: dispatch.attempt_id.clone(),
                                status: ActionStatus::Error,
                                result: json!({ "error": error.to_string() }),
                            }),
                            None,
                        )
                        .await
                        {
                            dispatch_all(&state, &session_id, dispatches);
                            let _ = pump_session(&state, &session_id).await;
                        }
                    }
                }
            });
        }
        SessionAction::RequestTool {
            action_id,
            turn_id,
            tool_call,
        } => {
            tokio::spawn(async move {
                let started = state
                    .repo
                    .mark_action_running_and_event(
                        &session_id,
                        &dispatch.row_id,
                        &dispatch.attempt_id,
                        EventType::ToolStarted,
                    )
                    .await;
                match started {
                    Ok(events) if events.is_empty() => return,
                    Ok(events) => publish_events(&state, events),
                    Err(_) => return,
                }
                let result = match state.tools.execute(&tool_call, &state.tool_context).await {
                    Ok(result) => result,
                    Err(error) => ToolResultMessage::error(
                        tool_call.id.clone(),
                        tool_call.tool_name.clone(),
                        error.to_string(),
                    ),
                };
                let status = if matches!(result.status, agent_session::ToolResultStatus::Success) {
                    ActionStatus::Completed
                } else {
                    ActionStatus::Error
                };
                let active = { state.active.lock().await.get(&session_id).cloned() };
                if let Some(active) = active {
                    if let Ok(dispatches) = apply_agent_input(
                        &state,
                        &session_id,
                        active,
                        AgentInput::ToolCompleted {
                            action_id,
                            turn_id,
                            result: result.clone(),
                        },
                        Some(ActionUpdate {
                            row_id: dispatch.row_id.clone(),
                            attempt_id: dispatch.attempt_id.clone(),
                            status,
                            result: serde_json::to_value(&result).unwrap_or_else(|_| json!({})),
                        }),
                        None,
                    )
                    .await
                    {
                        dispatch_all(&state, &session_id, dispatches);
                        let _ = pump_session(&state, &session_id).await;
                    }
                }
            });
        }
        SessionAction::RequestCompaction { .. } | SessionAction::CancelSessionWork => {}
    }
}

pub(crate) fn publish_events(state: &AppState, events: Vec<EventFrame>) {
    for event in events {
        let _ = state.events.send(event);
    }
}
