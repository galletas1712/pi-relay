use std::path::PathBuf;

use agent_core::AgentInput;
use agent_store::{
    Delegation, DelegationKind, DelegationProgress, DelegationStatus, DelegationSubagent,
    DelegationSubagentOverview, QueuedInputStatus, SubagentControlPhase, SubagentControlRecord,
    SubagentType,
};
use agent_vocab::{ToolCall, ToolResultMessage, UserMessage};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::codec::from_params;
use crate::delegation_snapshot::{build_delegation_snapshot, progress_view};
use crate::handoff::{
    delegation_dir, handoff_root, refresh_delegation_handoff_artifacts,
    refresh_task_prompt_artifact_if_present, render_transcript_markdown, safe_handoff_path_segment,
    task_prompt_rel, TASK_PROMPT_FILE,
};
use crate::runtime::{abort_session_tasks, publish_events, SessionDriver};
use crate::state::AppState;
use crate::subagents::{spawn_subagent, DelegationSubagentSpawn};
use crate::types::RpcError;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartFullParams {
    /// Present for websocket `delegation.start_full`, absent for the
    /// model-facing `delegate_writing_task` tool. The core receives the already
    /// extracted parent id separately; this field exists so serde can reject
    /// every other unknown key instead of silently accepting stale vocabulary.
    #[serde(rename = "parent_session_id")]
    _parent_session_id: Option<String>,
    role: String,
    prompt: String,
    workflow: Option<String>,
    label: Option<String>,
}

fn map_delegation_create_error(error: anyhow::Error) -> RpcError {
    if error
        .downcast_ref::<agent_store::RunningDelegationConflict>()
        .is_some()
    {
        return RpcError::new(
            "delegation_already_running",
            "a delegation is already running for this session; wait for it to finish before starting another",
        );
    }
    error.into()
}

pub(crate) struct SubagentWorkState {
    pub(crate) has_unfinished_actions: bool,
    pub(crate) has_queued_inputs: bool,
    pub(crate) has_active_runtime: bool,
    pub(crate) active_leaf_is_turn_boundary: bool,
}

impl SubagentWorkState {
    pub(crate) fn has_active_work(&self) -> bool {
        self.has_unfinished_actions || self.has_queued_inputs || self.has_active_runtime
    }

    pub(crate) fn is_completion_terminal(&self) -> bool {
        self.active_leaf_is_turn_boundary && !self.has_active_work()
    }
}

pub(crate) async fn load_subagent_work_state(
    state: &AppState,
    subagent_id: &str,
) -> std::result::Result<SubagentWorkState, RpcError> {
    Ok(SubagentWorkState {
        has_unfinished_actions: state.repo.has_unfinished_actions(subagent_id).await?,
        has_queued_inputs: state.repo.has_queued_inputs(subagent_id).await?,
        has_active_runtime: subagent_has_active_runtime(state, subagent_id).await,
        active_leaf_is_turn_boundary: state.repo.active_leaf_is_turn_boundary(subagent_id).await?,
    })
}

pub(crate) async fn ensure_subagent_steer_allowed(
    state: &AppState,
    subagent_id: &str,
    parent_session_id: &str,
) -> std::result::Result<(), RpcError> {
    let delegation = load_subagent_scope(state, subagent_id, parent_session_id).await?;
    if delegation.status != DelegationStatus::Running {
        return Err(RpcError::new(
            "delegation_not_running",
            "cannot steer a subagent whose delegation is terminal",
        ));
    }
    let work_state = load_subagent_work_state(state, subagent_id).await?;
    // A running delegation row can briefly race a subagent reaching its terminal
    // transcript boundary before the barrier wins the delegation CAS. Callers
    // hold the child SessionDriver lock while invoking this helper and while
    // enqueueing the steer. A boundary leaf with queued/unfinished/runtime work
    // is still active; only an idle boundary child is completion-terminal.
    if work_state.is_completion_terminal() {
        return Err(RpcError::new(
            "subagent_terminal",
            "cannot steer a subagent that is already terminal",
        ));
    }
    if !work_state.has_active_work() {
        return Err(RpcError::new(
            "subagent_not_running",
            "cannot steer a subagent without active work or queued input",
        ));
    }
    Ok(())
}

async fn load_subagent_scope(
    state: &AppState,
    subagent_id: &str,
    parent_session_id: &str,
) -> std::result::Result<Delegation, RpcError> {
    let parent = state
        .repo
        .session_parent_id(subagent_id)
        .await
        .map_err(|error| {
            eprintln!("failed to load parent for subagent {subagent_id}: {error:#}");
            RpcError::new("subagent_not_found", "subagent not found")
        })?;
    if parent.as_deref() != Some(parent_session_id) {
        return Err(RpcError::new(
            "subagent_not_found",
            "subagent is not in scope",
        ));
    }
    match state.repo.session_subagent_type(subagent_id).await? {
        Some(SubagentType::Full | SubagentType::ReadOnly) => {}
        None => {
            return Err(RpcError::new(
                "subagent_not_found",
                "subagent is not in scope",
            ))
        }
    }
    let delegation_id = state
        .repo
        .session_delegation_id(subagent_id)
        .await
        .map_err(|error| {
            eprintln!("failed to load delegation for subagent {subagent_id}: {error:#}");
            RpcError::new("subagent_not_found", "subagent is not in scope")
        })?
        .ok_or_else(|| RpcError::new("subagent_not_found", "subagent is not in scope"))?;
    let delegation = state
        .repo
        .get_delegation(&delegation_id)
        .await?
        .ok_or_else(|| RpcError::new("delegation_not_found", "delegation not found"))?;
    if delegation.parent_session_id != parent_session_id {
        return Err(RpcError::new(
            "subagent_not_found",
            "subagent is not in scope",
        ));
    }
    Ok(delegation)
}

