use agent_core::AgentInput;
use agent_session::{AgentSession, ModelContextEntry, SessionAction};
use agent_store::{
    ActionKind, ActionStatus, CompactionCompletion, CompactionJob, CompactionScope,
    CompactionTrigger, SessionConfig,
};
use agent_vocab::TranscriptItem;

use crate::provider_runtime::{
    compaction_auto_state, compaction_config_with_model_metadata, model_input_tokens_for_gate,
    model_metadata_for_config, parse_compaction_policy, run_compaction,
};
use crate::state::{AppState, RunningTask, TaskRegistrationId};
use crate::types::{DispatchAction, RpcError, RuntimeSession};

use super::dispatch::spawn_model_dispatch;
use super::events::publish_events;
use super::tasks::{
    action_has_live_task_for_lease, prune_finished_tasks, register_task, unregister_task,
    TaskRegistrationRejected,
};
use super::{history_error_to_rpc, session_uses_harness, SessionDriver};

impl SessionDriver {
    pub(super) async fn gate_model_dispatch(
        &self,
        dispatch: &DispatchAction,
    ) -> std::result::Result<bool, RpcError> {
        agent_perf::compaction_gate_pass();
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
        // A bare compacted root is already the smallest possible transcript.
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

async fn pause_after_reactive_compaction_transition_for_test(_dispatch: &DispatchAction) {
    #[cfg(test)]
    if let Some(milliseconds) = _dispatch
        .config
        .metadata
        .pointer("/fault_injection/pause_after_reactive_compaction_transition_ms")
        .and_then(serde_json::Value::as_u64)
    {
        tokio::time::sleep(std::time::Duration::from_millis(milliseconds)).await;
    }
}

fn recompaction_limit_reached(
    auto_state: &crate::provider_runtime::CompactionAutoState,
    source_leaf_id: &str,
) -> bool {
    auto_state.last_success_leaf_id.as_deref() == Some(source_leaf_id)
        && auto_state.consecutive_recompactions >= 1
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
    if recompaction_limit_reached(&auto_state, source_leaf_id) {
        // One immediate recompaction is allowed in case a provider-generated
        // summary still does not fit. A third overflow after two successful
        // checkpoints must fall through to the normal terminal model-error
        // path rather than compacting forever.
        return None;
    }
    let policy = parse_compaction_policy(&dispatch.config);
    if policy.explicitly_disables_auto() {
        return None;
    }
    let discovered = model_metadata_for_config(state, &dispatch.config, session_id)
        .await
        .ok()
        .flatten();
    let config = compaction_config_with_model_metadata(discovered, &policy);
    if !config.auto_enabled {
        return None;
    }
    Some(CompactionEligible {
        limit: config.auto_limit_tokens,
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
            dispatch.post_compaction_dispatch_lease.as_ref(),
            trigger,
            tokens_before,
            limit,
        )
        .await?;
    publish_events(state, created.events);
    if matches!(created.job.scope, CompactionScope::Boundary { .. }) {
        state.active.lock().await.remove(session_id);
    }
    pause_after_reactive_compaction_transition_for_test(dispatch).await;
    if spawn_compaction(
        state,
        session_id.to_string(),
        created.job,
        dispatch.config.clone(),
    )
    .is_err()
    {
        // The durable block/compaction transition already committed. Shutdown
        // must not terminally compensate it; boot recovery owns the row.
        return Ok(());
    }
    Ok(())
}

async fn compensate_resumed_action(
    state: &AppState,
    driver: &SessionDriver,
    session_id: &str,
    resumed: &agent_store::PersistedAction,
    post_compaction_dispatch_lease: Option<&agent_store::PostCompactionDispatchLease>,
    error: String,
) -> std::result::Result<(), RpcError> {
    if post_compaction_dispatch_lease
        .is_some_and(|lease| action_has_live_task_for_lease(state, &resumed.row_id, lease))
    {
        return Ok(());
    }
    let events = state
        .repo
        .fail_unfinished_model_action(
            session_id,
            &resumed.row_id,
            &resumed.attempt_id,
            post_compaction_dispatch_lease,
            &error,
        )
        .await?;
    let compensated = !events.is_empty();
    publish_events(state, events);
    if compensated {
        state.active.lock().await.remove(session_id);
        driver.recover_if_needed().await?;
        Err(RpcError::new("compaction_resume_failed", error))
    } else {
        Ok(())
    }
}

pub(crate) fn spawn_compaction(
    state: &AppState,
    session_id: String,
    job: CompactionJob,
    config: SessionConfig,
) -> Result<(), TaskRegistrationRejected> {
    prune_finished_tasks(state, None);
    let action_row_id = job.action_row_id.clone();
    let task_state = state.clone();
    let task_session_id = session_id.clone();
    let task_action_row_id = action_row_id.clone();
    let registration_id = TaskRegistrationId::new();
    let task_registration_id = registration_id.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let perf = agent_perf::Metrics::new_if_enabled(agent_perf::Operation::Compaction);
    let handle = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        let action_row_id = job.action_row_id.clone();
        let operation = run_compaction_job(task_state.clone(), session_id.clone(), job, config);
        let result = match perf.as_ref() {
            Some(perf) => perf.scope(operation).await,
            None => operation.await,
        };
        if !unregister_task(&task_state, &action_row_id, &task_registration_id) {
            return;
        }
        if let Err(error) = &result {
            eprintln!(
                "compaction task failed {session_id}: {}: {}",
                error.code, error.message
            );
            // The durable transition may already have committed. Never leave a
            // stale live projection suppressing recovery when the task exits.
            task_state.active.lock().await.remove(&session_id);
            let driver = SessionDriver::acquire(&task_state, &session_id).await;
            if let Err(recovery_error) = driver.recover_if_needed().await {
                eprintln!(
                    "compaction task recovery failed {session_id}: {}: {}",
                    recovery_error.code, recovery_error.message
                );
            }
        }
        if let Some(perf) = perf {
            let outcome = if result.is_ok() {
                agent_perf::Outcome::Completed
            } else {
                agent_perf::Outcome::Failed
            };
            perf.finish(outcome);
        }
    });
    register_task(
        state,
        RunningTask {
            session_id: task_session_id,
            action_row_id: task_action_row_id,
            registration_id,
            post_compaction_dispatch_lease: None,
            kind: ActionKind::Compaction,
            handle,
        },
        start_tx,
    )?;
    Ok(())
}

async fn run_compaction_job(
    state: AppState,
    session_id: String,
    job: CompactionJob,
    config: SessionConfig,
) -> std::result::Result<(), RpcError> {
    let result = match continuation_suffix_for_scope(&job) {
        Ok(continuation_suffix) => {
            #[cfg(test)]
            if config
                .metadata
                .pointer("/fault_injection/pause_compaction_dispatch_before_provider")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                std::future::pending::<()>().await;
            }
            run_compaction(&state, &config, &session_id, job.compaction_context.clone())
                .await
                .map(|output| (output, continuation_suffix))
        }
        Err(error) => Err(anyhow::anyhow!(error.message)),
    };
    let (events, resumed, failure) = match result {
        Ok((output, continuation_suffix)) => {
            let completion = CompactionCompletion {
                summary: output.summary,
                summary_kind: output.summary_kind.as_str().to_string(),
                provider_replay: output.provider_replay,
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
                    .provider_connections
                    .mark_compacted(&session_id, config.provider.kind, job.last_turn_id.0)
                    .await;
            }
            (result.events, result.resumed_model_action, None)
        }
        Err(error) => {
            let error = error.to_string();
            let events = state
                .repo
                .fail_compaction_action(&job, &config, error.clone())
                .await?;
            let committed = !events.is_empty();
            (events, None, committed.then_some(error))
        }
    };
    let transitioned = !events.is_empty() || resumed.is_some();
    publish_events(&state, events);
    if !transitioned {
        return Ok(());
    }

