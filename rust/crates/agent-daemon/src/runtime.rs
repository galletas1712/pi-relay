use std::sync::Arc;

use agent_core::AgentInput;
use agent_session::{
    AgentSession, HistoryOperationError, SessionAction, SessionEvent, SessionInput,
    TranscriptStorageNode, TranscriptStoreError,
};
use agent_store::{
    AcceptedInput, ActionStatus, ActionUpdate, CompactionJob, EventFrame, EventType, InputPriority,
    OutputBatch, PersistedAction, QueueMutationError, QueuedInput, SessionActivity, SessionConfig,
};
use agent_vocab::{
    ProviderReplayItem, ToolResultMessage, ToolResultStatus, TranscriptItem, UserMessage,
};
use anyhow::Context;
use serde_json::{json, Value};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::codec::transcript_store_from_stored;
use crate::provider_runtime::{run_compaction, run_model};
use crate::state::AppState;
use crate::types::{DispatchAction, RpcError, RuntimeSession};

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

async fn session_driver_lock(state: &AppState, session_id: &str) -> Arc<Mutex<()>> {
    let mut locks = state.session_driver_locks.lock().await;
    locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    locks
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

pub(crate) struct SessionDriver {
    state: AppState,
    session_id: String,
    _guard: OwnedMutexGuard<()>,
}

impl SessionDriver {
    pub(crate) async fn acquire(state: &AppState, session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        let lock = session_driver_lock(state, &session_id).await;
        let guard = lock.lock_owned().await;
        Self {
            state: state.clone(),
            session_id,
            _guard: guard,
        }
    }

    pub(crate) async fn ensure_idle_for_source_mutation(
        &self,
    ) -> std::result::Result<(), RpcError> {
        self.recover_if_needed().await?;
        if self
            .state
            .active
            .lock()
            .await
            .contains_key(&self.session_id)
            || self
                .state
                .repo
                .has_unfinished_actions(&self.session_id)
                .await
                .map_err(anyhow::Error::from)?
            || self
                .state
                .repo
                .has_queued_inputs(&self.session_id)
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

    pub(crate) async fn recover_if_needed(&self) -> std::result::Result<(), RpcError> {
        if self
            .state
            .active
            .lock()
            .await
            .contains_key(&self.session_id)
        {
            return Ok(());
        }
        self.state
            .repo
            .reset_abandoned_consuming_inputs(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let stored = self
            .state
            .repo
            .load_stored_session(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let store = transcript_store_from_stored(&stored)?;
        if store.is_turn_boundary() {
            return Ok(());
        }
        let recovered = AgentSession::from_stored_session(stored.clone())
            .map_err(|error| RpcError::new("invalid_transcript", format!("{error:?}")))?;
        let recovered_stored = recovered.to_stored_session(&self.session_id);
        let new_entries = recovered_stored
            .entries
            .iter()
            .skip(stored.entries.len())
            .cloned()
            .collect::<Vec<_>>();
        let events = self
            .state
            .repo
            .recover_session(
                &self.session_id,
                &new_entries,
                recovered_stored.active_leaf_id.as_deref(),
            )
            .await
            .map_err(anyhow::Error::from)?;
        publish_events(&self.state, events);
        clear_event_buffer_if_idle(&self.state, &self.session_id).await?;
        Ok(())
    }

    pub(crate) async fn ensure_active_loaded(&self) -> std::result::Result<(), RpcError> {
        if self
            .state
            .active
            .lock()
            .await
            .contains_key(&self.session_id)
        {
            return Ok(());
        }
        let config = self
            .state
            .repo
            .load_session_config(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let stored = self
            .state
            .repo
            .load_stored_session(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let session = AgentSession::from_stored_session(stored)
            .map_err(|error| RpcError::new("invalid_transcript", format!("{error:?}")))?;
        self.state.active.lock().await.insert(
            self.session_id.clone(),
            Arc::new(Mutex::new(RuntimeSession { session, config })),
        );
        Ok(())
    }

    pub(crate) async fn active_session(&self) -> Option<Arc<Mutex<RuntimeSession>>> {
        self.state
            .active
            .lock()
            .await
            .get(&self.session_id)
            .cloned()
    }

    pub(crate) async fn require_active_session(
        &self,
        code: &str,
        message: &str,
    ) -> std::result::Result<Arc<Mutex<RuntimeSession>>, RpcError> {
        self.active_session()
            .await
            .ok_or_else(|| RpcError::new(code, message))
    }

    pub(crate) async fn drive_until_blocked(
        &self,
    ) -> std::result::Result<Vec<DispatchAction>, RpcError> {
        self.ensure_active_loaded().await?;
        let mut dispatched_all = Vec::new();
        loop {
            let active = self.active_session().await;
            let Some(active) = active else { break };
            if let Some(dispatched) = self.consume_ready_steer(active.clone()).await? {
                let has_dispatched_work = !dispatched.is_empty();
                dispatched_all.extend(dispatched.clone());
                self.dispatch(dispatched);
                if has_dispatched_work {
                    break;
                }
                continue;
            }
            let dispatched = self
                .persist_active_outputs(active.clone(), None, None, None, Vec::new())
                .await?;
            let has_dispatched_work = !dispatched.is_empty();
            dispatched_all.extend(dispatched.clone());
            self.dispatch(dispatched);
            if has_dispatched_work {
                break;
            }

            if self
                .state
                .repo
                .has_unfinished_actions(&self.session_id)
                .await
                .map_err(anyhow::Error::from)?
            {
                break;
            }

            let maybe_input = self
                .state
                .repo
                .take_next_queued_input(&self.session_id)
                .await
                .map_err(anyhow::Error::from)?;
            if let Some(queued) = maybe_input {
                let agent_input =
                    agent_input_from_queued_priority(queued.priority, queued.content.clone());
                let active = self.active_session().await;
                if let Some(active) = active {
                    let enqueue_result = {
                        let mut runtime = active.lock().await;
                        runtime.session.enqueue_input(agent_input)
                    };
                    if let Err(error) = enqueue_result {
                        self.state
                            .repo
                            .reset_consuming_input(&self.session_id, &queued.id, &queued.claim_id)
                            .await
                            .map_err(anyhow::Error::from)?;
                        return Err(RpcError::new("invalid_input", error.to_string()));
                    }
                    let dispatched = self
                        .persist_active_outputs(active, None, Some(queued), None, Vec::new())
                        .await?;
                    let has_dispatched_work = !dispatched.is_empty();
                    dispatched_all.extend(dispatched.clone());
                    self.dispatch(dispatched);
                    if has_dispatched_work {
                        break;
                    }
                }
                continue;
            }

            self.state.active.lock().await.remove(&self.session_id);
            let event = self
                .state
                .repo
                .insert_event(&self.session_id, EventType::SessionIdle, json!({}))
                .await
                .map_err(anyhow::Error::from)?;
            publish_events(&self.state, vec![event]);
            clear_event_buffer_if_idle(&self.state, &self.session_id).await?;
            break;
        }
        Ok(dispatched_all)
    }

    async fn consume_ready_steer(
        &self,
        active: Arc<Mutex<RuntimeSession>>,
    ) -> std::result::Result<Option<Vec<DispatchAction>>, RpcError> {
        let is_ready_to_continue = {
            let runtime = active.lock().await;
            runtime.session.is_ready_to_continue()
        };
        if !is_ready_to_continue {
            return Ok(None);
        }

        let Some(queued) = self
            .state
            .repo
            .take_next_queued_steer_input(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?
        else {
            return Ok(None);
        };

        let agent_input = agent_input_from_queued_priority(queued.priority, queued.content.clone());
        let enqueue_result = {
            let mut runtime = active.lock().await;
            runtime.session.enqueue_input(agent_input)
        };
        if let Err(error) = enqueue_result {
            self.state
                .repo
                .reset_consuming_input(&self.session_id, &queued.id, &queued.claim_id)
                .await
                .map_err(anyhow::Error::from)?;
            return Err(RpcError::new("invalid_input", error.to_string()));
        }

        self.persist_active_outputs(active, None, Some(queued), None, Vec::new())
            .await
            .map(Some)
    }

    pub(crate) async fn apply_agent_input(
        &self,
        active: Arc<Mutex<RuntimeSession>>,
        input: AgentInput,
        action_update: Option<ActionUpdate>,
        context_tokens: Option<usize>,
        provider_replay: Vec<ProviderReplayItem>,
    ) -> std::result::Result<Vec<DispatchAction>, RpcError> {
        if let Some(update) = &action_update {
            if !self
                .state
                .repo
                .action_can_complete(&self.session_id, &update.row_id, &update.attempt_id)
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
        self.persist_active_outputs(active, action_update, None, None, provider_replay)
            .await
    }

    pub(crate) async fn resume_model_turn(
        &self,
        checkpoint_leaf_id: &str,
        turn_id: agent_vocab::TurnId,
        action_id: agent_vocab::ActionId,
        context_tokens: Option<usize>,
    ) -> std::result::Result<Vec<DispatchAction>, RpcError> {
        let config = self
            .state
            .repo
            .load_session_config(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let stored = self
            .state
            .repo
            .load_stored_session(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let mut session = AgentSession::from_stored_session(stored)
            .map_err(|error| RpcError::new("invalid_transcript", format!("{error:?}")))?;
        session
            .resume_model_turn(checkpoint_leaf_id, turn_id, action_id, context_tokens)
            .map_err(history_error_to_rpc)?;

        let active = Arc::new(Mutex::new(RuntimeSession { session, config }));
        self.state
            .active
            .lock()
            .await
            .insert(self.session_id.clone(), active.clone());
        self.persist_active_outputs(active, None, None, None, Vec::new())
            .await
    }

    pub(crate) async fn persist_active_outputs(
        &self,
        active: Arc<Mutex<RuntimeSession>>,
        action_update: Option<ActionUpdate>,
        consumed_input: Option<QueuedInput>,
        accepted_input: Option<AcceptedInput>,
        provider_replay: Vec<ProviderReplayItem>,
    ) -> std::result::Result<Vec<DispatchAction>, RpcError> {
        let (mut entries, events, actions, active_leaf_id, config) = {
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
        attach_provider_replay(&mut entries, provider_replay)?;
        let persisted = self
            .state
            .repo
            .persist_outputs(
                &self.session_id,
                OutputBatch::new(&entries, active_leaf_id.as_deref(), &events, &actions)
                    .with_action_update(action_update)
                    .with_consumed_input(consumed_input)
                    .with_accepted_input(accepted_input),
            )
            .await;
        let (frames, persisted_actions) = match persisted {
            Ok(persisted) => persisted,
            Err(error) => {
                self.state.active.lock().await.remove(&self.session_id);
                return Err(anyhow::Error::from(error).into());
            }
        };
        publish_events(&self.state, frames);
        Ok(attach_dispatch_config(persisted_actions, &config))
    }

    pub(crate) fn dispatch(&self, dispatches: Vec<DispatchAction>) {
        dispatch_all(&self.state, &self.session_id, dispatches);
    }
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
        .active_leaf_id()
        .map(str::to_string);
    (entries, events, actions, active_leaf_id)
}

fn attach_provider_replay(
    entries: &mut [TranscriptStorageNode],
    provider_replay: Vec<ProviderReplayItem>,
) -> std::result::Result<(), RpcError> {
    if provider_replay.is_empty() {
        return Ok(());
    }
    let Some(entry) = entries
        .iter_mut()
        .rev()
        .find(|entry| matches!(entry.item, TranscriptItem::AssistantMessage(_)))
    else {
        return Err(RpcError::new(
            "invalid_provider_output",
            "provider replay sidecar had no assistant transcript entry",
        ));
    };
    entry.provider_replay.extend(provider_replay);
    Ok(())
}

pub(crate) fn map_queued_mutation_error(error: anyhow::Error) -> RpcError {
    if let Some(error) = error.downcast_ref::<QueueMutationError>() {
        return RpcError::new("input_not_found", error.to_string());
    }
    error.into()
}

pub(crate) fn history_error_to_rpc(error: HistoryOperationError) -> RpcError {
    match error {
        HistoryOperationError::Busy => RpcError::new("session_busy", "session history is busy"),
        HistoryOperationError::Store(TranscriptStoreError::EntryNotFound) => {
            RpcError::new("entry_not_found", "transcript entry not found")
        }
        HistoryOperationError::Store(TranscriptStoreError::NotTurnBoundary) => {
            RpcError::new("not_turn_boundary", "target is not a turn boundary")
        }
        HistoryOperationError::Store(TranscriptStoreError::DuplicateEntry) => {
            RpcError::new("invalid_transcript", "duplicate transcript entry")
        }
        HistoryOperationError::Store(TranscriptStoreError::MissingParent) => RpcError::new(
            "invalid_transcript",
            "transcript entry has a missing parent",
        ),
    }
}

pub(crate) fn attach_dispatch_config(
    persisted_actions: Vec<PersistedAction>,
    config: &SessionConfig,
) -> Vec<DispatchAction> {
    persisted_actions
        .into_iter()
        .map(|action| DispatchAction {
            row_id: action.row_id,
            attempt_id: action.attempt_id,
            action: action.action,
            config: config.clone(),
        })
        .collect()
}

pub(crate) fn dispatch_all(state: &AppState, session_id: &str, dispatches: Vec<DispatchAction>) {
    for dispatch in dispatches {
        spawn_dispatch(state.clone(), session_id.to_string(), dispatch);
    }
}

pub(crate) fn spawn_compaction(
    state: &AppState,
    session_id: String,
    job: CompactionJob,
    config: SessionConfig,
) {
    let task_list = state.dispatch_tasks.clone();
    let task_state = state.clone();
    let handle = tokio::spawn(async move {
        if let Err(error) =
            run_compaction_job(task_state.clone(), session_id.clone(), job, config).await
        {
            eprintln!(
                "compaction task failed {session_id}: {}: {}",
                error.code, error.message
            );
        }
    });
    let mut tasks = task_list.lock().expect("dispatch task list lock poisoned");
    tasks.retain(|task| !task.is_finished());
    tasks.push(handle);
}

async fn run_compaction_job(
    state: AppState,
    session_id: String,
    job: CompactionJob,
    config: SessionConfig,
) -> std::result::Result<(), RpcError> {
    let result = run_compaction(&config, job.model_context.clone()).await;
    let events = match result {
        Ok(summary) => {
            let result = state
                .repo
                .complete_compaction_action(&job, summary)
                .await
                .map_err(anyhow::Error::from)?;
            result.events
        }
        Err(error) => state
            .repo
            .fail_compaction_action(&job, error.to_string())
            .await
            .map_err(anyhow::Error::from)?,
    };
    publish_events(&state, events);

    let driver = SessionDriver::acquire(&state, &session_id).await;
    driver.drive_until_blocked().await?;
    Ok(())
}

fn spawn_dispatch(state: AppState, session_id: String, dispatch: DispatchAction) {
    let event_type = match &dispatch.action {
        SessionAction::RequestModel { .. } => EventType::ModelError,
        SessionAction::RequestTool { .. } => EventType::ToolError,
        SessionAction::CancelSessionWork => return,
    };
    if matches!(&dispatch.action, SessionAction::RequestModel { .. })
        && session_uses_harness(&dispatch.config)
    {
        return;
    }

    let task_list = state.dispatch_tasks.clone();
    let task_state = state.clone();
    let handle = tokio::spawn(async move {
        let row_id = dispatch.row_id.clone();
        let result = match dispatch.action.clone() {
            SessionAction::RequestModel { .. } => {
                run_model_turn(task_state.clone(), session_id.clone(), dispatch).await
            }
            SessionAction::RequestTool { .. } => {
                run_tool_turn(task_state.clone(), session_id.clone(), dispatch).await
            }
            SessionAction::CancelSessionWork => Ok(()),
        };
        if let Err(error) = result {
            eprintln!(
                "dispatch task failed {session_id}/{row_id}: {}: {}",
                error.code, error.message
            );
            if let Err(stale_error) = task_state
                .repo
                .mark_action_stale(&session_id, &row_id)
                .await
            {
                eprintln!("failed to mark action stale {session_id}/{row_id}: {stale_error:#}");
            }
            match task_state
                .repo
                .insert_event(
                    &session_id,
                    event_type,
                    json!({
                        "action_row_id": row_id,
                        "error": error.message,
                    }),
                )
                .await
            {
                Ok(event) => {
                    publish_events(&task_state, vec![event]);
                    if let Err(clear_error) =
                        clear_event_buffer_if_idle(&task_state, &session_id).await
                    {
                        eprintln!(
                            "failed to clear idle event buffer {session_id}: {}: {}",
                            clear_error.code, clear_error.message
                        );
                    }
                }
                Err(event_error) => eprintln!(
                    "failed to record dispatch failure event {session_id}/{row_id}: {event_error:#}"
                ),
            }
        }
    });
    let mut tasks = task_list.lock().expect("dispatch task list lock poisoned");
    tasks.retain(|task| !task.is_finished());
    tasks.push(handle);
}

async fn run_model_turn(
    state: AppState,
    session_id: String,
    dispatch: DispatchAction,
) -> std::result::Result<(), RpcError> {
    let SessionAction::RequestModel {
        action_id,
        turn_id,
        model_context,
        ..
    } = dispatch.action
    else {
        return Ok(());
    };

    let result = run_model(&state, &dispatch.config, model_context).await;
    let driver = SessionDriver::acquire(&state, &session_id).await;
    if !state
        .repo
        .action_can_complete(&session_id, &dispatch.row_id, &dispatch.attempt_id)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Ok(());
    }
    let active = driver
        .active_session()
        .await
        .ok_or_else(|| RpcError::new("stale_action", "session is not active"))?;
    let (input, status, update_result, provider_replay) = match result {
        Ok(response) => (
            AgentInput::ModelCompleted {
                action_id,
                turn_id,
                assistant: response.assistant,
            },
            ActionStatus::Completed,
            json!({ "source": "provider" }),
            response.provider_replay,
        ),
        Err(error) => {
            let message = error.to_string();
            (
                AgentInput::ModelFailed {
                    action_id,
                    turn_id,
                    error: message.clone(),
                },
                ActionStatus::Error,
                json!({ "error": message }),
                Vec::new(),
            )
        }
    };
    let dispatches = driver
        .apply_agent_input(
            active,
            input,
            Some(ActionUpdate {
                row_id: dispatch.row_id,
                attempt_id: dispatch.attempt_id,
                status,
                result: update_result,
            }),
            None,
            provider_replay,
        )
        .await?;
    driver.dispatch(dispatches);
    driver.drive_until_blocked().await?;
    Ok(())
}

async fn run_tool_turn(
    state: AppState,
    session_id: String,
    dispatch: DispatchAction,
) -> std::result::Result<(), RpcError> {
    let SessionAction::RequestTool {
        action_id,
        turn_id,
        tool_call,
    } = dispatch.action
    else {
        return Ok(());
    };

    let events = state
        .repo
        .mark_action_running_and_event(
            &session_id,
            &dispatch.row_id,
            &dispatch.attempt_id,
            EventType::ToolStarted,
        )
        .await
        .map_err(anyhow::Error::from)?;
    if events.is_empty() {
        return Ok(());
    }
    publish_events(&state, events);

    let result = match state.tools.execute(&tool_call, &state.tool_context).await {
        Ok(result) => result,
        Err(error) => ToolResultMessage::error(
            tool_call.id.clone(),
            tool_call.tool_name.clone(),
            error.to_string(),
        ),
    };
    let status = if matches!(result.status, ToolResultStatus::Success) {
        ActionStatus::Completed
    } else {
        ActionStatus::Error
    };
    let driver = SessionDriver::acquire(&state, &session_id).await;
    if !state
        .repo
        .action_can_complete(&session_id, &dispatch.row_id, &dispatch.attempt_id)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Ok(());
    }
    let active = driver
        .active_session()
        .await
        .ok_or_else(|| RpcError::new("stale_action", "session is not active"))?;
    let mut consumed_input = None;
    {
        let mut runtime = active.lock().await;
        runtime
            .session
            .enqueue_input(AgentInput::ToolCompleted {
                action_id,
                turn_id,
                result: result.clone(),
            })
            .map_err(|error| RpcError::new("invalid_input", error.to_string()))?;
        runtime.session.drive();
    }
    let is_ready_to_continue = {
        let runtime = active.lock().await;
        runtime.session.is_ready_to_continue()
    };
    if is_ready_to_continue {
        if let Some(queued) = state
            .repo
            .take_next_queued_steer_input(&session_id)
            .await
            .map_err(anyhow::Error::from)?
        {
            let agent_input =
                agent_input_from_queued_priority(queued.priority, queued.content.clone());
            let enqueue_result = {
                let mut runtime = active.lock().await;
                runtime.session.enqueue_input(agent_input)
            };
            if let Err(error) = enqueue_result {
                state
                    .repo
                    .reset_consuming_input(&session_id, &queued.id, &queued.claim_id)
                    .await
                    .map_err(anyhow::Error::from)?;
                return Err(RpcError::new("invalid_input", error.to_string()));
            }
            consumed_input = Some(queued);
        }
        {
            let mut runtime = active.lock().await;
            runtime.session.drive();
        }
    }
    let dispatches = driver
        .persist_active_outputs(
            active,
            Some(ActionUpdate {
                row_id: dispatch.row_id,
                attempt_id: dispatch.attempt_id,
                status,
                result: serde_json::to_value(&result).unwrap_or_else(|_| json!({})),
            }),
            consumed_input,
            None,
            Vec::new(),
        )
        .await?;
    driver.dispatch(dispatches);
    driver.drive_until_blocked().await?;
    Ok(())
}

fn session_uses_harness(config: &SessionConfig) -> bool {
    config
        .metadata
        .get("harness")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn publish_events(state: &AppState, events: Vec<EventFrame>) {
    for event in events {
        let _ = state.events.send(event);
    }
}

pub(crate) async fn clear_event_buffer_if_idle(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<(), RpcError> {
    let activity = state
        .repo
        .activity(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    if activity == SessionActivity::Idle {
        state
            .repo
            .clear_session_events(session_id)
            .await
            .map_err(anyhow::Error::from)?;
    }
    Ok(())
}
