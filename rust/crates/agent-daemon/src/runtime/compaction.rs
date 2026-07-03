use agent_core::AgentInput;
use agent_session::{AgentSession, SessionAction, TranscriptStorageNode};
use agent_store::{
    ActionKind, ActionStatus, CompactionCompletion, CompactionJob, CompactionScope,
    CompactionTrigger, SessionConfig,
};
use agent_vocab::{ProviderReplayItem, TranscriptItem};

use crate::provider_runtime::{
    auto_limit_tokens, compaction_auto_explicitly_disabled, compaction_auto_state,
    compaction_config_with_model_metadata, model_input_tokens_for_gate, model_metadata_for_config,
    run_compaction,
};
use crate::state::{AppState, RunningTask};
use crate::types::{DispatchAction, RpcError};

use super::dispatch::spawn_model_dispatch;
use super::events::publish_events;
use super::tasks::{prune_finished_tasks, register_task, unregister_task};
use super::{history_error_to_rpc, session_uses_harness, SessionDriver};

impl SessionDriver {
    pub(super) async fn gate_model_dispatch(
        &self,
        dispatch: &DispatchAction,
    ) -> std::result::Result<bool, RpcError> {
        let Some(eligible) =
            check_compaction_eligible(&self.state, &self.session_id, dispatch).await
        else {
            return Ok(true);
        };
        let Some(limit) = eligible.limit else {
            return Ok(true);
        };
        let SessionAction::RequestModel {
            model_context,
            context_leaf_id,
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

        let tokens = match model_input_tokens_for_gate(
            &self.state,
            &dispatch.config,
            &self.session_id,
            context_leaf_id.as_deref(),
            model_context.clone(),
        )
        .await
        {
            Ok(tokens) => tokens,
            Err(error) => {
                let provider_error = error.downcast_ref::<agent_provider::ProviderError>();
                if provider_error.is_some_and(agent_provider::ProviderError::is_context_overflow) {
                    limit
                } else {
                    return Err(error.into());
                }
            }
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

async fn check_compaction_eligible(
    state: &AppState,
    session_id: &str,
    dispatch: &DispatchAction,
) -> Option<CompactionEligible> {
    let SessionAction::RequestModel {
        context_leaf_id, ..
    } = &dispatch.action
    else {
        return None;
    };
    if session_uses_harness(&dispatch.config) {
        return None;
    }
    let source_leaf_id = context_leaf_id.as_deref()?;
    let auto_state = compaction_auto_state(&dispatch.config);
    if auto_state.suppressed || auto_state.last_failure_leaf_id.as_deref() == Some(source_leaf_id) {
        return None;
    }
    if compaction_auto_explicitly_disabled(&dispatch.config) {
        return None;
    }
    let discovered = if dispatch.config.provider.kind == agent_vocab::ProviderKind::Claude {
        model_metadata_for_config(state, &dispatch.config, session_id)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let config = compaction_config_with_model_metadata(&dispatch.config, discovered);
    if !config.auto_enabled {
        return None;
    }
    Some(CompactionEligible {
        limit: auto_limit_tokens(&config),
    })
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
        .await?;
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
                .await?;
            if result.new_root_id.is_some() {
                state
                    .repo
                    .record_compaction_success(
                        &session_id,
                        result.new_root_id.as_deref(),
                        matches!(job.trigger, CompactionTrigger::Manual),
                    )
                    .await?;
                state
                    .provider_connections
                    .mark_compacted(&session_id, config.provider.kind, job.last_turn_id.0)
                    .await;
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
                    .await?;
            }
            fail_blocked_model_for_compaction_error(&state, &session_id, &job, &error).await?;
            (state.repo.fail_compaction_action(&job, error).await?, None)
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
            .await?
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
        // Mid-turn auto-compaction is used to recover from provider
        // context-overflow. Reinstalling the raw open-turn suffix would put
        // the same large tool outputs back into the next request and can
        // produce an endless compact/retry/overflow loop. The compaction
        // summary is the replacement checkpoint; resume the blocked model from
        // that compacted root without appending the pre-compaction suffix.
        CompactionScope::MidTurn { .. } => Ok(Vec::new()),
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
    let stored = state.repo.load_stored_session(session_id).await?;
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
        .await?;
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
        )
        .await?;
    driver.dispatch(dispatches).await?;
    Ok(())
}

pub(super) async fn recover_model_context_overflow_with_compaction(
    state: &AppState,
    session_id: &str,
    dispatch: &DispatchAction,
    provider_error: &agent_provider::ProviderError,
) -> std::result::Result<bool, RpcError> {
    if !provider_error.is_context_overflow() {
        return Ok(false);
    }
    let Some(eligible) = check_compaction_eligible(state, session_id, dispatch).await else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_session::ModelContext;
    use agent_vocab::{ActionId, TurnId, UserMessage};

    fn mid_turn_job_with_open_suffix() -> CompactionJob {
        let model_context = ModelContext::from_transcript_items(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage(UserMessage::text("large current turn")),
        ]);
        CompactionJob {
            action_row_id: "compaction_action".to_string(),
            attempt_id: "compaction_attempt".to_string(),
            source_session_id: "session".to_string(),
            source_leaf_id: "leaf".to_string(),
            model_context: model_context.clone(),
            compaction_context: model_context,
            tokens_before: Some(250_000),
            last_turn_id: TurnId(1),
            turn_started_at_ms: Some(1_700_000_000_000),
            trigger: CompactionTrigger::Auto {
                reason: "provider context overflow before model completion".to_string(),
            },
            reason: Some("provider context overflow before model completion".to_string()),
            scope: CompactionScope::MidTurn {
                source_leaf_id: "leaf".to_string(),
                turn_id: TurnId(1),
                blocked_model_action_id: ActionId(1),
                blocked_model_action_row_id: "model_action".to_string(),
                blocked_model_attempt_id: "model_attempt".to_string(),
            },
        }
    }

    #[test]
    fn mid_turn_overflow_compaction_does_not_reinstall_raw_open_turn_suffix() {
        let suffix =
            continuation_suffix_for_scope(&mid_turn_job_with_open_suffix()).expect("suffix builds");

        assert!(suffix.is_empty());
    }
}