pub(crate) async fn steer_subagent_core(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: SteerSubagentParams = from_params(params)?;
    let subagent_id = trim_required(&params.subagent_id, "subagent_id")?;
    let message = trim_required(&params.message, "message")?;
    let interrupt = params.interrupt.unwrap_or(false);
    let client_control_id = params
        .client_control_id
        .as_deref()
        .map(|id| trim_required(id, "client_control_id"))
        .transpose()?
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // Derive the immutable ledger key and check replay before child recovery.
    // Historical controls remain readable after terminal completion, while a
    // new terminal-delegation request cannot mutate/reactivate the child.
    let scope = load_subagent_scope(state, &subagent_id, parent_session_id).await?;
    let client_input_id = format!("subagent-control:{}:{}", scope.id, client_control_id);
    if let Some(control) = state
        .repo
        .get_scoped_subagent_control(
            &subagent_id,
            &client_input_id,
            parent_session_id,
            &scope.id,
            &UserMessage::text(&message),
            interrupt,
        )
        .await
        .map_err(map_subagent_control_store_error)?
    {
        if control.delegation_running
            && matches!(
                control.phase,
                SubagentControlPhase::PendingInterrupt
                    | SubagentControlPhase::InterruptApplied
                    | SubagentControlPhase::Ready
            )
            && matches!(
                control.status,
                QueuedInputStatus::Queued | QueuedInputStatus::Consuming
            )
        {
            crate::spawn_try_drive_until_blocked(
                state,
                subagent_id.clone(),
                "steer_subagent.replay",
            );
        }
        return Ok(subagent_control_result(&subagent_id, &control, true, None));
    }
    if scope.status != DelegationStatus::Running {
        return Err(RpcError::new(
            "delegation_not_running",
            "cannot steer a subagent whose delegation is terminal",
        ));
    }

    let driver = SessionDriver::acquire(state, &subagent_id).await;
    driver.reconcile_pending_subagent_controls().await?;
    driver.recover_if_needed().await?;
    ensure_subagent_steer_allowed(state, &subagent_id, parent_session_id).await?;
    let had_unfinished_actions = state.repo.has_unfinished_actions(&subagent_id).await?;
    let queued = state
        .repo
        .enqueue_scoped_subagent_steer(
            parent_session_id,
            &scope.id,
            &subagent_id,
            &UserMessage::text(message),
            &client_input_id,
            interrupt,
        )
        .await
        .map_err(map_subagent_control_store_error)?
        .ok_or_else(|| {
            RpcError::new(
                "delegation_not_running",
                "cannot steer a subagent whose delegation is terminal",
            )
        })?;
    let replayed = queued.replayed;
    if let Some(event) = queued.event {
        publish_events(state, vec![event]);
    }
    #[cfg(test)]
    if state
        .pause_subagent_control_after_commit
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        state.subagent_control_committed.notify_waiters();
        std::future::pending::<()>().await;
    }
    // Establish detached ownership immediately after the durable commit. This
    // single fresh-request waiter closes the commit-to-worker cancellation gap;
    // replay nudges use the nonwaiting helper above to avoid accumulation.
    crate::spawn_drive_until_blocked(state, subagent_id.clone(), "steer_subagent.accepted");
    let mut postcommit_error = None;
    let should_drive_inline = interrupt || !had_unfinished_actions;
    if should_drive_inline {
        let result = async {
            driver.reconcile_pending_subagent_controls().await?;
            driver.drive_until_blocked().await?;
            Ok::<(), RpcError>(())
        }
        .await;
        if let Err(error) = result {
            postcommit_error = Some(format!("{}: {}", error.code, error.message));
            record_accepted_control_drive_failure(
                state,
                &subagent_id,
                "steer_subagent.reconcile",
                &error,
            )
            .await;
        }
    }
    drop(driver);
    let current =
        match reload_accepted_subagent_control(state, &subagent_id, &queued.input_id).await {
            Ok(Some(control)) => control,
            Ok(None) => {
                let error = RpcError::new(
                    "accepted_control_missing",
                    "durably accepted subagent control could not be reloaded",
                );
                record_accepted_control_drive_failure(
                    state,
                    &subagent_id,
                    "steer_subagent.reload",
                    &error,
                )
                .await;
                return Ok(json!({
                    "subagent_id": subagent_id,
                    "accepted": true,
                    "queued": true,
                    "input_id": queued.input_id,
                    "replayed": replayed,
                    "phase": queued.control_phase,
                    "interrupted": Value::Null,
                    "drive_status": "pending",
                    "drive_error": error.message,
                }));
            }
            Err(error) => {
                eprintln!(
                "accepted subagent control status refresh failed session={subagent_id}: {error:#}"
            );
                return Ok(json!({
                    "subagent_id": subagent_id,
                    "accepted": true,
                    "queued": true,
                    "input_id": queued.input_id,
                    "replayed": replayed,
                    "phase": queued.control_phase,
                    "interrupted": Value::Null,
                    "drive_status": "pending",
                    "drive_error": error.to_string(),
                }));
            }
        };
    Ok(subagent_control_result(
        &subagent_id,
        &current,
        replayed,
        postcommit_error,
    ))
}

async fn reload_accepted_subagent_control(
    state: &AppState,
    subagent_id: &str,
    input_id: &str,
) -> anyhow::Result<Option<SubagentControlRecord>> {
    #[cfg(test)]
    if state
        .fail_subagent_control_reload_after_commit
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        anyhow::bail!("injected accepted-control status reload failure");
    }
    state
        .repo
        .get_subagent_control_by_input_id(subagent_id, input_id)
        .await
}

fn accepted_control_pending_result(
    subagent_id: &str,
    queued: &agent_store::EnqueueUserInputResult,
    replayed: bool,
    drive_error: String,
) -> Value {
    json!({
        "subagent_id": subagent_id,
        "accepted": true,
        "queued": true,
        "input_id": queued.input_id,
        "replayed": replayed,
        "phase": queued.control_phase,
        "interrupted": Value::Null,
        "interrupt_outcome": queued.control_interrupt_outcome,
        "drive_status": "pending",
        "drive_error": drive_error,
    })
}

fn subagent_control_result(
    subagent_id: &str,
    control: &SubagentControlRecord,
    replayed: bool,
    drive_error: Option<String>,
) -> Value {
    let drive_status = if drive_error.is_some() {
        "failed"
    } else {
        match control.phase {
            SubagentControlPhase::PendingInterrupt | SubagentControlPhase::InterruptApplied => {
                "pending"
            }
            SubagentControlPhase::Ready
                if matches!(
                    control.status,
                    QueuedInputStatus::Queued | QueuedInputStatus::Consuming
                ) =>
            {
                "started"
            }
            SubagentControlPhase::Ready => "settled",
            SubagentControlPhase::Cancelled => "cancelled",
        }
    };
    let interrupted = match control.phase {
        SubagentControlPhase::PendingInterrupt => Value::Null,
        _ => json!(control.interrupted),
    };
    json!({
        "subagent_id": subagent_id,
        "accepted": true,
        "queued": matches!(
            control.status,
            QueuedInputStatus::Queued | QueuedInputStatus::Consuming
        ),
        "input_id": control.input_id,
        "replayed": replayed,
        "phase": control.phase,
        "interrupted": interrupted,
        "interrupt_outcome": control.interrupt_outcome,
        "drive_status": drive_status,
        "drive_error": drive_error,
    })
}

fn map_subagent_control_store_error(error: anyhow::Error) -> RpcError {
    if error.to_string().contains("client_control_id_conflict") {
        RpcError::new(
            "client_control_id_conflict",
            "client_control_id was already used for a different subagent control",
        )
    } else {
        error.into()
    }
}

async fn record_accepted_control_drive_failure(
    state: &AppState,
    subagent_id: &str,
    reason: &str,
    error: &RpcError,
) {
    eprintln!(
        "accepted subagent control drive failed session={subagent_id} reason={reason}: {}: {}",
        error.code, error.message
    );
    match state
        .repo
        .insert_event(
            subagent_id,
            agent_store::EventType::ModelError,
            json!({ "error": error.message, "reason": reason, "accepted": true }),
        )
        .await
    {
        Ok(event) => publish_events(state, vec![event]),
        Err(event_error) => eprintln!(
            "failed to record accepted subagent control drive failure {subagent_id}: {event_error:#}"
        ),
    }
}

