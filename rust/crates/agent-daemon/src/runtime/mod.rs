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
    AcceptedInput, ActionUpdate, EventFrame, EventType, OutputBatch, QueuedInput, SessionActivity,
    SessionConfig, SubagentType,
};
use agent_vocab::{ProviderReplayItem, TranscriptItem, TurnOutcome};
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
pub(crate) use tasks::{abort_session_tasks, session_has_live_tasks, take_tasks};

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

    /// Acquire only if the session's driver lock is free. Returns `None` if it
    /// is already held (the session is being driven or recovered elsewhere on
    /// the stack), letting the stage barrier recover sibling tails without
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
            self.reconcile_abandoned_boundary_session().await?;
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
            .await
            .map_err(anyhow::Error::from)?;
        let mut events = events;
        let activity = self
            .state
            .repo
            .activity(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        if !should_continue && activity == SessionActivity::Idle {
            if let Some(event) = self.try_subagent_parent_idle_event().await {
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
                .await
                .map_err(anyhow::Error::from)?;
            let mut events = vec![event];
            if let Some(event) = self.try_subagent_parent_idle_event().await {
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

    pub(crate) async fn notify_subagent_parent_idle_if_needed(&self) {
        if let Some(event) = self.try_subagent_parent_idle_event().await {
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
        let activity = self
            .state
            .repo
            .activity(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        if activity == SessionActivity::Running
            && !session_has_live_tasks(&self.state, &self.session_id)
        {
            self.state
                .repo
                .mark_unfinished_actions_stale(&self.session_id)
                .await
                .map_err(anyhow::Error::from)?;
        }
        let activity = self
            .state
            .repo
            .activity(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        if activity == SessionActivity::Idle {
            self.notify_subagent_parent_idle_if_needed().await;
        }
        Ok(())
    }

    async fn try_subagent_parent_idle_event(&self) -> Option<EventFrame> {
        let notification = match self.subagent_idle_notification().await {
            Ok(Some(notification)) => notification,
            Ok(None) => return None,
            Err(error) => {
                eprintln!(
                    "failed to build parent subagent idle event child={}: {}: {}",
                    self.session_id, error.code, error.message
                );
                return None;
            }
        };
        let (parent_session_id, notification_key, payload) = notification;

        // A stage member's completion is delivered as ONE stage steer, NOT a
        // per-child idle (FIX D). Fire the once-gate WITHOUT writing a
        // parent-visible SubagentIdle row (so events_after / the run board never
        // surface per-child idle for stage members), then — on that single firing
        // — destroy the RO snapshot and run the barrier. The barrier is
        // single-flighted by the DB stage-row CAS, so concurrent terminal children
        // steer the parent exactly once. Return None: the per-child idle is
        // suppressed for the parent in every case.
        let stage_id = match self.state.repo.session_stage_id(&self.session_id).await {
            Ok(stage_id) => stage_id,
            Err(error) => {
                // Conservative on a lookup error: emit nothing rather than a
                // possibly-wrong per-child idle for what may be a stage member.
                eprintln!(
                    "failed to load stage id for subagent {}: {error:#}",
                    self.session_id
                );
                return None;
            }
        };
        if let Some(stage_id) = stage_id {
            let first_fire = match self
                .state
                .repo
                .claim_subagent_idle_once(&self.session_id, &notification_key)
                .await
            {
                Ok(first_fire) => first_fire,
                Err(error) => {
                    eprintln!(
                        "failed to claim stage-member idle once-gate child={}: {error:#}",
                        self.session_id
                    );
                    return None;
                }
            };
            if first_fire {
                self.destroy_read_only_subagent_workspaces().await;
            }
            self.try_stage_barrier(&stage_id).await;
            return None;
        }

        // Non-stage subagent: the per-child idle IS the parent's signal. Insert it
        // once (the same once-gate dedup), and on that single firing reclaim a
        // read-only subagent's disposable snapshot — its handoff/transcript is
        // durable in Postgres, so the snapshot is safe to destroy.
        match self
            .state
            .repo
            .insert_subagent_idle_event_once(
                &parent_session_id,
                &self.session_id,
                &notification_key,
                payload,
            )
            .await
        {
            Ok(event) => {
                if event.is_some() {
                    self.destroy_read_only_subagent_workspaces().await;
                }
                event
            }
            Err(error) => {
                eprintln!(
                    "failed to publish parent subagent idle event child={}: {error:#}",
                    self.session_id
                );
                None
            }
        }
    }

    /// The stage barrier: when a subagent of a stage reaches its once-only
    /// terminal idle, complete the stage iff every subagent is terminal. The
    /// `finish_stage` CAS (stage-row `for update` lock + `status='running'`
    /// fence) is the single-flight; whichever caller wins it (a concurrent
    /// terminal child or the boot sweep) renders the handoff and steers the
    /// parent exactly once.
    async fn try_stage_barrier(&self, stage_id: &str) {
        // `recover_if_needed` -> terminal idle -> barrier -> sibling
        // `recover_if_needed` is a recursive async cycle; box it to break the
        // infinitely-sized future. The cycle terminates because each sibling's
        // barrier short-circuits once the stage is no longer `running`.
        let future = Box::pin(crate::stage_runner::complete_stage_if_ready(
            &self.state,
            stage_id,
        ));
        if let Err(error) = future.await {
            eprintln!(
                "stage barrier failed for stage {stage_id} (child {}): {}: {}",
                self.session_id, error.code, error.message
            );
        }
    }

    /// The parent-visible idle notification for this child: its parent id, the
    /// once-gate dedup key (the terminal active leaf), and the event payload.
    /// `None` when this session has no parent (a top-level session).
    async fn subagent_idle_notification(
        &self,
    ) -> std::result::Result<Option<(String, String, Value)>, RpcError> {
        let Some(parent_session_id) = self
            .state
            .repo
            .session_parent_id(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?
        else {
            return Ok(None);
        };
        let config = self
            .state
            .repo
            .load_session_config(&self.session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let turns = self
            .state
            .repo
            .transcript_turns(&self.session_id, None, Some(20))
            .await
            .map_err(anyhow::Error::from)?;
        // A subagent with no finished turn defaults to Crashed, consistent with
        // the durable handoff classification (`handoff::subagent_outcome`): a
        // session that reached idle without any TurnFinished did not finish
        // gracefully.
        let outcome = turns
            .cards
            .iter()
            .rev()
            .find_map(|card| card.outcome)
            .unwrap_or(TurnOutcome::Crashed);
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
        let text = turns
            .cards
            .iter()
            .rev()
            .filter_map(|card| card.assistant_message.as_ref())
            .find_map(|entry| match &entry.item {
                TranscriptItem::AssistantMessage(message) => Some(message.text()),
                _ => None,
            })
            .unwrap_or_default();
        let summary_preview = if text.chars().count() > 500 {
            format!("{}…", text.chars().take(500).collect::<String>())
        } else {
            text
        };
        let payload = json!({
            "child_session_id": self.session_id,
            "role": config.metadata.get("role_name").and_then(Value::as_str),
            "role_workspace": config.metadata.get("role_workspace").and_then(Value::as_str),
            "display_name": config.metadata.get("display_name").and_then(Value::as_str),
            "outcome": outcome,
            "summary_preview": summary_preview,
        });
        Ok(Some((parent_session_id, notification_key, payload)))
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

pub(crate) async fn replace_active_session_config(
    state: &AppState,
    session_id: &str,
    config: SessionConfig,
) {
    let active = state.active.lock().await.get(session_id).cloned();
    if let Some(active) = active {
        active.lock().await.config = config;
    }
}

fn session_uses_harness(config: &SessionConfig) -> bool {
    config
        .metadata
        .get("harness")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