    let driver = SessionDriver::acquire(&state, &session_id).await;
    if let Some(error) = failure {
        apply_compaction_failure_to_runtime(&state, &driver, &session_id, &job, &error).await?;
    }
    if let Some(resumed) = resumed {
        super::ensure_post_compaction_dispatch_recovery(&state);
        let resumed_config = match install_runtime_compaction_checkpoint(&state, &session_id, &job)
            .await
        {
            Ok(config) => config,
            Err(error) => {
                eprintln!(
                        "failed to install committed compaction checkpoint/config {session_id}/{}; retaining marker for recovery: {}",
                        resumed.row_id, error.message
                    );
                state.active.lock().await.remove(&session_id);
                state.post_compaction_recovery_notify.notify_one();
                return Ok(());
            }
        };
        let dispatch = DispatchAction {
            row_id: resumed.row_id.clone(),
            attempt_id: resumed.attempt_id.clone(),
            post_compaction_dispatch_lease: None,
            action: resumed.action.clone(),
            config: resumed_config,
        };
        // Harness sessions deliberately expose pending model actions to the
        // development RPC instead of owning an internal runner.
        if session_uses_harness(&dispatch.config) {
            return Ok(());
        }
        let intent = agent_store::PostCompactionDispatchIntent {
            session_id: session_id.clone(),
            row_id: dispatch.row_id.clone(),
            attempt_id: dispatch.attempt_id.clone(),
        };
        let claimed = match state
            .repo
            .claim_post_compaction_model_action(
                &intent,
                agent_store::POST_COMPACTION_DISPATCH_LEASE_DURATION,
            )
            .await
        {
            Ok(claimed) => claimed,
            Err(agent_store::PostCompactionDispatchClaimError::Corrupt(error)) => {
                let message = format!(
                    "failed to reconstruct model action after compaction: {}",
                    error.message()
                );
                let events = state
                    .repo
                    .fail_corrupt_post_compaction_model_action(&intent, error.fence(), &message)
                    .await?;
                publish_events(&state, events);
                state.active.lock().await.remove(&session_id);
                driver.recover_if_needed().await?;
                return Err(RpcError::new("compaction_resume_failed", message));
            }
            Err(agent_store::PostCompactionDispatchClaimError::Transient(error)) => {
                eprintln!(
                    "failed to claim model action after compaction {session_id}/{}; retaining marker for recovery: {error:#}",
                    resumed.row_id
                );
                state.post_compaction_recovery_notify.notify_one();
                return Ok(());
            }
        };
        let Some(claimed) = claimed else {
            return compensate_resumed_action(
                &state,
                &driver,
                &session_id,
                &resumed,
                None,
                "resumed model action was no longer pending after compaction".to_string(),
            )
            .await;
        };
        let dispatch = DispatchAction {
            row_id: claimed.pending.row_id,
            attempt_id: claimed.pending.attempt_id,
            post_compaction_dispatch_lease: Some(claimed.lease),
            action: claimed.pending.action,
            config: dispatch.config,
        };
        let lease = dispatch.post_compaction_dispatch_lease.clone();
        let registration_id = spawn_model_dispatch(
            state.clone(),
            session_id.clone(),
            dispatch,
            true,
            agent_perf::Metrics::new_if_enabled(agent_perf::Operation::ModelAction),
        )
        .await;
        if matches!(registration_id, Err(TaskRegistrationRejected)) {
            // Shutdown owns the runner barrier now. Keep the exact durable
            // lease for expiry/recovery on the next boot.
            return Ok(());
        }
        if !registration_id
            .ok()
            .flatten()
            .as_ref()
            .is_some_and(|registration_id| {
                super::tasks::task_registration_is_live(&state, &resumed.row_id, registration_id)
            })
        {
            return compensate_resumed_action(
                &state,
                &driver,
                &session_id,
                &resumed,
                lease.as_ref(),
                "failed to register resumed model runner after compaction".to_string(),
            )
            .await;
        }
        // A successful claim has a synchronously registered runner; a failed
        // claim is reconciled above without dispatching a duplicate.
        // Do not let unrelated queue driving compensate an action that now has
        // a registered runner.
        return Ok(());
    }
    driver.drive_until_blocked().await?;
    Ok(())
}