pub(crate) async fn interrupt_subagent_core(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: InterruptSubagentParams = from_params(params)?;
    let subagent_id = trim_required(&params.subagent_id, "subagent_id")?;
    let client_control_id = params
        .client_control_id
        .as_deref()
        .map(|id| trim_required(id, "client_control_id"))
        .transpose()?
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let scope = load_subagent_scope(state, &subagent_id, parent_session_id).await?;
    let client_input_id = format!("subagent-control:{}:{}", scope.id, client_control_id);
    if let Some(control) = state
        .repo
        .get_scoped_subagent_interrupt(&subagent_id, &client_input_id, parent_session_id, &scope.id)
        .await
        .map_err(map_subagent_control_store_error)?
    {
        if control.delegation_running
            && matches!(
                control.phase,
                SubagentControlPhase::PendingInterrupt | SubagentControlPhase::InterruptApplied
            )
            && matches!(
                control.status,
                QueuedInputStatus::Queued | QueuedInputStatus::Consuming
            )
        {
            crate::spawn_try_drive_until_blocked(
                state,
                subagent_id.clone(),
                "interrupt_subagent.replay",
            );
        }
        return Ok(subagent_control_result(&subagent_id, &control, true, None));
    }
    if scope.status != DelegationStatus::Running {
        return Err(RpcError::new(
            "delegation_not_running",
            "cannot interrupt a subagent whose delegation is terminal",
        ));
    }

    let driver = SessionDriver::acquire(state, &subagent_id).await;
    driver.reconcile_pending_subagent_controls().await?;
    driver.recover_if_needed().await?;
    ensure_subagent_steer_allowed(state, &subagent_id, parent_session_id).await?;
    let queued = state
        .repo
        .enqueue_scoped_subagent_interrupt(
            parent_session_id,
            &scope.id,
            &subagent_id,
            &client_input_id,
        )
        .await
        .map_err(map_subagent_control_store_error)?
        .ok_or_else(|| {
            RpcError::new(
                "delegation_not_running",
                "cannot interrupt a subagent whose delegation is terminal",
            )
        })?;
    let replayed = queued.replayed;
    // See the combined-steer path: one detached owner is installed before any
    // postcommit await, so aborting the parent tool future cannot strand this
    // accepted interrupt.
    crate::spawn_drive_until_blocked(state, subagent_id.clone(), "interrupt_subagent.accepted");
    let mut postcommit_error = None;
    let result = async {
        driver.reconcile_pending_subagent_controls().await?;
        driver.drive_until_blocked().await?;
        Ok::<(), RpcError>(())
    }
    .await;
    if let Err(error) = result {
        postcommit_error = Some(format!("{}: {}", error.code, error.message));
        record_accepted_control_drive_failure(
            state,
            &subagent_id,
            "interrupt_subagent.reconcile",
            &error,
        )
        .await;
    }
    drop(driver);
    let current =
        match reload_accepted_subagent_control(state, &subagent_id, &queued.input_id).await {
            Ok(Some(control)) => control,
            Ok(None) => {
                let error = RpcError::new(
                    "accepted_control_missing",
                    "durably accepted subagent interrupt could not be reloaded",
                );
                record_accepted_control_drive_failure(
                    state,
                    &subagent_id,
                    "interrupt_subagent.reload",
                    &error,
                )
                .await;
                return Ok(accepted_control_pending_result(
                    &subagent_id,
                    &queued,
                    replayed,
                    error.message,
                ));
            }
            Err(error) => {
                eprintln!(
                "accepted subagent interrupt status refresh failed session={subagent_id}: {error:#}"
            );
                return Ok(accepted_control_pending_result(
                    &subagent_id,
                    &queued,
                    replayed,
                    error.to_string(),
                ));
            }
        };
    Ok(subagent_control_result(
        &subagent_id,
        &current,
        replayed,
        postcommit_error,
    ))
}

