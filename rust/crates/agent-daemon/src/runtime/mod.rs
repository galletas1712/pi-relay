mod compaction;
mod dispatch;
mod errors;
mod events;
mod model;
mod outputs;
mod tasks;
mod tool;

use std::sync::Arc;

use agent_core::AgentInput;
use agent_session::{AgentSession, SessionAction, SessionInput};
use agent_store::{
    AcceptedInput, ActionUpdate, EventType, OutputBatch, QueuedInput, SessionConfig,
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
pub(crate) use errors::{history_error_to_rpc, map_queued_mutation_error};
pub(crate) use events::{clear_event_buffer_if_idle, publish_events};
use outputs::attach_provider_replay;
pub(crate) use outputs::{
    agent_input_from_queued_priority, attach_dispatch_config, collect_runtime_outputs,
};
pub(crate) use tasks::{abort_session_tasks, take_tasks};

pub(crate) async fn ensure_expected_active_leaf(
    state: &AppState,
    session_id: &str,
    params: &Value,
) -> std::result::Result<(), RpcError> {
    if params.get("expected_active_leaf_id").is_none() {
        return Ok(());
    }
    let active_leaf_id = state
        .repo
        .active_leaf_id(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    ensure_expected_active_leaf_matches(&active_leaf_id, params)
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
            .await
            .map_err(anyhow::Error::from)?;
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
            .await
            .map_err(anyhow::Error::from)?;
        self.state
            .workspaces
            .ensure_session(&self.session_id, &config.outer_cwd, &config.workspaces)
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
            .await
            .map_err(anyhow::Error::from)?;
        self.state
            .workspaces
            .ensure_session(&self.session_id, &config.outer_cwd, &config.workspaces)
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
        self.state
            .workspaces
            .ensure_session(&self.session_id, &config.outer_cwd, &config.workspaces)
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
}

fn session_uses_harness(config: &SessionConfig) -> bool {
    config
        .metadata
        .get("harness")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