fn continuation_suffix_for_scope(
    job: &CompactionJob,
) -> std::result::Result<Vec<ModelContextEntry>, RpcError> {
    match &job.scope {
        CompactionScope::Boundary { .. } => Ok(Vec::new()),
        CompactionScope::MidTurn { .. } => {
            let Some(open_turn) = job.model_context.open_turn_entries() else {
                return if matches!(
                    job.model_context.transcript_items(),
                    [TranscriptItem::CompactionSummary(_)]
                ) {
                    // A blocked action remains MidTurn across immediate
                    // recompaction even when its checkpoint is a bare summary.
                    Ok(Vec::new())
                } else {
                    Err(RpcError::new(
                        "invalid_compaction",
                        "mid-turn compaction source has no open turn",
                    ))
                };
            };
            Ok(open_turn
                .filter_map(|(item, provider_replay)| {
                    let TranscriptItem::UserMessage(message) = item else {
                        return None;
                    };
                    Some(ModelContextEntry {
                        item: TranscriptItem::UserMessage(message.clone()),
                        provider_replay: provider_replay.to_vec(),
                    })
                })
                .collect())
        }
    }
}

async fn install_runtime_compaction_checkpoint(
    state: &AppState,
    session_id: &str,
    job: &CompactionJob,
) -> std::result::Result<SessionConfig, RpcError> {
    let stored = state.repo.load_stored_session(session_id).await?;
    let config = state.repo.load_session_config(session_id).await?;
    state
        .workspaces
        .ensure_session(session_id, &config.outer_cwd, &config.workspaces)
        .await?;
    let mut session = AgentSession::from_stored_session_preserving_open_turn(stored)
        .map_err(history_error_to_rpc)?;
    if let CompactionScope::MidTurn {
        turn_id,
        blocked_model_action_id,
        ..
    } = &job.scope
    {
        let action_id = *blocked_model_action_id;
        let active_leaf_id = session
            .transcript_store()
            .active_leaf_id()
            .map(str::to_string)
            .ok_or_else(|| {
                RpcError::new("invalid_compaction", "compaction produced no active leaf")
            })?;
        session
            .restore_compacted_runtime(&active_leaf_id, *turn_id, action_id)
            .map_err(history_error_to_rpc)?;
    }
    if let Some(active) = state.active.lock().await.get(session_id).cloned() {
        let mut runtime = active.lock().await;
        runtime.session = session;
        runtime.config = config.clone();
    } else {
        state.active.lock().await.insert(
            session_id.to_string(),
            std::sync::Arc::new(tokio::sync::Mutex::new(RuntimeSession {
                session,
                config: config.clone(),
            })),
        );
    }
    Ok(config)
}