async fn subagent_has_active_runtime(state: &AppState, subagent_id: &str) -> bool {
    let active = state.active.lock().await.get(subagent_id).cloned();
    let Some(active) = active else {
        return false;
    };
    let runtime = active.lock().await;
    runtime.session.is_ready_to_continue()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FanoutTask {
    role: String,
    prompt: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartFanoutParams {
    /// Present for websocket `delegation.start_readonly_fanout`, absent for the
    /// model-facing `delegate_readonly_tasks` tool.
    #[serde(rename = "parent_session_id")]
    _parent_session_id: Option<String>,
    tasks: Vec<FanoutTask>,
    workflow: Option<String>,
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegationIdParams {
    /// Present for websocket `delegation.status`/`delegation.cancel`, absent for
    /// model-facing `inspect_delegation`/`cancel_delegation`.
    #[serde(rename = "parent_session_id")]
    _parent_session_id: Option<String>,
    delegation_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SteerSubagentParams {
    /// Present for websocket `delegation.steer_subagent`, absent for the
    /// model-facing `steer_subagent` tool.
    #[serde(rename = "parent_session_id")]
    _parent_session_id: Option<String>,
    subagent_id: String,
    message: String,
    interrupt: Option<bool>,
    client_control_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InterruptSubagentParams {
    subagent_id: String,
    client_control_id: Option<String>,
}

fn trim_required(value: &str, field: &str) -> std::result::Result<String, RpcError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(RpcError::new(
            "invalid_params",
            format!("{field} cannot be empty"),
        ));
    }
    Ok(trimmed.to_string())
}

/// Non-recursive invariant: only the top-level session orchestrates
/// delegations. A subagent (full or read-only) must never spawn its own
/// delegation.
async fn reject_if_subagent(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<(), RpcError> {
    if state
        .repo
        .session_subagent_type(session_id)
        .await?
        .is_some()
    {
        return Err(RpcError::new(
            "delegations_not_allowed_for_subagent",
            "only the top-level session can run delegations; subagents cannot spawn subagents",
        ));
    }
    Ok(())
}

/// Spawn-failure compensation: move a half-created delegation to a terminal
/// status and interrupt any subagents already spawned for it so the parent is
/// not stranded behind the one-delegation-per-parent guard. This is deliberately
/// unconditional because spawn failure owns the just-created delegation row.
/// User-visible cancellation does NOT use this helper; it uses
/// `cancel_running_delegation` so it cannot clobber a concurrent normal
/// completion.
async fn terminate_delegation(state: &AppState, delegation_id: &str, status: DelegationStatus) {
    if let Err(error) = state
        .repo
        .set_delegation_status(delegation_id, status)
        .await
    {
        eprintln!("failed to set delegation {delegation_id} to a terminal status: {error:#}");
    }
    match state.repo.list_delegation_subagents(delegation_id).await {
        Ok(subagents) => {
            for subagent in &subagents {
                if let Err(error) = cancel_subagent_without_reactivation(
                    state,
                    &subagent.session_id,
                    subagent.subagent_type,
                )
                .await
                {
                    eprintln!(
                        "failed to cancel subagent {} while terminating delegation {}: {}: {}",
                        subagent.session_id, delegation_id, error.code, error.message
                    );
                }
            }
        }
        Err(error) => {
            eprintln!(
                "failed to list subagents while terminating delegation {delegation_id}: {error:#}"
            )
        }
    }
}

fn progress_from_subagent_overview(
    delegation: &Delegation,
    subagents: &[DelegationSubagentOverview],
) -> DelegationProgress {
    let spawned = subagents.len() as i32;
    let terminal = subagents
        .iter()
        .filter(|subagent| subagent.terminal_status.is_some())
        .count() as i32;
    let failed = subagents
        .iter()
        .filter(|subagent| subagent.terminal_status.as_deref() == Some("failed"))
        .count() as i32;
    let missing = delegation.expected_subagents.saturating_sub(spawned).max(0);
    let running = match delegation.status {
        DelegationStatus::Running => spawned.saturating_sub(terminal) + missing,
        _ => 0,
    };
    DelegationProgress {
        expected: delegation.expected_subagents,
        spawned,
        terminal,
        running,
        failed,
    }
}

async fn cancel_subagent_without_reactivation(
    state: &AppState,
    session_id: &str,
    subagent_type: Option<SubagentType>,
) -> std::result::Result<(), RpcError> {
    abort_session_tasks(state, session_id);
    let driver = SessionDriver::acquire(state, session_id).await;
    if let Some(active) = driver.active_session().await {
        // Persist an interrupted turn boundary if the subagent has live runtime
        // state, but deliberately do not drive afterwards: queued inputs for a
        // cancelled delegation must not reactivate the subagent.
        let _dispatches = driver
            .apply_agent_input(active, AgentInput::Interrupt, None)
            .await?;
    } else {
        let events = state
            .repo
            .cancel_unfinished_session_work(session_id, "delegation cancelled")
            .await?;
        if !events.is_empty() {
            publish_events(state, events);
        }
    }
    state.active.lock().await.remove(session_id);
    if subagent_type == Some(SubagentType::ReadOnly) {
        if let Err(error) = state
            .runtime_hosts
            .destroy_session_workspaces(session_id)
            .await
        {
            eprintln!("failed to destroy read-only subagent workspace {session_id}: {error:#}");
        }
    }
    Ok(())
}

/// Start the single full (writing) subagent of a delegation. Homogeneity and the
/// single-full invariant are structural: the schema accepts exactly one scalar
/// role/prompt, so no caller can mix kinds or request a second writer.
pub(crate) async fn start_full_core(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: StartFullParams = from_params(params)?;
    let role = trim_required(&params.role, "role")?;
    let prompt = trim_required(&params.prompt, "prompt")?;

    reject_if_subagent(state, parent_session_id).await?;

    let delegation = state
        .repo
        .create_delegation(
            parent_session_id,
            DelegationKind::Full,
            params.workflow.as_deref(),
            params.label.as_deref(),
            1,
        )
        .await
        .map_err(map_delegation_create_error)?;

    let spawned = match spawn_subagent(
        state,
        DelegationSubagentSpawn {
            parent_session_id: parent_session_id.to_string(),
            role,
            task: prompt,
            subagent_type: SubagentType::Full,
            delegation_id: delegation.id.clone(),
        },
    )
    .await
    {
        Ok(spawned) => spawned,
        Err(error) => {
            // The delegation row already exists; fail it so the
            // one-delegation-per-parent guard releases rather than blocking the
            // parent forever.
            terminate_delegation(state, &delegation.id, DelegationStatus::Failed).await;
            return Err(error);
        }
    };

    Ok(json!({
        "delegation_id": delegation.id,
        "subagent_session_id": spawned.started.session_id,
    }))
}

/// Start N read-only subagents in parallel, one per task, each in its own
/// disposable snapshot. Homogeneity is structural: every task is forced to
/// `read_only`, so a fan-out can never contain a writer.
pub(crate) async fn start_readonly_fanout_core(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: StartFanoutParams = from_params(params)?;
    if params.tasks.is_empty() {
        return Err(RpcError::new("invalid_params", "tasks cannot be empty"));
    }
    let mut tasks = Vec::with_capacity(params.tasks.len());
    for task in &params.tasks {
        tasks.push((
            trim_required(&task.role, "role")?,
            trim_required(&task.prompt, "prompt")?,
        ));
    }

    reject_if_subagent(state, parent_session_id).await?;

    let expected_subagents = tasks.len();
    let delegation = state
        .repo
        .create_delegation(
            parent_session_id,
            DelegationKind::ReadonlyFanout,
            params.workflow.as_deref(),
            params.label.as_deref(),
            expected_subagents as i32,
        )
        .await
        .map_err(map_delegation_create_error)?;

    let mut subagent_session_ids = Vec::with_capacity(expected_subagents);
    for (role, prompt) in tasks {
        match spawn_subagent(
            state,
            DelegationSubagentSpawn {
                parent_session_id: parent_session_id.to_string(),
                role,
                task: prompt,
                subagent_type: SubagentType::ReadOnly,
                delegation_id: delegation.id.clone(),
            },
        )
        .await
        {
            Ok(spawned) => subagent_session_ids.push(spawned.started.session_id),
            Err(error) => {
                // Tear down the subagents already spawned for this delegation
                // and fail it, so a partial fan-out never leaves running
                // children or blocks the parent behind the
                // one-delegation-per-parent guard.
                terminate_delegation(state, &delegation.id, DelegationStatus::Failed).await;
                return Err(error);
            }
        }
    }

    Ok(json!({
        "delegation_id": delegation.id,
        "subagent_session_ids": subagent_session_ids,
    }))
}

async fn load_delegation_for_parent(
    state: &AppState,
    parent_session_id: &str,
    delegation_id: &str,
) -> std::result::Result<Delegation, RpcError> {
    let delegation = state
        .repo
        .get_delegation(delegation_id)
        .await?
        .ok_or_else(|| RpcError::new("delegation_not_found", "delegation not found"))?;
    if delegation.parent_session_id != parent_session_id {
        return Err(RpcError::new(
            "delegation_not_found",
            "delegation is not in scope",
        ));
    }
    Ok(delegation)
}

pub(crate) async fn status_core(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: DelegationIdParams = from_params(params)?;
    let delegation =
        load_delegation_for_parent(state, parent_session_id, &params.delegation_id).await?;
    build_delegation_snapshot(state, &delegation).await
}

/// Cancel an in-flight delegation. Cancellation first wins an attempt-fenced
/// `running -> cancelled` CAS; only the CAS winner interrupts subagents and
/// writes transcript-only artifacts. If completion or another cancellation wins
/// first, this returns `{ "cancelled": false }` and leaves existing artifacts
/// untouched. Interrupting a read-only subagent is allowed here because the
/// whole delegation is being torn down; per-subagent steering is allowed for
/// running RO subagents through `steer_subagent_core`.
pub(crate) async fn cancel_core(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: DelegationIdParams = from_params(params)?;
    let delegation =
        load_delegation_for_parent(state, parent_session_id, &params.delegation_id).await?;
    // Only an in-flight delegation can be cancelled; a terminal delegation keeps
    // its status (never clobber a done/failed delegation or report a false
    // cancel).
    if delegation.status != DelegationStatus::Running {
        return Ok(json!({ "cancelled": false }));
    }
    let (won_cancel, events) = state
        .repo
        .cancel_running_delegation_and_queued_partials(
            &delegation.parent_session_id,
            &delegation.id,
            &delegation.attempt_id,
            "delegation_cancelled",
        )
        .await?;
    if !won_cancel {
        return Ok(json!({ "cancelled": false }));
    }
    publish_events(state, events);
    cancel_delegation_subagents_without_reactivation(state, &delegation.id).await;
    let (handoff_dir, subagents) = write_cancelled_subagent_transcripts(state, &delegation).await?;
    Ok(json!({
        "cancelled": true,
        "delegation_id": delegation.id,
        "handoff_dir": handoff_dir,
        "subagents": subagents,
    }))
}

async fn cancel_delegation_subagents_without_reactivation(state: &AppState, delegation_id: &str) {
    match state.repo.list_delegation_subagents(delegation_id).await {
        Ok(subagents) => {
            for subagent in &subagents {
                if let Err(error) = cancel_subagent_without_reactivation(
                    state,
                    &subagent.session_id,
                    subagent.subagent_type,
                )
                .await
                {
                    eprintln!(
                        "failed to cancel subagent {} while cancelling delegation {}: {}: {}",
                        subagent.session_id, delegation_id, error.code, error.message
                    );
                }
            }
        }
        Err(error) => {
            eprintln!(
                "failed to list subagents while cancelling delegation {delegation_id}: {error:#}"
            )
        }
    }
}

async fn write_cancelled_subagent_transcripts(
    state: &AppState,
    delegation: &Delegation,
) -> std::result::Result<(String, Vec<Value>), RpcError> {
    let parent_config = state
        .repo
        .load_session_config(&delegation.parent_session_id)
        .await?;
    let delegation_segment = safe_path_segment(&delegation.id, "delegation_id")?;
    let handoff_dir = delegation_dir(&parent_config.workspace_id, &delegation_segment);
    let dir = handoff_dir.join("cancelled");
    let subagents = state.repo.list_delegation_subagents(&delegation.id).await?;
    let mut transcript_refs = Vec::with_capacity(subagents.len());
    for subagent in &subagents {
        let subagent_segment = safe_path_segment(&subagent.session_id, "subagent_id")?;
        let history = state.repo.active_branch(&subagent.session_id).await?;
        let transcript = render_transcript_markdown(&history);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(anyhow::Error::from)?;
        let transcript_file = format!("cancelled/{subagent_segment}.transcript.md");
        let path = dir.join(format!("{subagent_segment}.transcript.md"));
        tokio::fs::write(&path, transcript.as_bytes())
            .await
            .map_err(anyhow::Error::from)?;
        transcript_refs.push(json!({
            "subagent_id": subagent.session_id,
            "transcript_file": transcript_file,
        }));
    }
    Ok((handoff_dir.to_string_lossy().into_owned(), transcript_refs))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadHandoffFileParams {
    #[serde(rename = "parent_session_id")]
    _parent_session_id: Option<String>,
    delegation_id: String,
    subagent_id: Option<String>,
    file: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum HandoffFileRequest<'a> {
    Normal { subagent_id: &'a str, file: &'a str },
    CancelledTranscript { subagent_id: &'a str },
}

impl HandoffFileRequest<'_> {
    fn subagent_id(&self) -> &str {
        match self {
            Self::Normal { subagent_id, .. } | Self::CancelledTranscript { subagent_id } => {
                subagent_id
            }
        }
    }
}

/// Resolve the closed handoff file vocabulary. Normal files live under
/// `<subagent_id>/{task_prompt.md,final_message.md,transcript.md}`. Cancelled
/// delegations expose the transcript-only cancellation artifact via
/// `cancelled/<subagent_id>.transcript.md`; a previously published terminal-child
/// `final_message.md` may also remain readable after cancellation, but normal
/// transcripts are not exposed.
fn parse_handoff_file_request<'a>(
    subagent_id: Option<&'a str>,
    file: &'a str,
) -> std::result::Result<HandoffFileRequest<'a>, RpcError> {
    match file {
        TASK_PROMPT_FILE | "final_message.md" | "transcript.md" => {
            let subagent_id = subagent_id.ok_or_else(|| {
                RpcError::new("invalid_params", format!("{file} requires a subagent_id"))
            })?;
            Ok(HandoffFileRequest::Normal { subagent_id, file })
        }
        relative => {
            if let Some(rest) = relative.strip_prefix("cancelled/") {
                if let Some(relative_subagent_id) = rest.strip_suffix(".transcript.md") {
                    safe_path_segment(relative_subagent_id, "subagent_id")?;
                    if subagent_id.is_some_and(|id| id != relative_subagent_id) {
                        return Err(RpcError::new(
                            "invalid_params",
                            "subagent_id does not match cancellation transcript path",
                        ));
                    }
                    return Ok(HandoffFileRequest::CancelledTranscript {
                        subagent_id: relative_subagent_id,
                    });
                }
            }
            Err(RpcError::new(
                "invalid_params",
                format!(
                    "file must be one of task_prompt.md | final_message.md | transcript.md | cancelled/<subagent_id>.transcript.md, got {relative}"
                ),
            ))
        }
    }
}

fn read_allowed_for_status(
    status: DelegationStatus,
    request: HandoffFileRequest<'_>,
) -> std::result::Result<Option<bool>, RpcError> {
    match status {
        DelegationStatus::Running => match request {
            HandoffFileRequest::Normal {
                file: TASK_PROMPT_FILE,
                ..
            } => Ok(Some(true)),
            HandoffFileRequest::Normal {
                file: "transcript.md",
                ..
            } => Ok(Some(true)),
            HandoffFileRequest::Normal {
                file: "final_message.md",
                ..
            } => Ok(None),
            HandoffFileRequest::Normal { file, .. } => Err(RpcError::new(
                "invalid_params",
                format!("unsupported handoff file {file}"),
            )),
            HandoffFileRequest::CancelledTranscript { .. } => Ok(Some(false)),
        },
        DelegationStatus::Done | DelegationStatus::DoneWithFailures => match request {
            HandoffFileRequest::Normal { .. } => Ok(Some(true)),
            HandoffFileRequest::CancelledTranscript { .. } => Ok(Some(false)),
        },
        DelegationStatus::Cancelled => match request {
            HandoffFileRequest::Normal {
                file: TASK_PROMPT_FILE,
                ..
            } => Ok(Some(true)),
            HandoffFileRequest::CancelledTranscript { .. } => Ok(Some(true)),
            HandoffFileRequest::Normal {
                file: "final_message.md",
                ..
            } => Ok(Some(true)),
            HandoffFileRequest::Normal {
                file: "transcript.md",
                ..
            } => Ok(Some(false)),
            HandoffFileRequest::Normal { .. } => Ok(Some(false)),
        },
        DelegationStatus::Failed => match request {
            HandoffFileRequest::Normal {
                file: TASK_PROMPT_FILE,
                ..
            } => Ok(Some(true)),
            _ => Ok(Some(false)),
        },
    }
}

async fn read_allowed_for_request(
    state: &AppState,
    delegation: &Delegation,
    request: HandoffFileRequest<'_>,
) -> std::result::Result<bool, RpcError> {
    match read_allowed_for_status(delegation.status, request)? {
        Some(allowed) => Ok(allowed),
        None => {
            let work_state = load_subagent_work_state(state, request.subagent_id()).await?;
            Ok(work_state.is_completion_terminal())
        }
    }
}

fn unavailable_handoff_file_error(status: DelegationStatus) -> RpcError {
    match status {
        DelegationStatus::Running => RpcError::new(
            "handoff_file_not_found",
            "handoff file not found; the delegation may not have finished yet",
        ),
        DelegationStatus::Cancelled => RpcError::new(
            "handoff_file_not_found",
            "normal handoff files are not published for cancelled delegations",
        ),
        DelegationStatus::Failed => RpcError::new(
            "handoff_file_not_found",
            "handoff files are not published for failed delegations",
        ),
        DelegationStatus::Done | DelegationStatus::DoneWithFailures => {
            RpcError::new("handoff_file_not_found", "handoff file not found")
        }
    }
}

fn validate_member_subagent(
    request: HandoffFileRequest<'_>,
    members: &[DelegationSubagent],
) -> std::result::Result<(), RpcError> {
    let subagent_id = match request {
        HandoffFileRequest::Normal { subagent_id, .. }
        | HandoffFileRequest::CancelledTranscript { subagent_id } => subagent_id,
    };
    if members
        .iter()
        .any(|member| member.session_id == subagent_id)
    {
        Ok(())
    } else {
        Err(RpcError::new(
            "handoff_file_not_found",
            "subagent does not belong to this delegation",
        ))
    }
}

/// Reject any path segment that is not a plain file/dir name. A single
/// `Component::Normal` with no separators, no `.`/`..`, and no NUL is the only
/// thing that can ever escape the handoff subtree, so we validate the segment
/// in isolation before it is ever joined onto the trusted base.
fn safe_path_segment(segment: &str, field: &str) -> std::result::Result<String, RpcError> {
    safe_handoff_path_segment(segment, field)
}

/// Resolve a handoff file request to an absolute path strictly under
/// `<parent_workspace_id>/.pi-handoff/<delegation_id>/`. The request was already
/// parsed into a closed vocabulary; every dynamic segment (`delegation_id` and
/// `subagent_id`) is validated as a single safe path component, so the result
/// can never traverse out of the handoff subtree.
fn resolve_handoff_file_path(
    parent_workspace_id: &str,
    delegation_id: &str,
    request: HandoffFileRequest<'_>,
) -> std::result::Result<PathBuf, RpcError> {
    let delegation_segment = safe_path_segment(delegation_id, "delegation_id")?;
    let mut path = delegation_dir(parent_workspace_id, &delegation_segment);
    match request {
        HandoffFileRequest::Normal { subagent_id, file } => {
            path.push(safe_path_segment(subagent_id, "subagent_id")?);
            // `file` is already constrained to the known literals above.
            path.push(file);
        }
        HandoffFileRequest::CancelledTranscript { subagent_id } => {
            path.push("cancelled");
            path.push(format!(
                "{}.transcript.md",
                safe_path_segment(subagent_id, "subagent_id")?
            ));
        }
    }
    Ok(path)
}

/// Read one handoff file for product inspection. The web client cannot read
/// host files directly; this is the only path through which it reaches the
/// handoff subtree, and it is scoped to the parent (the delegation must belong
/// to it, exactly like `delegation.status`) and traversal-safe (every segment
/// is validated).
pub(crate) async fn read_handoff_file_core(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: ReadHandoffFileParams = from_params(params)?;
    let delegation =
        load_delegation_for_parent(state, parent_session_id, &params.delegation_id).await?;
    let request = parse_handoff_file_request(params.subagent_id.as_deref(), &params.file)?;
    safe_path_segment(&delegation.id, "delegation_id")?;
    safe_path_segment(request.subagent_id(), "subagent_id")?;
    // A read may only target a subagent that belongs to this delegation;
    // otherwise a caller could probe arbitrary `<delegation>/<segment>/` paths.
    let members = state.repo.list_delegation_subagents(&delegation.id).await?;
    validate_member_subagent(request, &members)?;

    if read_allowed_for_request(state, &delegation, request).await? {
        if matches!(
            request,
            HandoffFileRequest::Normal {
                file: TASK_PROMPT_FILE,
                ..
            }
        ) {
            let parent_config = state.repo.load_session_config(parent_session_id).await?;
            let dir = delegation_dir(&parent_config.workspace_id, &delegation.id);
            let subagent_id = request.subagent_id();
            let member = members
                .iter()
                .find(|member| member.session_id == subagent_id)
                .ok_or_else(|| {
                    RpcError::new(
                        "handoff_file_not_found",
                        "subagent does not belong to this delegation",
                    )
                })?;
            let task_prompt =
                refresh_task_prompt_artifact_if_present(&dir, subagent_id, member.task.as_deref())
                    .await?;
            if task_prompt.is_none() {
                return Err(RpcError::new(
                    "handoff_file_not_found",
                    "task prompt is unavailable for this subagent",
                ));
            }
        } else {
            match delegation.status {
                DelegationStatus::Running
                | DelegationStatus::Done
                | DelegationStatus::DoneWithFailures => {
                    let include_final_messages = matches!(
                        delegation.status,
                        DelegationStatus::Running
                            | DelegationStatus::Done
                            | DelegationStatus::DoneWithFailures
                    );
                    refresh_delegation_handoff_artifacts(
                        state,
                        &delegation,
                        include_final_messages,
                    )
                    .await?;
                }
                // Cancelled reads are limited to the explicit transcript-only
                // cancellation artifacts plus already-written terminal-child
                // final_message.md files. Do not refresh or expose normal
                // transcript.md files here: running snapshots may have written
                // stale normal transcripts before cancellation won.
                DelegationStatus::Cancelled | DelegationStatus::Failed => {}
            }
        }
    } else {
        return Err(unavailable_handoff_file_error(delegation.status));
    }
    let parent_config = state.repo.load_session_config(parent_session_id).await?;
    let path = resolve_handoff_file_path(&parent_config.workspace_id, &delegation.id, request)?;
    // Defense in depth: confine the symlink-resolved target under the parent's
    // handoff dir so a symlink planted inside it cannot escape to an arbitrary
    // host file. Segment validation above already blocks `..`/abs paths.
    let handoff_root_path = handoff_root(&parent_config.workspace_id);
    let canonical = match tokio::fs::canonicalize(&path).await {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(RpcError::new(
                "handoff_file_not_found",
                "handoff file not found; the delegation may not have finished yet",
            ))
        }
        Err(error) => {
            // Never leak the absolute host handoff path to the client.
            eprintln!(
                "failed to resolve handoff file {}: {error:#}",
                path.display()
            );
            return Err(RpcError::new(
                "handoff_file_read_failed",
                "failed to read handoff file",
            ));
        }
    };
    let canonical_root = match tokio::fs::canonicalize(&handoff_root_path).await {
        Ok(root) => root,
        Err(error) => {
            eprintln!(
                "failed to resolve handoff root {}: {error:#}",
                handoff_root_path.display()
            );
            return Err(RpcError::new(
                "handoff_file_read_failed",
                "failed to read handoff file",
            ));
        }
    };
    if !canonical.starts_with(&canonical_root) {
        return Err(RpcError::new(
            "invalid_params",
            "resolved handoff path escapes the handoff directory",
        ));
    }
    let content = match tokio::fs::read_to_string(&canonical).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(RpcError::new(
                "handoff_file_not_found",
                "handoff file not found; the delegation may not have finished yet",
            ))
        }
        Err(error) => {
            // Never leak the absolute host handoff path to the client.
            eprintln!(
                "failed to read handoff file {}: {error:#}",
                canonical.display()
            );
            return Err(RpcError::new(
                "handoff_file_read_failed",
                "failed to read handoff file",
            ));
        }
    };
    Ok(json!({
        "delegation_id": delegation.id,
        "subagent_id": request.subagent_id(),
        "file": params.file.clone(),
        "content": content,
    }))
}

