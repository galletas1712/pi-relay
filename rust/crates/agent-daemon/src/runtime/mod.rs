mod compaction;
mod dispatch;
mod errors;
mod events;
mod model;
mod outputs;
mod tasks;
mod tool;

use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use agent_core::AgentInput;
use agent_session::{AgentSession, SessionAction, SessionInput};
use agent_store::{
    AcceptedInput, ActionUpdate, EventFrame, EventType, OutputBatch, QueuedInput, SessionActivity,
    SessionConfig, SubagentType, POST_COMPACTION_DISPATCH_LEASE_DURATION,
};
use agent_vocab::ProviderReplayItem;
use anyhow::Context;
use serde_json::{json, Value};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::codec::transcript_store_from_stored;
use crate::state::AppState;
use crate::types::{DispatchAction, RpcError, RuntimeSession};

pub(crate) use compaction::spawn_compaction;
use dispatch::dispatch_all;
#[cfg(test)]
pub(crate) use dispatch::runner_start_count;
pub(crate) use errors::{
    history_error_to_rpc, map_queued_mutation_error, map_source_mutation_error,
};
pub(crate) use events::{clear_event_buffer_if_idle, publish_events};
#[cfg(test)]
pub(crate) use model::apply_model_response;
use outputs::attach_provider_replay;
pub(crate) use outputs::{
    agent_input_from_queued_priority, attach_dispatch_config, collect_runtime_outputs,
};
pub(crate) use tasks::{
    abort_session_tasks, register_auxiliary_task, session_has_live_tasks, take_tasks,
};

pub(crate) async fn recover_post_compaction_dispatches_on_boot(
    state: &AppState,
) -> anyhow::Result<usize> {
    ensure_post_compaction_dispatch_recovery(state);
    recover_post_compaction_dispatches_once(state).await
}

async fn recover_post_compaction_dispatches_once(state: &AppState) -> anyhow::Result<usize> {
    if tasks::is_shutting_down(state) {
        return Ok(0);
    }
    let mut recovered_total = 0;
    let session_ids = state.repo.post_compaction_dispatch_session_ids().await?;
    for session_id in session_ids {
        let driver = SessionDriver::acquire(state, &session_id).await;
        recovered_total += driver
            .recover_post_compaction_dispatches()
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to recover post-compaction dispatch for {session_id}: {}: {}",
                    error.code,
                    error.message
                )
            })?;
    }
    Ok(recovered_total)
}