async fn apply_compaction_failure_to_runtime(
    state: &AppState,
    driver: &SessionDriver,
    session_id: &str,
    job: &CompactionJob,
    error: &str,
) -> std::result::Result<(), RpcError> {
    let CompactionScope::MidTurn {
        turn_id,
        blocked_model_action_id,
        ..
    } = &job.scope
    else {
        return Ok(());
    };
    let model_error = format!("compaction failed before model dispatch: {error}");
    let Some(active) = state.active.lock().await.get(session_id).cloned() else {
        driver.recover_if_needed().await?;
        return Ok(());
    };
    let action_id = *blocked_model_action_id;
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
    use agent_vocab::{
        ActionId, AssistantItem, AssistantMessage, CompactionSummary, DaemonToolObservation,
        ToolCall, ToolCallId, ToolResultMessage, ToolResultStatus, TurnId, UserMessage,
    };

    fn compaction_job(model_context: ModelContext, scope: CompactionScope) -> CompactionJob {
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
            scope,
        }
    }

    fn mid_turn_scope() -> CompactionScope {
        CompactionScope::MidTurn {
            source_leaf_id: "leaf".to_string(),
            turn_id: TurnId(1),
            blocked_model_action_id: ActionId(1),
            blocked_model_action_row_id: "model_action".to_string(),
            blocked_model_attempt_id: "model_attempt".to_string(),
        }
    }

    fn tool_call() -> ToolCall {
        ToolCall {
            id: ToolCallId::from_u64(7),
            tool_name: "Bash".to_string(),
            args_json: r#"{"command":"printf large-output"}"#.to_string(),
        }
    }

    #[test]
    fn boundary_compaction_has_no_continuation_suffix() {
        let job = compaction_job(
            ModelContext::from_transcript_items(vec![TranscriptItem::UserMessage(
                UserMessage::text("finished history"),
            )]),
            CompactionScope::Boundary {
                source_leaf_id: "leaf".to_string(),
            },
        );

        let suffix = continuation_suffix_for_scope(&job).expect("suffix builds");
        assert!(suffix.is_empty());
    }

    #[test]
    fn proactive_mid_turn_compaction_retains_user_message() {
        let user = UserMessage::text("answer with the requested sentinels");
        let job = compaction_job(
            ModelContext::from_transcript_items(vec![
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage(user.clone()),
            ]),
            mid_turn_scope(),
        );

        let suffix = continuation_suffix_for_scope(&job).expect("suffix builds");

        assert_eq!(suffix.len(), 1);
        assert_eq!(suffix[0].item, TranscriptItem::UserMessage(user));
        assert!(suffix[0].provider_replay.is_empty());
    }

    #[test]
    fn tool_heavy_mid_turn_retains_user_messages_without_generated_output() {
        let tool_call = tool_call();
        let initial = UserMessage::text("initial instruction");
        let steering = UserMessage::text("later steering instruction");
        let job = compaction_job(
            ModelContext::from_transcript_items(vec![
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage(initial.clone()),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::ToolCall(tool_call.clone())],
                }),
                TranscriptItem::ToolCallStarted {
                    turn_id: TurnId(1),
                    tool_call: tool_call.clone(),
                },
                TranscriptItem::ToolResult(ToolResultMessage {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.tool_name.clone(),
                    output: "very large generated output".to_string(),
                    status: ToolResultStatus::Success,
                }),
                TranscriptItem::DaemonToolObservation(DaemonToolObservation::new(
                    ToolCallId::from_u64(8),
                    "delegate",
                    "{}",
                    serde_json::json!({"output": "generated observation"}),
                    ToolResultStatus::Success,
                    None,
                )),
                TranscriptItem::UserMessage(steering.clone()),
            ]),
            mid_turn_scope(),
        );

        let suffix = continuation_suffix_for_scope(&job).expect("suffix builds");

        assert_eq!(
            suffix
                .iter()
                .map(|node| node.item.clone())
                .collect::<Vec<_>>(),
            vec![
                TranscriptItem::UserMessage(initial),
                TranscriptItem::UserMessage(steering),
            ]
        );
        assert!(suffix.iter().all(|entry| entry.provider_replay.is_empty()));
    }

    #[test]
    fn repeated_compaction_reads_users_from_summary_rooted_open_turn() {
        let user = UserMessage::text("instruction retained by the first compaction");
        let job = compaction_job(
            ModelContext::from_transcript_items(vec![
                TranscriptItem::CompactionSummary(CompactionSummary::new(
                    "session",
                    "old_leaf",
                    "first summary",
                    None,
                    TurnId(1),
                )),
                TranscriptItem::UserMessage(user.clone()),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("generated output".to_string())],
                }),
            ]),
            mid_turn_scope(),
        );

        let suffix = continuation_suffix_for_scope(&job).expect("suffix builds");

        assert_eq!(suffix.len(), 1);
        assert_eq!(suffix[0].item, TranscriptItem::UserMessage(user));
    }

    #[test]
    fn repeated_compaction_from_bare_summary_has_no_suffix() {
        let job = compaction_job(
            ModelContext::from_transcript_items(vec![TranscriptItem::CompactionSummary(
                CompactionSummary::new("session", "old_leaf", "first summary", None, TurnId(1)),
            )]),
            mid_turn_scope(),
        );

        let suffix = continuation_suffix_for_scope(&job).expect("suffix builds");

        assert!(suffix.is_empty());
    }

    #[test]
    fn mid_turn_compaction_without_open_turn_is_invalid() {
        let job = compaction_job(
            ModelContext::from_transcript_items(vec![TranscriptItem::UserMessage(
                UserMessage::text("missing turn anchor"),
            )]),
            mid_turn_scope(),
        );

        let error = continuation_suffix_for_scope(&job).expect_err("invalid source must fail");

        assert_eq!(error.code, "invalid_compaction");
    }

    #[test]
    fn repeated_successful_recompaction_is_bounded() {
        let state = crate::provider_runtime::CompactionAutoState {
            last_success_leaf_id: Some("current-compaction-leaf".to_string()),
            consecutive_recompactions: 1,
            ..Default::default()
        };

        assert!(recompaction_limit_reached(
            &state,
            "current-compaction-leaf"
        ));
        assert!(!recompaction_limit_reached(&state, "older-leaf"));
    }
}