pub(crate) async fn rpc_read_handoff_file(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let parent_session_id = parent_session_id_from_params(&params)?;
    read_handoff_file_core(state, &parent_session_id, params).await
}

fn parent_session_id_from_params(params: &Value) -> std::result::Result<String, RpcError> {
    let parent_session_id = params
        .get("parent_session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if parent_session_id.is_empty() {
        return Err(RpcError::new(
            "invalid_params",
            "parent_session_id cannot be empty",
        ));
    }
    Ok(parent_session_id.to_string())
}

pub(crate) async fn rpc_start_full(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let parent_session_id = parent_session_id_from_params(&params)?;
    start_full_core(state, &parent_session_id, params).await
}

pub(crate) async fn rpc_start_readonly_fanout(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let parent_session_id = parent_session_id_from_params(&params)?;
    start_readonly_fanout_core(state, &parent_session_id, params).await
}

pub(crate) async fn rpc_status(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let parent_session_id = parent_session_id_from_params(&params)?;
    status_core(state, &parent_session_id, params).await
}

pub(crate) async fn rpc_cancel(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let parent_session_id = parent_session_id_from_params(&params)?;
    cancel_core(state, &parent_session_id, params).await
}

pub(crate) async fn rpc_steer_subagent(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let parent_session_id = parent_session_id_from_params(&params)?;
    steer_subagent_core(state, &parent_session_id, params).await
}