pub(super) fn ensure_post_compaction_dispatch_recovery(state: &AppState) {
    if state.shutting_down.load(Ordering::Acquire)
        || state
            .post_compaction_recovery_scheduled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return;
    }
    let task_state = state.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        let mut error_backoff = Duration::from_millis(100);
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
            () = task_state.post_compaction_recovery_notify.notified() => {}
        }
        loop {
            if task_state.shutting_down.load(Ordering::Acquire) {
                break;
            }
            let delay = match task_state
                .repo
                .next_post_compaction_dispatch_lease_delay()
                .await
            {
                Ok(Some(delay)) => delay.saturating_add(Duration::from_millis(10)),
                Ok(None) => Duration::from_secs(30),
                Err(error) => {
                    eprintln!("failed to load post-compaction recovery lease delay: {error:#}");
                    let delay = error_backoff;
                    error_backoff = (error_backoff * 2).min(Duration::from_secs(5));
                    delay
                }
            };
            tokio::select! {
                () = tokio::time::sleep(delay.max(Duration::from_millis(10))) => {}
                () = task_state.post_compaction_recovery_notify.notified() => {}
            }
            if task_state.shutting_down.load(Ordering::Acquire) {
                break;
            }
            if let Err(error) = recover_post_compaction_dispatches_once(&task_state).await {
                eprintln!("delayed post-compaction dispatch recovery failed: {error:#}");
                tokio::select! {
                    () = tokio::time::sleep(error_backoff) => {}
                    () = task_state.post_compaction_recovery_notify.notified() => {}
                }
                error_backoff = (error_backoff * 2).min(Duration::from_secs(5));
            } else {
                error_backoff = Duration::from_millis(100);
            }
        }
    });
    let _ = tasks::register_recovery_task(state, handle, start_tx);
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

    async fn load_interrupted_control_runtime(
        &self,
    ) -> std::result::Result<Arc<Mutex<RuntimeSession>>, RpcError> {
        let config = self
            .state
            .repo
            .load_session_config(&self.session_id)
            .await?;
        self.state
            .workspaces
            .ensure_session(&self.session_id, &config.outer_cwd, &config.workspaces)
            .await?;
        let stored = self
            .state
            .repo
            .load_stored_session(&self.session_id)
            .await?;
        let session = AgentSession::from_stored_session_interrupted(stored)
            .map_err(|error| RpcError::new("invalid_transcript", format!("{error:?}")))?;
        let active = Arc::new(Mutex::new(RuntimeSession { session, config }));
        self.state
            .active
            .lock()
            .await
            .insert(self.session_id.clone(), active.clone());
        Ok(active)
    }

    /// Acquire only if the session's driver lock is free. Returns `None` if it
    /// is already held (the session is being driven or recovered elsewhere on
    /// the stack), letting the delegation barrier recover sibling tails without
    /// blocking on — or deadlocking against — a lock held further up the call
    /// chain (e.g. the firing child whose terminal idle triggered the barrier).
    pub(crate) async fn try_acquire(
        state: &AppState,
        session_id: impl Into<String>,
    ) -> Option<Self> {
        let session_id = session_id.into();
        let lock = session_driver_lock(state, &session_id).await;
        let guard = lock.try_lock_owned().ok()?;
        Some(Self {
            state: state.clone(),
            session_id,
            _guard: guard,
        })
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
                .await?
            || self.state.repo.has_queued_inputs(&self.session_id).await?
        {
            return Err(RpcError::new(
                "session_busy",
                "this operation requires an idle session",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn ensure_active_loaded_preserving_open_turn(
        &self,
    ) -> std::result::Result<(), RpcError> {
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
            .await?;
        self.state
            .workspaces
            .ensure_session(&self.session_id, &config.outer_cwd, &config.workspaces)
            .await?;
        let stored = self
            .state
            .repo
            .load_stored_session(&self.session_id)
            .await?;
        let session = AgentSession::from_stored_session_preserving_open_turn(stored)
            .map_err(|error| RpcError::new("invalid_transcript", format!("{error:?}")))?;
        self.state.active.lock().await.insert(
            self.session_id.clone(),
            Arc::new(Mutex::new(RuntimeSession { session, config })),
        );
        Ok(())
    }

    async fn recover_post_compaction_dispatches(&self) -> std::result::Result<usize, RpcError> {
        let intents = self
            .state
            .repo
            .post_compaction_dispatch_intents(&self.session_id)
            .await?;
        let mut recovered = 0;
        for intent in intents {
            let claimed = match self
                .state
                .repo
                .claim_post_compaction_model_action(
                    &intent,
                    POST_COMPACTION_DISPATCH_LEASE_DURATION,
                )
                .await
            {
                Ok(Some(claimed)) => claimed,
                Ok(None) => continue,
                Err(agent_store::PostCompactionDispatchClaimError::Corrupt(error)) => {
                    let message = format!(
                        "failed to reconstruct post-compaction dispatch: {}",
                        error.message()
                    );
                    let events = self
                        .state
                        .repo
                        .fail_corrupt_post_compaction_model_action(&intent, error.fence(), &message)
                        .await?;
                    publish_events(&self.state, events);
                    continue;
                }
                Err(agent_store::PostCompactionDispatchClaimError::Transient(error)) => {
                    return Err(error.into());
                }
            };
            let SessionAction::RequestModel {
                action_id,
                turn_id,
                context_leaf_id: Some(context_leaf_id),
                ..
            } = &claimed.pending.action
            else {
                let message =
                    "failed to reconstruct post-compaction dispatch: invalid model action"
                        .to_string();
                let events = self
                    .state
                    .repo
                    .fail_unfinished_model_action(
                        &intent.session_id,
                        &intent.row_id,
                        &intent.attempt_id,
                        Some(&claimed.lease),
                        &message,
                    )
                    .await?;
                publish_events(&self.state, events);
                continue;
            };

            if self
                .state
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
            {
                self.state.active.lock().await.remove(&self.session_id);
                return Ok(recovered);
            }
            let config = match self
                .install_post_compaction_runtime(
                    context_leaf_id,
                    *turn_id,
                    *action_id,
                    &claimed.pending.provider,
                )
                .await
            {
                Ok(config) => config,
                Err(error) => {
                    self.state.active.lock().await.remove(&self.session_id);
                    return Err(RpcError::new(
                        "post_compaction_recovery_failed",
                        format!(
                            "failed to install post-compaction dispatch runtime; retaining lease for retry: {}",
                            error.message
                        ),
                    ));
                }
            };
            let dispatch = DispatchAction {
                row_id: claimed.pending.row_id,
                attempt_id: claimed.pending.attempt_id,
                post_compaction_dispatch_lease: Some(claimed.lease),
                action: claimed.pending.action,
                config,
            };
            if session_uses_harness(&dispatch.config) {
                // Harness/manual completion accepts running actions, so consume
                // the restart marker through the normal claim CAS but do not
                // start an internal provider runner.
                recovered += 1;
                continue;
            }
            #[cfg(test)]
            if let Some(milliseconds) = dispatch
                .config
                .metadata
                .pointer("/fault_injection/pause_recovery_before_register_ms")
                .and_then(serde_json::Value::as_u64)
            {
                tokio::time::sleep(Duration::from_millis(milliseconds)).await;
            }
            let lease = dispatch.post_compaction_dispatch_lease.clone();
            let registration_id = dispatch::spawn_model_dispatch(
                self.state.clone(),
                self.session_id.clone(),
                dispatch,
                true,
            )
            .await;
            if matches!(registration_id, Err(tasks::TaskRegistrationRejected)) {
                self.state.active.lock().await.remove(&self.session_id);
                return Ok(recovered);
            }
            if !registration_id
                .ok()
                .flatten()
                .as_ref()
                .is_some_and(|registration_id| {
                    tasks::task_registration_is_live(&self.state, &intent.row_id, registration_id)
                })
            {
                let message =
                    "failed to register recovered post-compaction model runner".to_string();
                let events = self
                    .state
                    .repo
                    .fail_unfinished_model_action(
                        &intent.session_id,
                        &intent.row_id,
                        &intent.attempt_id,
                        lease.as_ref(),
                        &message,
                    )
                    .await?;
                publish_events(&self.state, events);
                self.state.active.lock().await.remove(&self.session_id);
                continue;
            }
            recovered += 1;
        }
        Ok(recovered)
    }

    async fn install_post_compaction_runtime(
        &self,
        context_leaf_id: &str,
        turn_id: agent_vocab::TurnId,
        action_id: agent_vocab::ActionId,
        provider: &agent_vocab::ProviderConfig,
    ) -> std::result::Result<SessionConfig, RpcError> {
        let stored = self
            .state
            .repo
            .load_stored_session(&self.session_id)
            .await?;
        let mut config = self
            .state
            .repo
            .load_session_config(&self.session_id)
            .await?;
        config.provider = provider.clone();
        self.state
            .workspaces
            .ensure_session(&self.session_id, &config.outer_cwd, &config.workspaces)
            .await?;
        let mut session = AgentSession::from_stored_session_preserving_open_turn(stored)
            .map_err(history_error_to_rpc)?;
        if session.transcript_store().active_leaf_id() != Some(context_leaf_id) {
            return Err(RpcError::new(
                "invalid_compaction",
                "post-compaction dispatch leaf is not active",
            ));
        }
        session
            .restore_compacted_runtime(context_leaf_id, turn_id, action_id)
            .map_err(history_error_to_rpc)?;
        self.state.active.lock().await.insert(
            self.session_id.clone(),
            Arc::new(Mutex::new(RuntimeSession {
                session,
                config: config.clone(),
            })),
        );
        Ok(config)
    }

    /// Resolve all durable combined controls before ordinary recovery or queue
    /// driving can claim their messages.
    ///
    /// The interrupt transcript/action update and `interrupt_applied` phase
    /// commit atomically. Runtime task abortion then occurs while the row still
    /// blocks the mailbox; `ready` is persisted only afterwards. A crash in the
    /// middle therefore resumes from an unambiguous phase.
    pub(crate) async fn reconcile_pending_subagent_controls(
        &self,
    ) -> std::result::Result<(), RpcError> {
        // This driver is the sole queue owner for the exact child. Normalize
        // any claim left by a crashed/older daemon before advancing phases;
        // phase writes intentionally require `queued` so they cannot race a
        // live consumer.
        self.state
            .repo
            .reset_abandoned_consuming_inputs(&self.session_id)
            .await?;
        loop {
            let Some(control) = self
                .state
                .repo
                .next_pending_subagent_control(&self.session_id)
                .await?
            else {
                return Ok(());
            };
            if control.phase == agent_store::SubagentControlPhase::PendingInterrupt {
                match self
                    .state
                    .repo
                    .apply_subagent_control_interrupt_at_boundary(
                        &self.session_id,
                        &control.input_id,
                    )
                    .await?
                {
                    agent_store::SubagentBoundaryInterruptResult::Applied { .. } => {
                        abort_session_tasks(&self.state, &self.session_id);
                        self.state
                            .repo
                            .mark_subagent_control_ready(&self.session_id, &control.input_id)
                            .await?;
                        continue;
                    }
                    agent_store::SubagentBoundaryInterruptResult::GenerationAdvanced => continue,
                    agent_store::SubagentBoundaryInterruptResult::NotAtBoundary => {}
                }
                if !self
                    .state
                    .repo
                    .subagent_control_target_is_current(&self.session_id, &control)
                    .await?
                {
                    self.state
                        .repo
                        .skip_stale_subagent_control_interrupt(&self.session_id, &control.input_id)
                        .await?;
                    continue;
                }
                let active = self.load_interrupted_control_runtime().await?;
                self.persist_active_outputs_with_control(
                    active,
                    None,
                    None,
                    None,
                    Vec::new(),
                    Some(&control.input_id),
                )
                .await?;
            }
            abort_session_tasks(&self.state, &self.session_id);
            self.state
                .repo
                .mark_subagent_control_ready(&self.session_id, &control.input_id)
                .await?;
        }
    }

    pub(crate) async fn recover_if_needed(&self) -> std::result::Result<(), RpcError> {
        self.reconcile_pending_subagent_controls().await?;
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
            .await?;
        let had_post_compaction_intent = !self
            .state
            .repo
            .post_compaction_dispatch_intents(&self.session_id)
            .await?
            .is_empty();
        if self.recover_post_compaction_dispatches().await? > 0 || had_post_compaction_intent {
            return Ok(());
        }
        if self
            .state
            .repo
            .active_leaf_is_turn_boundary(&self.session_id)
            .await?
        {
            self.reconcile_abandoned_boundary_session().await?;
            return Ok(());
        }
        let stored = self
            .state
            .repo
            .load_stored_session(&self.session_id)
            .await?;
        let store = transcript_store_from_stored(&stored)?;
        if store.is_turn_boundary() {
            self.reconcile_abandoned_boundary_session().await?;
            return Ok(());
        }
        let recovered = AgentSession::from_stored_session(stored.clone())
            .map_err(|error| RpcError::new("invalid_transcript", format!("{error:?}")))?;
        let should_continue = recovered.is_ready_to_continue();
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
            .await?;
        let mut events = events;
        let activity = self.state.repo.activity(&self.session_id).await?;
        if !should_continue && activity == SessionActivity::Idle {
            if let Some(event) = self.try_handle_subagent_terminal_for_parent().await {
                events.push(event);
            }
        }
        publish_events(&self.state, events);
        clear_event_buffer_if_idle(&self.state, &self.session_id).await?;
        if should_continue {
            self.drive_until_blocked().await?;
        }
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
            .await?;
        self.state
            .workspaces
            .ensure_session(&self.session_id, &config.outer_cwd, &config.workspaces)
            .await?;
        let stored = self
            .state
            .repo
            .load_stored_session(&self.session_id)
            .await?;
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
            if let Some(dispatches) = self.consume_ready_steer(active.clone()).await? {
                if self
                    .dispatch_and_check(dispatches, &mut dispatched_all)
                    .await?
                {
                    break;
                }
                continue;
            }
            let dispatched = self
                .persist_active_outputs(active.clone(), None, None, None, Vec::new())
                .await?;
            if self
                .dispatch_and_check(dispatched, &mut dispatched_all)
                .await?
            {
                break;
            }

            if self
                .state
                .repo
                .has_unfinished_actions(&self.session_id)
                .await?
            {
                break;
            }

            let maybe_input = self
                .state
                .repo
                .take_next_queued_input(&self.session_id)
                .await?;
            if let Some(queued) = maybe_input {
                let agent_input =
                    agent_input_from_queued_priority(queued.priority, queued.content.clone());
                let active = self.active_session().await;
                if let Some(active) = active {
                    let enqueue_result = {
                        let mut runtime = active.lock().await;
                        // Ordinary queue consumption starts future work. Install
                        // the provider route captured when this item was
                        // accepted before the session creates its first action.
                        runtime.config.provider = queued.provider.clone();
                        runtime.session.enqueue_input(agent_input)
                    };
                    if let Err(error) = enqueue_result {
                        self.state
                            .repo
                            .reset_consuming_input(&self.session_id, &queued.id, &queued.claim_id)
                            .await?;
                        return Err(RpcError::new("invalid_input", error.to_string()));
                    }
                    let dispatched = self
                        .persist_active_outputs(active, None, Some(queued), None, Vec::new())
                        .await?;
                    if self
                        .dispatch_and_check(dispatched, &mut dispatched_all)
                        .await?
                    {
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
                .await?;
            let mut events = vec![event];
            if let Some(event) = self.try_handle_subagent_terminal_for_parent().await {
                events.push(event);
            }
            publish_events(&self.state, events);
            clear_event_buffer_if_idle(&self.state, &self.session_id).await?;
            break;
        }
        Ok(dispatched_all)
    }

    /// Dispatch freshly persisted actions, then check readiness. Returns whether
    /// the caller should break out of the drive loop (work was dispatched, or
    /// ready actions were dispatched).
    async fn dispatch_and_check(
        &self,
        dispatched: Vec<DispatchAction>,
        dispatched_all: &mut Vec<DispatchAction>,
    ) -> std::result::Result<bool, RpcError> {
        let has_dispatched_work = !dispatched.is_empty();
        dispatched_all.extend(dispatched.clone());
        self.dispatch(dispatched).await?;
        if has_dispatched_work {
            return Ok(true);
        }
        let pending_dispatched = self.dispatch_ready_actions().await?;
        if !pending_dispatched.is_empty() {
            dispatched_all.extend(pending_dispatched);
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) async fn handle_subagent_terminal_for_parent_if_needed(&self) {
        if let Some(event) = self.try_handle_subagent_terminal_for_parent().await {
            publish_events(&self.state, vec![event]);
        }
    }

    async fn destroy_read_only_subagent_workspaces(&self) {
        match self
            .state
            .repo
            .session_subagent_type(&self.session_id)
            .await
        {
            Ok(Some(SubagentType::ReadOnly)) => {
                if let Err(error) = self
                    .state
                    .workspaces
                    .destroy_session_workspaces(&self.session_id)
                    .await
                {
                    eprintln!(
                        "failed to destroy read-only subagent workspace {}: {error:#}",
                        self.session_id
                    );
                }
            }
            Ok(_) => {}
            Err(error) => eprintln!(
                "failed to load subagent type for workspace teardown {}: {error:#}",
                self.session_id
            ),
        }
    }

    async fn reconcile_abandoned_boundary_session(&self) -> std::result::Result<(), RpcError> {
        let activity = self.state.repo.activity(&self.session_id).await?;
        if activity == SessionActivity::Running
            && !session_has_live_tasks(&self.state, &self.session_id)
        {
            self.state
                .repo
                .mark_unfinished_actions_stale(&self.session_id)
                .await?;
        }
        let activity = self.state.repo.activity(&self.session_id).await?;
        if activity == SessionActivity::Idle {
            self.handle_subagent_terminal_for_parent_if_needed().await;
        }
        Ok(())
    }

    async fn try_handle_subagent_terminal_for_parent(&self) -> Option<EventFrame> {
        let notification_key = match self.subagent_idle_notification_key().await {
            Ok(Some(notification_key)) => notification_key,
            Ok(None) => return None,
            Err(error) => {
                eprintln!(
                    "failed to build subagent terminal notification key child={}: {}: {}",
                    self.session_id, error.code, error.message
                );
                return None;
            }
        };

        // Every parentful subagent is a delegation member (the only spawn path
        // sets a delegation_id). A delegation member's completion is delivered as
        // ONE typed delegation wakeup observation,
        // NOT a per-child idle. Fire the once-gate WITHOUT writing a
        // parent-visible SubagentIdle row (so events_after / the product UI never
        // surface per-child idle), then — on that single firing — destroy the RO
        // snapshot and run the barrier. The barrier is single-flighted by the DB
        // delegation-row CAS, so concurrent terminal children wake the parent exactly
        // once. Return None: the per-child idle is suppressed for the parent.
        let delegation_id = match self
            .state
            .repo
            .session_delegation_id(&self.session_id)
            .await
        {
            Ok(Some(delegation_id)) => delegation_id,
            Ok(None) => return None,
            Err(error) => {
                eprintln!(
                    "failed to load delegation id for subagent {}: {error:#}",
                    self.session_id
                );
                return None;
            }
        };
        let first_fire = match self
            .state
            .repo
            .claim_subagent_idle_once(&self.session_id, &notification_key)
            .await
        {
            Ok(first_fire) => first_fire,
            Err(error) => {
                eprintln!(
                    "failed to claim delegation-member idle once-gate child={}: {error:#}",
                    self.session_id
                );
                return None;
            }
        };
        if first_fire {
            self.destroy_read_only_subagent_workspaces().await;
        }
        self.try_delegation_barrier(&delegation_id).await;
        None
    }

    /// The delegation barrier: when a subagent of a delegation reaches its
    /// once-only terminal idle, complete the delegation iff every subagent is
    /// terminal. The `finish_delegation` CAS (`status='running'` +
    /// `attempt_id` fence) is the single-flight for terminal delegation status;
    /// only its winner publishes normal handoff files and then enqueues the
    /// deterministic parent wakeup observation. A cancellation winner therefore
    /// remains transcript-only.
    async fn try_delegation_barrier(&self, delegation_id: &str) {
        // `recover_if_needed` -> terminal idle -> barrier -> sibling
        // `recover_if_needed` is a recursive async cycle; box it to break the
        // infinitely-sized future. The cycle terminates because each sibling's
        // barrier short-circuits once the delegation is no longer `running`.
        let future = Box::pin(crate::delegation_runner::complete_delegation_if_ready(
            &self.state,
            delegation_id,
        ));
        if let Err(error) = future.await {
            eprintln!(
                "delegation barrier failed for delegation {delegation_id} (child {}): {}: {}",
                self.session_id, error.code, error.message
            );
        }
    }

    /// The once-gate dedup key (the terminal active leaf) for this child's
    /// parent-visible idle. `None` when this session has no parent (a top-level
    /// session). The caller only needs the key: a delegation member's completion
    /// is delivered as a single delegation wakeup observation, not a per-child
    /// idle payload.
    async fn subagent_idle_notification_key(
        &self,
    ) -> std::result::Result<Option<String>, RpcError> {
        if self
            .state
            .repo
            .session_parent_id(&self.session_id)
            .await?
            .is_none()
        {
            return Ok(None);
        }
        let turns = self
            .state
            .repo
            .transcript_turns(&self.session_id, None, Some(20))
            .await?;
        let notification_key = turns
            .cards
            .iter()
            .rev()
            .find(|card| card.outcome.is_some())
            .map(|card| format!("active_leaf:{}", card.active_leaf_id))
            .or_else(|| {
                turns
                    .active_leaf_id
                    .as_ref()
                    .map(|active_leaf_id| format!("active_leaf:{active_leaf_id}"))
            })
            .unwrap_or_else(|| "empty".to_string());
        Ok(Some(notification_key))
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
            .await?
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
                .await?;
            return Err(RpcError::new("invalid_input", error.to_string()));
        }

        let dispatches = self
            .persist_active_outputs(active, None, Some(queued), None, Vec::new())
            .await?;
        Ok(Some(dispatches))
    }

    pub(crate) async fn apply_agent_input(
        &self,
        active: Arc<Mutex<RuntimeSession>>,
        input: AgentInput,
        action_update: Option<ActionUpdate>,
    ) -> std::result::Result<Vec<DispatchAction>, RpcError> {
        self.ensure_action_can_complete(&action_update).await?;
        {
            let mut runtime = active.lock().await;
            runtime
                .session
                .enqueue_input(input)
                .map_err(|error| RpcError::new("invalid_input", error.to_string()))?;
        }
        self.persist_active_outputs(active, action_update, None, None, Vec::new())
            .await
    }

    pub(crate) async fn apply_session_input(
        &self,
        active: Arc<Mutex<RuntimeSession>>,
        input: SessionInput,
        action_update: Option<ActionUpdate>,
        provider_replay: Vec<ProviderReplayItem>,
    ) -> std::result::Result<Vec<DispatchAction>, RpcError> {
        self.ensure_action_can_complete(&action_update).await?;
        {
            let mut runtime = active.lock().await;
            runtime
                .session
                .enqueue_session_input(input)
                .map_err(|error| RpcError::new("invalid_input", error.to_string()))?;
        }
        self.persist_active_outputs(active, action_update, None, None, provider_replay)
            .await
    }

    async fn ensure_action_can_complete(
        &self,
        action_update: &Option<ActionUpdate>,
    ) -> std::result::Result<(), RpcError> {
        if let Some(update) = action_update {
            if !self
                .state
                .repo
                .action_can_complete(
                    &self.session_id,
                    &update.row_id,
                    &update.attempt_id,
                    update.post_compaction_dispatch_lease.as_ref(),
                )
                .await
                .context("check action can complete")?
            {
                return Err(RpcError::new(
                    "stale_action",
                    "action attempt is no longer running",
                ));
            }
        }
        Ok(())
    }

    pub(crate) async fn resume_model_turn(
        &self,
        checkpoint_leaf_id: &str,
        turn_id: agent_vocab::TurnId,
        action_id: agent_vocab::ActionId,
    ) -> std::result::Result<Vec<DispatchAction>, RpcError> {
        let config = self
            .state
            .repo
            .load_session_config(&self.session_id)
            .await?;
        self.state
            .workspaces
            .ensure_session(&self.session_id, &config.outer_cwd, &config.workspaces)
            .await?;
        let stored = self
            .state
            .repo
            .load_stored_session(&self.session_id)
            .await?;
        let mut session = AgentSession::from_stored_session(stored)
            .map_err(|error| RpcError::new("invalid_transcript", format!("{error:?}")))?;
        session
            .resume_model_turn(checkpoint_leaf_id, turn_id, action_id)
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
        self.persist_active_outputs_with_control(
            active,
            action_update,
            consumed_input,
            accepted_input,
            provider_replay,
            None,
        )
        .await
    }

    async fn persist_active_outputs_with_control(
        &self,
        active: Arc<Mutex<RuntimeSession>>,
        action_update: Option<ActionUpdate>,
        consumed_input: Option<QueuedInput>,
        accepted_input: Option<AcceptedInput>,
        provider_replay: Vec<ProviderReplayItem>,
        control_interrupt_input_id: Option<&str>,
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
        let consumed_client_input_id = consumed_input
            .as_ref()
            .and_then(|input| input.client_input_id.clone());
        let mut batch = OutputBatch::new(&entries, active_leaf_id.as_deref(), &events, &actions)
            .with_action_update(action_update)
            .with_consumed_input(consumed_input)
            .with_accepted_input(accepted_input)
            .with_provider(config.provider.clone());
        if let Some(input_id) = control_interrupt_input_id {
            batch = batch.with_control_interrupt(input_id);
        }
        let persisted = self
            .state
            .repo
            .persist_outputs(&self.session_id, batch)
            .await;
        let (frames, persisted_actions) = match persisted {
            Ok(persisted) => persisted,
            Err(error) => {
                self.state.active.lock().await.remove(&self.session_id);
                return Err(error.into());
            }
        };
        publish_events(&self.state, frames);
        let future = Box::pin(
            crate::delegation_runner::publish_next_partial_after_parent_decision(
                &self.state,
                &self.session_id,
                consumed_client_input_id.as_deref(),
            ),
        );
        if let Err(error) = future.await {
            eprintln!(
                "failed to publish next partial delegation wakeup after parent {} consumed {:?}: {}: {}",
                self.session_id,
                consumed_client_input_id,
                error.code,
                error.message
            );
        }
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
            .await?;
        let config = self
            .state
            .repo
            .load_session_config(&self.session_id)
            .await?;
        self.state
            .workspaces
            .ensure_session(&self.session_id, &config.outer_cwd, &config.workspaces)
            .await?;
        if let Some(provider) = pending.first().map(|action| action.provider.clone()) {
            if let Some(active) = self.active_session().await {
                active.lock().await.config.provider = provider;
            }
        }
        let resolved = pending
            .into_iter()
            .map(|action| {
                let mut dispatch_config = config.clone();
                dispatch_config.provider = action.provider;
                DispatchAction {
                    row_id: action.row_id,
                    attempt_id: action.attempt_id,
                    post_compaction_dispatch_lease: None,
                    action: action.action,
                    config: dispatch_config,
                }
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
        dispatch_all(&self.state, &self.session_id, ready).await;
        Ok(())
    }
}

pub(crate) async fn replace_active_session_config(
    state: &AppState,
    session_id: &str,
    config: SessionConfig,
) {
    let active = state.active.lock().await.get(session_id).cloned();
    if let Some(active) = active {
        let mut active = active.lock().await;
        let provider = active.config.provider.clone();
        active.config = config;
        active.config.provider = provider;
    }
}

fn session_uses_harness(config: &SessionConfig) -> bool {
    config
        .metadata
        .get("harness")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
