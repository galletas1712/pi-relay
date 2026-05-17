use std::sync::Arc;

use agent_core::AgentInput;
use agent_session::{
    AgentSession, HistoryOperationError, SessionAction, SessionEvent, SessionInput,
    TranscriptStorageNode, TranscriptStoreError,
};
use agent_store::{
    AcceptedInput, ActionKind, ActionStatus, ActionUpdate, CompactionCompletion, CompactionJob,
    CompactionScope, CompactionTrigger, EventFrame, EventType, InputPriority, OutputBatch,
    PersistedAction, QueueMutationError, QueuedInput, SessionActivity, SessionConfig,
};
use agent_tools::dynamic_tool_context;
use agent_vocab::{
    ProviderKind, ProviderReplayItem, ToolResultMessage, ToolResultStatus, TranscriptItem,
    UserMessage,
};
use anyhow::Context;
use serde_json::{json, Value};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::task::JoinHandle;

use crate::codec::transcript_store_from_stored;
use crate::provider_runtime::{
    auto_limit_tokens, compaction_auto_state, compaction_config, count_model_input_tokens,
    provider_error_is_context_overflow, run_compaction, run_model,
};
use crate::state::{AppState, RunningTask};
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
        self.ensure_idle_without_recovery().await
    }

    pub(crate) async fn ensure_idle_for_metadata_mutation(
        &self,
    ) -> std::result::Result<(), RpcError> {
        self.ensure_idle_without_recovery().await
    }

    async fn ensure_idle_without_recovery(&self) -> std::result::Result<(), RpcError> {
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
                "this operation requires an idle session",
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
        if self
            .state
            .repo
            .active_leaf_is_turn_boundary(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?
        {
            return Ok(());
        }
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
                self.dispatch(dispatched).await?;
                if has_dispatched_work {
                    break;
                }
                let pending_dispatched = self.dispatch_ready_actions().await?;
                if !pending_dispatched.is_empty() {
                    dispatched_all.extend(pending_dispatched);
                    break;
                }
                continue;
            }
            let dispatched = self
                .persist_active_outputs(active.clone(), None, None, None, Vec::new())
                .await?;
            let has_dispatched_work = !dispatched.is_empty();
            dispatched_all.extend(dispatched.clone());
            self.dispatch(dispatched).await?;
            if has_dispatched_work {
                break;
            }
            let pending_dispatched = self.dispatch_ready_actions().await?;
            if !pending_dispatched.is_empty() {
                dispatched_all.extend(pending_dispatched);
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
                    self.dispatch(dispatched).await?;
                    if has_dispatched_work {
                        break;
                    }
                    let pending_dispatched = self.dispatch_ready_actions().await?;
                    if !pending_dispatched.is_empty() {
                        dispatched_all.extend(pending_dispatched);
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

    pub(crate) async fn dispatch(
        &self,
        dispatches: Vec<DispatchAction>,
    ) -> std::result::Result<(), RpcError> {
        self.dispatch_pending_or_direct(dispatches).await
    }

    pub(crate) async fn dispatch_ready_actions(
        &self,
    ) -> std::result::Result<Vec<DispatchAction>, RpcError> {
        let pending = self
            .state
            .repo
            .pending_actions_for_dispatch(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let config = self
            .state
            .repo
            .load_session_config(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let resolved = pending
            .into_iter()
            .map(|action| DispatchAction {
                row_id: action.row_id,
                attempt_id: action.attempt_id,
                action: action.action,
                config: config.clone(),
            })
            .collect::<Vec<_>>();
        self.dispatch_pending_or_direct(resolved.clone()).await?;
        Ok(resolved)
    }

    async fn dispatch_pending_or_direct(
        &self,
        dispatches: Vec<DispatchAction>,
    ) -> std::result::Result<(), RpcError> {
        let mut ready = Vec::new();
        for dispatch in dispatches {
            match &dispatch.action {
                SessionAction::RequestModel { .. } => {
                    if self.gate_model_dispatch(&dispatch).await? {
                        ready.push(dispatch);
                    }
                }
                SessionAction::RequestTool { .. } => ready.push(dispatch),
                SessionAction::CancelSessionWork => {}
            }
        }
        dispatch_all(&self.state, &self.session_id, ready);
        Ok(())
    }

    async fn gate_model_dispatch(
        &self,
        dispatch: &DispatchAction,
    ) -> std::result::Result<bool, RpcError> {
        let Some(eligible) = check_compaction_eligible(dispatch) else {
            return Ok(true);
        };
        let Some(limit) = eligible.limit else {
            return Ok(true);
        };
        let SessionAction::RequestModel {
            model_context,
            context_tokens,
            ..
        } = &dispatch.action
        else {
            return Ok(true);
        };
        // Just-compacted contexts always start with a CompactionSummary as the
        // last item; the next dispatch is the resumed model action that
        // shouldn't immediately compact again.
        if matches!(
            model_context.transcript_items().last(),
            Some(TranscriptItem::CompactionSummary(_))
        ) {
            return Ok(true);
        }

        let tokens = match context_tokens {
            Some(tokens) => *tokens,
            None if !provider_can_estimate_input_tokens(&dispatch.config) => {
                // Codex backend has no token-counting endpoint; the next
                // turn's `usage.input_tokens` from `response.completed` will
                // populate `context_tokens` and gate compaction then.
                return Ok(true);
            }
            None => match count_model_input_tokens(
                &self.state,
                &dispatch.config,
                &self.session_id,
                model_context.clone(),
            )
            .await
            {
                Ok(tokens) => tokens,
                Err(error) => {
                    let provider_error = error.downcast_ref::<agent_provider::ProviderError>();
                    if provider_error.is_some_and(provider_error_is_context_overflow) {
                        limit
                    } else {
                        return Err(anyhow::Error::from(error).into());
                    }
                }
            },
        };
        if tokens < limit {
            return Ok(true);
        }

        block_and_spawn_auto_compaction(
            &self.state,
            &self.session_id,
            dispatch,
            ActionStatus::Pending,
            AutoCompactionReason::Threshold { tokens, limit },
            Some(tokens),
            Some(limit),
        )
        .await?;
        Ok(false)
    }
}

/// Two callsites (`gate_model_dispatch` and
/// `recover_model_context_overflow_with_compaction`) need the same set of
/// eligibility guards before they're allowed to dispatch compaction. This
/// captures the common decision in one place.
struct CompactionEligible {
    /// Resolved compaction threshold in tokens. `None` means the model has no
    /// known context window and we can't compute a proactive limit; the
    /// reactive overflow path can still fire because the provider error tells
    /// us the model itself rejected the input.
    limit: Option<usize>,
}

fn check_compaction_eligible(dispatch: &DispatchAction) -> Option<CompactionEligible> {
    let SessionAction::RequestModel {
        context_leaf_id, ..
    } = &dispatch.action
    else {
        return None;
    };
    if session_uses_harness(&dispatch.config) {
        return None;
    }
    let config = compaction_config(&dispatch.config);
    if !config.auto_enabled {
        return None;
    }
    let source_leaf_id = context_leaf_id.as_deref()?;
    let auto_state = compaction_auto_state(&dispatch.config);
    if auto_state.suppressed || auto_state.last_failure_leaf_id.as_deref() == Some(source_leaf_id) {
        return None;
    }
    Some(CompactionEligible {
        limit: auto_limit_tokens(&config),
    })
}

/// Anthropic exposes `/messages/count_tokens` so the gate can size the
/// transcript before dispatch. Codex has no such endpoint, so the gate must
/// fall through and rely on the reactive overflow recovery path.
fn provider_can_estimate_input_tokens(config: &SessionConfig) -> bool {
    matches!(config.provider.kind, ProviderKind::Claude)
}

/// Typed reason for an auto-compaction trigger. Serialized to a stable string
/// for the wire payload (`payload.reason`) so the web renderer doesn't need
/// to change.
enum AutoCompactionReason {
    /// Pre-dispatch gate: the measured/estimated context exceeded the
    /// configured auto-compaction threshold.
    Threshold { tokens: usize, limit: usize },
    /// Post-dispatch recovery: the provider rejected the request with a
    /// context-window overflow error.
    Overflow { provider_error: String },
}

impl AutoCompactionReason {
    fn into_trigger_reason(self) -> String {
        match self {
            Self::Threshold { tokens, limit } => {
                format!("threshold: model_context_tokens {tokens} >= auto_limit_tokens {limit}")
            }
            Self::Overflow { provider_error } => {
                format!("provider context overflow before model completion: {provider_error}")
            }
        }
    }
}

/// Shared "block this model action and spawn a compaction job" path used by
/// both the proactive gate (Pending → Blocked) and the reactive overflow
/// recovery (Running → Blocked). Centralizes event publishing, the
/// boundary-scope active-session cleanup, and the spawn handoff.
async fn block_and_spawn_auto_compaction(
    state: &AppState,
    session_id: &str,
    dispatch: &DispatchAction,
    expected_status: ActionStatus,
    reason: AutoCompactionReason,
    tokens_before: Option<usize>,
    limit: Option<usize>,
) -> std::result::Result<(), RpcError> {
    let trigger = CompactionTrigger::Auto {
        reason: reason.into_trigger_reason(),
    };
    let created = state
        .repo
        .block_model_action_for_compaction(
            session_id,
            &dispatch.row_id,
            &dispatch.attempt_id,
            expected_status,
            trigger,
            tokens_before,
            limit,
        )
        .await
        .map_err(anyhow::Error::from)?;
    publish_events(state, created.events);
    if matches!(created.job.scope, CompactionScope::Boundary { .. }) {
        state.active.lock().await.remove(session_id);
    }
    spawn_compaction(
        state,
        session_id.to_string(),
        created.job,
        dispatch.config.clone(),
    );
    Ok(())
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
    prune_finished_tasks(state);
    let action_row_id = job.action_row_id.clone();
    let task_state = state.clone();
    let task_session_id = session_id.clone();
    let task_action_row_id = action_row_id.clone();
    let handle = tokio::spawn(async move {
        let action_row_id = job.action_row_id.clone();
        let result = run_compaction_job(task_state.clone(), session_id.clone(), job, config).await;
        unregister_task(&task_state, &action_row_id);
        if let Err(error) = result {
            eprintln!(
                "compaction task failed {session_id}: {}: {}",
                error.code, error.message
            );
        }
    });
    register_task(
        state,
        RunningTask {
            session_id: task_session_id,
            action_row_id: task_action_row_id,
            kind: ActionKind::Compaction,
            handle,
        },
    );
}

pub(crate) fn abort_session_tasks(state: &AppState, session_id: &str) -> Vec<ActionKind> {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    tasks.retain(|_, task| !task.handle.is_finished());
    let action_row_ids = tasks
        .iter()
        .filter_map(|(action_row_id, task)| {
            (task.session_id == session_id).then(|| action_row_id.clone())
        })
        .collect::<Vec<_>>();
    let mut aborted = Vec::new();
    for action_row_id in action_row_ids {
        if let Some(task) = tasks.remove(&action_row_id) {
            aborted.push(task.kind);
            task.handle.abort();
        }
    }
    aborted
}

pub(crate) fn take_tasks(state: &AppState) -> Vec<JoinHandle<()>> {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    tasks.drain().map(|(_, task)| task.handle).collect()
}

fn register_task(state: &AppState, task: RunningTask) {
    state
        .tasks
        .lock()
        .expect("task registry lock poisoned")
        .insert(task.action_row_id.clone(), task);
}

fn unregister_task(state: &AppState, action_row_id: &str) {
    state
        .tasks
        .lock()
        .expect("task registry lock poisoned")
        .remove(action_row_id);
}

fn prune_finished_tasks(state: &AppState) {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    tasks.retain(|_, task| !task.handle.is_finished());
}

async fn run_compaction_job(
    state: AppState,
    session_id: String,
    job: CompactionJob,
    config: SessionConfig,
) -> std::result::Result<(), RpcError> {
    let result = run_compaction(&state, &config, &session_id, job.compaction_context.clone()).await;
    let (events, resumed) = match result {
        Ok(output) => {
            let continuation_suffix = continuation_suffix_for_scope(&job)?;
            let completion = CompactionCompletion {
                summary: output.summary,
                summary_kind: output.summary_kind.as_str().to_string(),
                provider_replay: output.provider_replay,
                remote: output.remote,
                provider: output.provider,
                usage: output.usage,
                continuation_suffix,
            };
            let result = state
                .repo
                .complete_compaction_action(&job, completion)
                .await
                .map_err(anyhow::Error::from)?;
            if result.new_root_id.is_some() {
                state
                    .repo
                    .record_compaction_success(
                        &session_id,
                        result.new_root_id.as_deref(),
                        matches!(job.trigger, CompactionTrigger::Manual),
                    )
                    .await
                    .map_err(anyhow::Error::from)?;
                if matches!(job.scope, CompactionScope::MidTurn { .. }) {
                    install_runtime_compaction_checkpoint(
                        &state,
                        &session_id,
                        &job,
                        String::new(),
                        Vec::new(),
                        Vec::new(),
                    )
                    .await?;
                }
            }
            (result.events, result.resumed_model_action)
        }
        Err(error) => {
            let error = error.to_string();
            if matches!(job.trigger, CompactionTrigger::Auto { .. }) {
                state
                    .repo
                    .record_auto_compaction_failure(
                        &session_id,
                        &config,
                        &job.source_leaf_id,
                        &error,
                    )
                    .await
                    .map_err(anyhow::Error::from)?;
            }
            fail_blocked_model_for_compaction_error(&state, &session_id, &job, &error).await?;
            (
                state
                    .repo
                    .fail_compaction_action(&job, error)
                    .await
                    .map_err(anyhow::Error::from)?,
                None,
            )
        }
    };
    publish_events(&state, events);

    let driver = SessionDriver::acquire(&state, &session_id).await;
    if let Some(resumed) = resumed {
        let dispatch = DispatchAction {
            row_id: resumed.row_id,
            attempt_id: resumed.attempt_id,
            action: resumed.action,
            config: config.clone(),
        };
        if state
            .repo
            .claim_pending_model_action(&session_id, &dispatch.row_id, &dispatch.attempt_id)
            .await
            .map_err(anyhow::Error::from)?
        {
            spawn_model_dispatch(state.clone(), session_id.clone(), dispatch, true);
        }
    }
    driver.drive_until_blocked().await?;
    Ok(())
}

fn continuation_suffix_for_scope(
    job: &CompactionJob,
) -> std::result::Result<Vec<TranscriptStorageNode>, RpcError> {
    match &job.scope {
        CompactionScope::Boundary { .. } => Ok(Vec::new()),
        CompactionScope::MidTurn { .. } => {
            let mut parent_id: Option<String> = None;
            Ok(job
                .model_context
                .split_before_open_turn()
                .map(|(_, suffix)| suffix)
                .unwrap_or_default()
                .into_iter()
                .map(|entry| {
                    let node = TranscriptStorageNode {
                        id: format!("entry_{}", uuid::Uuid::new_v4()),
                        parent_id: parent_id.clone(),
                        timestamp_ms: crate::codec::now_ms(),
                        item: entry.item,
                        provider_replay: entry.provider_replay,
                    };
                    parent_id = Some(node.id.clone());
                    node
                })
                .collect())
        }
    }
}

async fn install_runtime_compaction_checkpoint(
    state: &AppState,
    session_id: &str,
    job: &CompactionJob,
    _summary: String,
    _provider_replay: Vec<ProviderReplayItem>,
    _suffix_nodes: Vec<TranscriptStorageNode>,
) -> std::result::Result<(), RpcError> {
    let Some(active) = state.active.lock().await.get(session_id).cloned() else {
        return Ok(());
    };
    let stored = state
        .repo
        .load_stored_session(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let session = AgentSession::from_stored_session_preserving_open_turn(stored)
        .map_err(history_error_to_rpc)?;
    let mut runtime = active.lock().await;
    runtime.session = session;
    if let CompactionScope::MidTurn {
        turn_id,
        blocked_model_action_id,
        ..
    } = &job.scope
    {
        let action_id = *blocked_model_action_id;
        let active_leaf_id = runtime
            .session
            .transcript_store()
            .active_leaf_id()
            .map(str::to_string)
            .ok_or_else(|| {
                RpcError::new("invalid_compaction", "compaction produced no active leaf")
            })?;
        runtime
            .session
            .restore_compacted_runtime(&active_leaf_id, *turn_id, action_id)
            .map_err(history_error_to_rpc)?;
    }
    Ok(())
}

async fn fail_blocked_model_for_compaction_error(
    state: &AppState,
    session_id: &str,
    job: &CompactionJob,
    error: &str,
) -> std::result::Result<(), RpcError> {
    let CompactionScope::MidTurn {
        turn_id,
        blocked_model_action_id,
        blocked_model_action_row_id,
        blocked_model_attempt_id,
        ..
    } = &job.scope
    else {
        return Ok(());
    };
    let model_error = format!("compaction failed before model dispatch: {error}");
    let events = state
        .repo
        .fail_blocked_or_pending_model_action(
            session_id,
            blocked_model_action_row_id,
            blocked_model_attempt_id,
            &model_error,
        )
        .await
        .map_err(anyhow::Error::from)?;
    publish_events(state, events);
    let Some(active) = state.active.lock().await.get(session_id).cloned() else {
        return Ok(());
    };
    let action_id = *blocked_model_action_id;
    let driver = SessionDriver::acquire(state, session_id).await;
    let dispatches = driver
        .apply_agent_input(
            active,
            AgentInput::ModelFailed {
                action_id,
                turn_id: *turn_id,
                error: model_error.clone(),
            },
            None,
            None,
            Vec::new(),
        )
        .await?;
    driver.dispatch(dispatches).await?;
    Ok(())
}

fn spawn_dispatch(state: AppState, session_id: String, dispatch: DispatchAction) {
    if matches!(&dispatch.action, SessionAction::RequestModel { .. }) {
        spawn_model_dispatch(state, session_id, dispatch, false);
    } else {
        spawn_claimed_dispatch(state, session_id, dispatch);
    }
}

fn spawn_model_dispatch(
    state: AppState,
    session_id: String,
    dispatch: DispatchAction,
    already_claimed: bool,
) {
    if session_uses_harness(&dispatch.config) {
        return;
    }
    if !already_claimed {
        let state = state.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            let run = state
                .repo
                .claim_pending_model_action(&session_id, &dispatch.row_id, &dispatch.attempt_id)
                .await;
            match run {
                Ok(true) => spawn_model_dispatch(state, session_id, dispatch, true),
                Ok(false) => {}
                Err(error) => eprintln!(
                    "failed to claim model action {session_id}/{}: {error:#}",
                    dispatch.row_id
                ),
            }
        });
        return;
    }
    spawn_claimed_dispatch(state, session_id, dispatch);
}

fn spawn_claimed_dispatch(state: AppState, session_id: String, dispatch: DispatchAction) {
    let event_type = match &dispatch.action {
        SessionAction::RequestModel { .. } => EventType::ModelError,
        SessionAction::RequestTool { .. } => EventType::ToolError,
        SessionAction::CancelSessionWork => return,
    };
    prune_finished_tasks(&state);
    let action_row_id = dispatch.row_id.clone();
    let action_kind = match &dispatch.action {
        SessionAction::RequestModel { .. } => ActionKind::Model,
        SessionAction::RequestTool { .. } => ActionKind::Tool,
        SessionAction::CancelSessionWork => return,
    };
    let task_state = state.clone();
    let task_session_id = session_id.clone();
    let task_action_row_id = action_row_id.clone();
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
        unregister_task(&task_state, &row_id);
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
    register_task(
        &state,
        RunningTask {
            session_id: task_session_id,
            action_row_id: task_action_row_id,
            kind: action_kind,
            handle,
        },
    );
}

async fn run_model_turn(
    state: AppState,
    session_id: String,
    dispatch: DispatchAction,
) -> std::result::Result<(), RpcError> {
    let original_action = dispatch.action.clone();
    let SessionAction::RequestModel {
        action_id,
        turn_id,
        model_context,
        context_leaf_id,
        context_tokens,
    } = original_action
    else {
        return Ok(());
    };
    let dispatch = DispatchAction {
        action: SessionAction::RequestModel {
            action_id,
            turn_id,
            model_context: model_context.clone(),
            context_leaf_id,
            context_tokens,
        },
        ..dispatch
    };

    let result = run_model(&state, &dispatch.config, &session_id, model_context).await;
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
    let (input, status, update_result, provider_replay, context_tokens) = match result {
        Ok(response) => {
            let context_tokens = response.usage.as_ref().and_then(|usage| usage.input_tokens);
            if context_tokens.is_some() {
                state
                    .repo
                    .reset_auto_compaction_failures(&session_id)
                    .await
                    .map_err(anyhow::Error::from)?;
            }
            (
                AgentInput::ModelCompleted {
                    action_id,
                    turn_id,
                    assistant: response.assistant,
                },
                ActionStatus::Completed,
                json!({
                    "source": "provider",
                    "usage": response.usage,
                }),
                response.provider_replay,
                context_tokens,
            )
        }
        Err(error) => {
            if recover_model_context_overflow_with_compaction(
                &state,
                &session_id,
                &dispatch,
                &error,
            )
            .await?
            {
                return Ok(());
            }
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
                None,
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
            context_tokens,
            provider_replay,
        )
        .await?;
    driver.dispatch(dispatches).await?;
    driver.drive_until_blocked().await?;
    Ok(())
}

async fn recover_model_context_overflow_with_compaction(
    state: &AppState,
    session_id: &str,
    dispatch: &DispatchAction,
    error: &anyhow::Error,
) -> std::result::Result<bool, RpcError> {
    let Some(provider_error) = error.downcast_ref::<agent_provider::ProviderError>() else {
        return Ok(false);
    };
    if !provider_error_is_context_overflow(provider_error) {
        return Ok(false);
    }
    let Some(eligible) = check_compaction_eligible(dispatch) else {
        return Ok(false);
    };

    block_and_spawn_auto_compaction(
        state,
        session_id,
        dispatch,
        ActionStatus::Running,
        AutoCompactionReason::Overflow {
            provider_error: provider_error.to_string(),
        },
        None,
        eligible.limit,
    )
    .await?;
    Ok(true)
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

    let tool_context = dynamic_tool_context(
        &state.default_tool_context,
        std::path::PathBuf::from(dispatch.config.starting_cwd.clone()),
    );
    let result = match state
        .tools
        .execute(dispatch.config.provider.kind, &tool_call, &tool_context)
        .await
    {
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
    driver.dispatch(dispatches).await?;
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