const DEFAULT_DELEGATION_LIST_LIMIT: i64 = 3;
const MAX_DELEGATION_LIST_LIMIT: i64 = 100;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegationListParams {
    parent_session_id: String,
    limit: Option<i64>,
}

fn bounded_delegation_list_limit(limit: Option<i64>) -> std::result::Result<i64, RpcError> {
    let limit = limit.unwrap_or(DEFAULT_DELEGATION_LIST_LIMIT);
    if !(0..=MAX_DELEGATION_LIST_LIMIT).contains(&limit) {
        return Err(RpcError::new(
            "invalid_params",
            format!("limit must be between 0 and {MAX_DELEGATION_LIST_LIMIT}"),
        ));
    }
    Ok(limit)
}

fn list_subagent_status(
    delegation_status: DelegationStatus,
    subagent: &DelegationSubagentOverview,
) -> String {
    match delegation_status {
        DelegationStatus::Running => {
            if let Some(terminal_status) = &subagent.terminal_status {
                terminal_status.clone()
            } else if subagent.activity != agent_store::SessionActivity::Idle {
                subagent.activity.to_string()
            } else {
                "running".to_string()
            }
        }
        DelegationStatus::Done | DelegationStatus::DoneWithFailures => subagent
            .terminal_status
            .clone()
            .unwrap_or_else(|| delegation_status.as_str().to_string()),
        DelegationStatus::Cancelled | DelegationStatus::Failed => {
            delegation_status.as_str().to_string()
        }
    }
}

/// Per-parent delegation list for the product Agents outline: a bounded
/// newest-first page with compact subagent rows. (The model tool surface has no list.)
pub(crate) async fn rpc_list(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: DelegationListParams = from_params(params)?;
    let parent_session_id = params.parent_session_id.trim();
    if parent_session_id.is_empty() {
        return Err(RpcError::new(
            "invalid_params",
            "parent_session_id cannot be empty",
        ));
    }
    let limit = bounded_delegation_list_limit(params.limit)?;
    if !state.repo.session_exists(parent_session_id).await? {
        return Err(RpcError::new(
            "internal_error",
            format!("session not found: {parent_session_id}"),
        ));
    }
    let mut delegations = state
        .repo
        .list_parent_delegations_newest(parent_session_id, limit.saturating_add(1))
        .await?;
    let has_more = delegations.len() > limit as usize;
    delegations.truncate(limit as usize);
    let mut views = Vec::with_capacity(delegations.len());
    for delegation in &delegations {
        let subagent_rows = state
            .repo
            .delegation_subagent_overview(&delegation.id)
            .await?;
        let progress = progress_from_subagent_overview(delegation, &subagent_rows);
        let mut subagents = Vec::with_capacity(subagent_rows.len());
        for subagent in subagent_rows {
            safe_path_segment(&subagent.session_id, "subagent_id")?;
            let task_prompt_file = subagent
                .has_task
                .then(|| task_prompt_rel(&subagent.session_id));
            let status = list_subagent_status(delegation.status, &subagent);
            let has_active_work = subagent.activity != agent_store::SessionActivity::Idle
                || subagent_has_active_runtime(state, &subagent.session_id).await;
            let steerable = delegation.status == DelegationStatus::Running
                && subagent.subagent_type.is_some()
                && subagent.terminal_status.is_none()
                && has_active_work;
            subagents.push(json!({
                "id": subagent.session_id,
                "status": status,
                "activity": subagent.activity,
                "role": subagent.role,
                "title": subagent.title,
                "type": subagent.subagent_type,
                "subagent_type": subagent.subagent_type,
                "task_prompt_file": task_prompt_file,
                "steerable": steerable,
                "outcome": serde_json::Value::Null,
                "final_message_file": serde_json::Value::Null,
                "transcript_file": serde_json::Value::Null,
            }));
        }
        views.push(json!({
            "delegation_id": delegation.id,
            "kind": delegation.kind,
            "status": delegation.status,
            "workflow": delegation.workflow,
            "label": delegation.label,
            "progress": progress_view(progress),
            "subagents": subagents,
        }));
    }
    Ok(json!({
        "parent_session_id": parent_session_id,
        "limit": limit,
        "has_more": has_more,
        "delegations": views,
    }))
}

pub(crate) fn is_delegation_tool_name(name: &str) -> bool {
    matches!(
        name,
        "delegate_writing_task"
            | "delegate_readonly_tasks"
            | "inspect_delegation"
            | "cancel_delegation"
            | "steer_subagent"
            | "interrupt_subagent"
    )
}

/// Model-facing dispatch: run the core fn for the named delegation tool and
/// wrap the result as a tool result message. The session id is the parent's.
pub(crate) async fn run_delegation_tool(
    state: &AppState,
    parent_session_id: &str,
    call: &ToolCall,
) -> ToolResultMessage {
    if let Err(error) = reject_if_subagent(state, parent_session_id).await {
        return ToolResultMessage::error(
            call.id.clone(),
            &call.tool_name,
            format!("{}: {}", error.code, error.message),
        );
    }
    let mut params: Value = match serde_json::from_str(&call.args_json) {
        Ok(params) => params,
        Err(error) => {
            return ToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                format!("{} arguments were invalid JSON: {error}", call.tool_name),
            )
        }
    };
    if matches!(
        call.tool_name.as_str(),
        "steer_subagent" | "interrupt_subagent"
    ) {
        let Some(object) = params.as_object_mut() else {
            return ToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                format!("{} arguments must be a JSON object", call.tool_name),
            );
        };
        // Provider/runtime retries preserve the tool-call id. Always replace
        // hidden/provider-supplied values at this runtime trust boundary.
        object.insert(
            "client_control_id".to_string(),
            json!(format!("tool-call:{}", call.id.0)),
        );
    }
    let result = match call.tool_name.as_str() {
        "delegate_writing_task" => start_full_core(state, parent_session_id, params).await,
        "delegate_readonly_tasks" => {
            start_readonly_fanout_core(state, parent_session_id, params).await
        }
        "inspect_delegation" => status_core(state, parent_session_id, params).await,
        "cancel_delegation" => cancel_core(state, parent_session_id, params).await,
        "steer_subagent" => steer_subagent_core(state, parent_session_id, params).await,
        "interrupt_subagent" => interrupt_subagent_core(state, parent_session_id, params).await,
        other => Err(RpcError::new(
            "unknown_tool",
            format!("unknown delegation tool: {other}"),
        )),
    };
    match result {
        Ok(value) => ToolResultMessage::success(
            call.id.clone(),
            &call.tool_name,
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
        ),
        Err(error) => ToolResultMessage::error(
            call.id.clone(),
            &call.tool_name,
            format!("{}: {}", error.code, error.message),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const CWD: &str = "/home/u/.local/state/pi-relay/sessions/parent/cwd";

    #[test]
    fn delegation_tool_interception_accepts_only_canonical_names() {
        for name in [
            "delegate_writing_task",
            "delegate_readonly_tasks",
            "inspect_delegation",
            "cancel_delegation",
            "steer_subagent",
            "interrupt_subagent",
        ] {
            assert!(
                is_delegation_tool_name(name),
                "{name} should be intercepted"
            );
        }
        for old in [
            "stage_start_full",
            "stage_start_readonly_fanout",
            "stage_status",
            "stage_cancel",
        ] {
            assert!(
                !is_delegation_tool_name(old),
                "{old} must not be intercepted"
            );
        }
        assert!(!is_delegation_tool_name("delegation.list"));
        assert!(!is_delegation_tool_name("delegation.start_full"));
    }

    #[test]
    fn combined_control_result_reports_durable_interrupt_phase_truthfully() {
        let pending = SubagentControlRecord {
            input_id: "input-1".to_string(),
            status: QueuedInputStatus::Queued,
            kind: agent_store::SubagentControlKind::Steer,
            phase: SubagentControlPhase::PendingInterrupt,
            interrupt: true,
            interrupted: false,
            interrupt_outcome: None,
            target_active_leaf_id: Some("old-leaf".to_string()),
            target_turn_id: Some(7),
            target_action_attempt_ids: vec!["attempt-1".to_string()],
            delegation_running: true,
        };
        let result = subagent_control_result("child", &pending, false, None);
        assert_eq!(result["accepted"], true);
        assert_eq!(result["phase"], "pending_interrupt");
        assert_eq!(result["interrupted"], Value::Null);
        assert_eq!(result["drive_status"], "pending");

        let applied = SubagentControlRecord {
            phase: SubagentControlPhase::InterruptApplied,
            interrupted: true,
            interrupt_outcome: Some("interrupted".to_string()),
            ..pending
        };
        let result = subagent_control_result(
            "child",
            &applied,
            false,
            Some("postcommit drive failed".to_string()),
        );
        assert_eq!(result["accepted"], true);
        assert_eq!(result["phase"], "interrupt_applied");
        assert_eq!(result["interrupted"], true);
        assert_eq!(result["interrupt_outcome"], "interrupted");
        assert_eq!(result["drive_status"], "failed");
    }

    #[test]
    fn stale_stage_id_parameter_is_rejected_as_hard_rename_regression_guard() {
        let error: RpcError = from_params::<DelegationIdParams>(json!({
            "stage_id": "delegation-1",
        }))
        .unwrap_err();
        assert_eq!(error.code, "invalid_params");

        let error: RpcError = from_params::<DelegationIdParams>(json!({
            "parent_session_id": "parent",
            "delegation_id": "delegation-1",
            "stage_id": "delegation-1",
        }))
        .unwrap_err();
        assert_eq!(error.code, "invalid_params");
    }

    #[test]
    fn resolves_subagent_file_under_subagent_dir() {
        let path = resolve_handoff_file_path(
            CWD,
            "delegation-1",
            HandoffFileRequest::Normal {
                subagent_id: "child-9",
                file: "final_message.md",
            },
        )
        .unwrap();
        assert_eq!(
            path,
            Path::new(CWD)
                .join(".pi-handoff")
                .join("delegation-1")
                .join("child-9")
                .join("final_message.md")
        );
    }

    #[test]
    fn resolves_cancelled_transcript_under_cancelled_dir() {
        let request = parse_handoff_file_request(None, "cancelled/child-9.transcript.md").unwrap();
        let path = resolve_handoff_file_path(CWD, "delegation-1", request).unwrap();
        assert_eq!(
            path,
            Path::new(CWD)
                .join(".pi-handoff")
                .join("delegation-1")
                .join("cancelled")
                .join("child-9.transcript.md")
        );
    }

    #[test]
    fn rejects_unknown_file_name() {
        let error = parse_handoff_file_request(None, "secrets.env").unwrap_err();
        assert_eq!(error.code, "invalid_params");
    }

    #[test]
    fn rejects_index_json_because_snapshots_replaced_root_manifests() {
        let error = parse_handoff_file_request(None, "index.json").unwrap_err();
        assert_eq!(error.code, "invalid_params");
    }

    #[test]
    fn rejects_traversal_in_delegation_id() {
        for evil in ["..", "../other", "a/b", "/etc", "delegation/../..", "."] {
            let error = resolve_handoff_file_path(
                CWD,
                evil,
                HandoffFileRequest::Normal {
                    subagent_id: "child",
                    file: "transcript.md",
                },
            )
            .unwrap_err();
            assert_eq!(
                error.code, "invalid_params",
                "delegation_id {evil} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_traversal_in_subagent_id() {
        for evil in ["..", "../x", "a/b", "/abs"] {
            let error = resolve_handoff_file_path(
                CWD,
                "delegation-1",
                HandoffFileRequest::Normal {
                    subagent_id: evil,
                    file: "transcript.md",
                },
            )
            .unwrap_err();
            assert_eq!(
                error.code, "invalid_params",
                "subagent_id {evil} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_traversal_in_cancelled_transcript_path() {
        for evil in [
            "cancelled/../child.transcript.md",
            "cancelled/a/b.transcript.md",
            "cancelled//child.transcript.md",
        ] {
            let error = parse_handoff_file_request(None, evil).unwrap_err();
            assert_eq!(
                error.code, "invalid_params",
                "cancelled path {evil} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_mismatched_subagent_id_for_cancelled_transcript_path() {
        let error =
            parse_handoff_file_request(Some("other-child"), "cancelled/child-9.transcript.md")
                .unwrap_err();
        assert_eq!(error.code, "invalid_params");
    }

    #[test]
    fn requires_subagent_id_for_subagent_files() {
        let error = parse_handoff_file_request(None, "transcript.md").unwrap_err();
        assert_eq!(error.code, "invalid_params");
    }
}
