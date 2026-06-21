use std::path::{Component, Path, PathBuf};

use agent_core::AgentInput;
use agent_store::{Delegation, DelegationKind, DelegationStatus, InputPriority, SubagentType};
use agent_vocab::{ToolCall, ToolResultMessage, UserMessage};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::codec::from_params;
use crate::handoff::{
    refresh_delegation_handoff_artifacts, render_transcript_markdown, SubagentArtifact,
};
use crate::runtime::{abort_session_tasks, publish_events, SessionDriver};
use crate::state::AppState;
use crate::subagents::{spawn_subagent, DelegationSubagentSpawn};
use crate::types::RpcError;

const HANDOFF_DIR: &str = ".pi-handoff";

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

async fn subagent_delegation_for_parent(
    state: &AppState,
    parent_session_id: &str,
    subagent_id: &str,
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
    let delegation_id = state
        .repo
        .session_delegation_id(subagent_id)
        .await
        .map_err(|error| {
            eprintln!("failed to load delegation for subagent {subagent_id}: {error:#}");
            RpcError::new("subagent_not_found", "subagent is not in scope")
        })?
        .ok_or_else(|| RpcError::new("subagent_not_found", "subagent is not in scope"))?;
    load_delegation_for_parent(state, parent_session_id, &delegation_id).await
}

pub(crate) async fn steer_subagent_core(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: SteerSubagentParams = from_params(params)?;
    let subagent_id = trim_required(&params.subagent_id, "subagent_id")?;
    let message = trim_required(&params.message, "message")?;
    let delegation = subagent_delegation_for_parent(state, parent_session_id, &subagent_id).await?;
    if delegation.status != DelegationStatus::Running {
        return Err(RpcError::new(
            "delegation_not_running",
            "cannot steer a subagent whose delegation is terminal",
        ));
    }
    match state.repo.session_subagent_type(&subagent_id).await? {
        Some(SubagentType::Full) => {}
        Some(SubagentType::ReadOnly) => {
            return Err(RpcError::new(
                "cannot_steer_read_only_subagent",
                "a read-only subagent cannot be steered",
            ))
        }
        None => {
            return Err(RpcError::new(
                "subagent_not_found",
                "subagent is not in scope",
            ))
        }
    }
    // A running delegation row can briefly race a subagent reaching its terminal
    // transcript boundary before the barrier wins the delegation CAS. Do not
    // enqueue a steer in that terminal window: doing so would start a new turn
    // and reactivate a subagent that has already finished.
    if state
        .repo
        .active_leaf_is_turn_boundary(&subagent_id)
        .await?
    {
        return Err(RpcError::new(
            "subagent_terminal",
            "cannot steer a subagent that is already terminal",
        ));
    }
    let has_unfinished_actions = state.repo.has_unfinished_actions(&subagent_id).await?;
    let has_queued_inputs = state.repo.has_queued_inputs(&subagent_id).await?;
    let has_active_runtime = subagent_has_active_runtime(state, &subagent_id).await;
    if !has_unfinished_actions && !has_queued_inputs && !has_active_runtime {
        return Err(RpcError::new(
            "subagent_not_running",
            "cannot steer a subagent without active work or queued input",
        ));
    }

    let client_input_id = format!("subagent-steer:{}:{}", delegation.id, uuid::Uuid::new_v4());
    let queued = state
        .repo
        .enqueue_user_input(
            &subagent_id,
            InputPriority::Steer,
            &UserMessage::text(message),
            Some(&client_input_id),
        )
        .await?;
    if let Some(event) = queued.event {
        publish_events(state, vec![event]);
    }
    if !has_unfinished_actions {
        let driver = SessionDriver::acquire(state, &subagent_id).await;
        driver.drive_until_blocked().await?;
    }
    Ok(json!({
        "subagent_id": subagent_id,
        "queued": true,
        "input_id": queued.input_id,
    }))
}

async fn subagent_has_active_runtime(state: &AppState, subagent_id: &str) -> bool {
    state.active.lock().await.contains_key(subagent_id)
}

fn delegation_dir(parent_outer_cwd: &str, delegation_id: &str) -> PathBuf {
    Path::new(parent_outer_cwd)
        .join(HANDOFF_DIR)
        .join(delegation_id)
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
    subagent_id: String,
    message: String,
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

/// One-delegation-per-parent guard: a parent may not start a delegation while
/// another of its delegations is still running.
async fn reject_if_delegation_running(
    state: &AppState,
    parent_session_id: &str,
) -> std::result::Result<(), RpcError> {
    if state
        .repo
        .parent_has_running_delegation(parent_session_id)
        .await?
    {
        return Err(RpcError::new(
            "delegation_already_running",
            "a delegation is already running for this session; wait for it to finish before starting another",
        ));
    }
    Ok(())
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
            .workspaces
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
    reject_if_delegation_running(state, parent_session_id).await?;

    let delegation = state
        .repo
        .create_delegation(
            parent_session_id,
            DelegationKind::Full,
            params.workflow.as_deref(),
            params.label.as_deref(),
            1,
        )
        .await?;

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
    reject_if_delegation_running(state, parent_session_id).await?;

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
        .await?;

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

fn handoff_dir(parent_outer_cwd: &str, delegation_id: &str) -> String {
    Path::new(parent_outer_cwd)
        .join(HANDOFF_DIR)
        .join(delegation_id)
        .to_string_lossy()
        .into_owned()
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

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn count_failed_subagent_artifacts(artifacts: &[SubagentArtifact]) -> usize {
    artifacts
        .iter()
        .filter(|artifact| artifact.terminal_status == Some("failed"))
        .count()
}

fn delegation_view(
    delegation: &Delegation,
    subagents: Value,
    handoff_dir: String,
    terminal_count: usize,
    running_count: usize,
    failed_count: usize,
) -> Value {
    let spawned_count = subagents.as_array().map_or(0, Vec::len);
    json!({
        "delegation_id": delegation.id,
        "kind": delegation.kind,
        "status": delegation.status,
        "workflow": delegation.workflow,
        "label": delegation.label,
        "expected_subagents": delegation.expected_subagents,
        "spawned_subagents": spawned_count,
        "terminal_subagents": terminal_count,
        "running_subagents": running_count,
        "failed_subagents": failed_count,
        "progress": {
            "expected": delegation.expected_subagents,
            "spawned": spawned_count,
            "terminal": terminal_count,
            "running": running_count,
            "failed": failed_count,
        },
        "subagents": subagents,
        "handoff_dir": handoff_dir,
    })
}

pub(crate) async fn status_core(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: DelegationIdParams = from_params(params)?;
    let delegation =
        load_delegation_for_parent(state, parent_session_id, &params.delegation_id).await?;
    let include_final_messages = matches!(
        delegation.status,
        DelegationStatus::Done | DelegationStatus::DoneWithFailures
    );
    let (handoff_dir_path, artifacts) =
        refresh_delegation_handoff_artifacts(state, &delegation, include_final_messages).await?;
    let subagents = state.repo.list_delegation_subagents(&delegation.id).await?;
    let spawned_count = subagents.len();
    let mut terminal_count = 0usize;
    let mut running_count = 0usize;
    let count_artifact_terminality = matches!(
        delegation.status,
        DelegationStatus::Running | DelegationStatus::Done | DelegationStatus::DoneWithFailures
    );
    let failed_count = if count_artifact_terminality {
        count_failed_subagent_artifacts(&artifacts)
    } else {
        0
    };
    let mut subagent_views = Vec::with_capacity(subagents.len());
    for subagent in subagents {
        let artifact = artifacts
            .iter()
            .find(|artifact| artifact.session_id == subagent.session_id);
        let terminal_status = artifact.and_then(|artifact| artifact.terminal_status);
        if count_artifact_terminality && terminal_status.is_some() {
            terminal_count += 1;
        } else if delegation.status == DelegationStatus::Running {
            running_count += 1;
        }
        let final_message = artifact.and_then(|artifact| artifact.final_message.clone());
        let suggested_next = artifact.and_then(|artifact| artifact.suggested_next.clone());
        let final_message_path = artifact
            .and_then(|artifact| artifact.final_message_path.as_deref())
            .map(path_string);
        let final_message_relative_path = artifact.and_then(SubagentArtifact::final_message_rel);
        let transcript_path = artifact.map(|artifact| path_string(&artifact.transcript_path));
        let transcript_relative_path = artifact.map(SubagentArtifact::transcript_rel);
        let (cancellation_transcript_path, cancellation_transcript_relative_path) =
            if delegation.status == DelegationStatus::Cancelled {
                let relative = format!("cancelled/{}.transcript.md", subagent.session_id);
                let path = handoff_dir_path.join(&relative);
                if path.exists() {
                    (Some(path_string(&path)), Some(relative))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };
        let status = match delegation.status {
            DelegationStatus::Running => terminal_status
                .map(str::to_string)
                .unwrap_or_else(|| "running".to_string()),
            DelegationStatus::Done | DelegationStatus::DoneWithFailures => terminal_status
                .map(str::to_string)
                .unwrap_or_else(|| delegation.status.as_str().to_string()),
            DelegationStatus::Cancelled | DelegationStatus::Failed => {
                delegation.status.as_str().to_string()
            }
        };
        let has_active_work = if delegation.status == DelegationStatus::Running
            && subagent.subagent_type == Some(SubagentType::Full)
            && terminal_status.is_none()
        {
            state
                .repo
                .has_unfinished_actions(&subagent.session_id)
                .await?
                || state.repo.has_queued_inputs(&subagent.session_id).await?
                || subagent_has_active_runtime(state, &subagent.session_id).await
        } else {
            false
        };
        let steerable = delegation.status == DelegationStatus::Running
            && subagent.subagent_type == Some(SubagentType::Full)
            && terminal_status.is_none()
            && has_active_work;
        subagent_views.push(json!({
            "id": subagent.session_id,
            "role": subagent.role,
            "type": subagent.subagent_type,
            "subagent_type": subagent.subagent_type,
            "activity": subagent.activity,
            "status": status,
            "steerable": steerable,
            "final_message": final_message,
            "suggested_next": suggested_next,
            "final_message_path": final_message_path,
            "final_message_relative_path": final_message_relative_path.clone(),
            "final_message_file": final_message_relative_path,
            "transcript_path": transcript_path,
            "transcript_relative_path": transcript_relative_path.clone(),
            "transcript_file": transcript_relative_path,
            "cancellation_transcript_path": cancellation_transcript_path,
            "cancellation_transcript_relative_path": cancellation_transcript_relative_path,
            "task": subagent.task,
        }));
    }
    let expected_count = delegation.expected_subagents.max(0) as usize;
    if delegation.status == DelegationStatus::Running && spawned_count < expected_count {
        running_count += expected_count - spawned_count;
    }
    Ok(delegation_view(
        &delegation,
        json!(subagent_views),
        path_string(&handoff_dir_path),
        terminal_count,
        running_count,
        failed_count,
    ))
}

/// Cancel an in-flight delegation. Cancellation first wins an attempt-fenced
/// `running -> cancelled` CAS; only the CAS winner interrupts subagents and
/// writes transcript-only artifacts. If completion or another cancellation wins
/// first, this returns `{ "cancelled": false }` and leaves existing artifacts
/// untouched. Interrupting a read-only subagent is allowed here because the
/// whole delegation is being torn down (the per-subagent guard only blocks the
/// model/parent from steering or interrupting an individual RO subagent).
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
    let won_cancel = state
        .repo
        .cancel_running_delegation(&delegation.id, &delegation.attempt_id)
        .await?;
    if !won_cancel {
        return Ok(json!({ "cancelled": false }));
    }
    cancel_delegation_subagents_without_reactivation(state, &delegation.id).await;
    let transcript_paths = write_cancelled_subagent_transcripts(state, &delegation).await?;
    Ok(json!({
        "cancelled": true,
        "transcripts": transcript_paths,
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
) -> std::result::Result<Vec<Value>, RpcError> {
    let parent_config = state
        .repo
        .load_session_config(&delegation.parent_session_id)
        .await?;
    let dir = delegation_dir(&parent_config.outer_cwd, &delegation.id).join("cancelled");
    let subagents = state.repo.list_delegation_subagents(&delegation.id).await?;
    let mut transcripts = Vec::with_capacity(subagents.len());
    for subagent in &subagents {
        let history = state.repo.active_branch(&subagent.session_id).await?;
        let transcript = render_transcript_markdown(&history);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(anyhow::Error::from)?;
        let path = dir.join(format!("{}.transcript.md", subagent.session_id));
        tokio::fs::write(&path, transcript.as_bytes())
            .await
            .map_err(anyhow::Error::from)?;
        transcripts.push(json!({
            "subagent_id": subagent.session_id,
            "transcript": path.to_string_lossy(),
        }));
    }
    Ok(transcripts)
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

/// The handoff files a client may read. `final_message.md`/`transcript.md` live
/// under a subagent dir. The closed filename set is the whole reason a caller
/// cannot smuggle an arbitrary filename.
fn validate_subagent_handoff_file(file: &str) -> std::result::Result<(), RpcError> {
    match file {
        "final_message.md" | "transcript.md" => Ok(()),
        other => Err(RpcError::new(
            "invalid_params",
            format!("file must be one of final_message.md | transcript.md, got {other}"),
        )),
    }
}

/// Reject any path segment that is not a plain file/dir name. A single
/// `Component::Normal` with no separators, no `.`/`..`, and no NUL is the only
/// thing that can ever escape the handoff subtree, so we validate the segment
/// in isolation before it is ever joined onto the trusted base.
fn safe_path_segment(segment: &str, field: &str) -> std::result::Result<String, RpcError> {
    let trimmed = segment.trim();
    let reject = || {
        RpcError::new(
            "invalid_params",
            format!("{field} is not a valid path segment"),
        )
    };
    if trimmed.is_empty() || trimmed.contains('\0') {
        return Err(reject());
    }
    let mut components = Path::new(trimmed).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) if name == std::ffi::OsStr::new(trimmed) => {
            Ok(trimmed.to_string())
        }
        _ => Err(reject()),
    }
}

/// Resolve a handoff file request to an absolute path strictly under
/// `<parent_outer_cwd>/.pi-handoff/<delegation_id>/`. Every dynamic segment
/// (`delegation_id`, optional `subagent_id`, `file`) is validated as a single safe
/// path component, so the result can never traverse out of the handoff subtree.
fn resolve_handoff_file_path(
    parent_outer_cwd: &str,
    delegation_id: &str,
    subagent_id: Option<&str>,
    file: &str,
) -> std::result::Result<PathBuf, RpcError> {
    validate_subagent_handoff_file(file)?;
    let delegation_segment = safe_path_segment(delegation_id, "delegation_id")?;
    let mut path = Path::new(parent_outer_cwd)
        .join(HANDOFF_DIR)
        .join(delegation_segment);
    let subagent_id = subagent_id
        .ok_or_else(|| RpcError::new("invalid_params", format!("{file} requires a subagent_id")))?;
    path.push(safe_path_segment(subagent_id, "subagent_id")?);
    // `file` is already constrained to the known literals above.
    path.push(file);
    Ok(path)
}

/// Read one handoff file for the run board. The web client cannot read host
/// files directly; this is the only path through which it reaches the handoff
/// subtree, and it is scoped to the parent (the delegation must belong to it,
/// exactly like `delegation.status`) and traversal-safe (every segment is
/// validated).
pub(crate) async fn read_handoff_file_core(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: ReadHandoffFileParams = from_params(params)?;
    let delegation =
        load_delegation_for_parent(state, parent_session_id, &params.delegation_id).await?;
    let include_final_messages = matches!(
        delegation.status,
        DelegationStatus::Done | DelegationStatus::DoneWithFailures
    );
    let _ =
        refresh_delegation_handoff_artifacts(state, &delegation, include_final_messages).await?;
    // A subagent-scoped read may only target a subagent that belongs to this
    // delegation; otherwise a caller could probe arbitrary
    // `<delegation>/<segment>/` paths.
    if let Some(subagent_id) = params.subagent_id.as_deref() {
        let members = state.repo.list_delegation_subagents(&delegation.id).await?;
        if !members
            .iter()
            .any(|member| member.session_id == subagent_id)
        {
            return Err(RpcError::new(
                "handoff_file_not_found",
                "subagent does not belong to this delegation",
            ));
        }
    }
    let parent_config = state.repo.load_session_config(parent_session_id).await?;
    let path = resolve_handoff_file_path(
        &parent_config.outer_cwd,
        &delegation.id,
        params.subagent_id.as_deref(),
        &params.file,
    )?;
    // Defense in depth: confine the symlink-resolved target under the parent's
    // handoff dir so a symlink planted inside it cannot escape to an arbitrary
    // host file. Segment validation above already blocks `..`/abs paths.
    let handoff_root = Path::new(&parent_config.outer_cwd).join(HANDOFF_DIR);
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
    let canonical_root = match tokio::fs::canonicalize(&handoff_root).await {
        Ok(root) => root,
        Err(error) => {
            eprintln!(
                "failed to resolve handoff root {}: {error:#}",
                handoff_root.display()
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
        "subagent_id": params.subagent_id,
        "file": params.file,
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

/// Per-parent delegation list for the run board: each delegation with its
/// kind/status and its subagents' ids/status. (The model-facing tool surface
/// has no list.)
pub(crate) async fn rpc_list(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let parent_session_id = parent_session_id_from_params(&params)?;
    let parent_config = state.repo.load_session_config(&parent_session_id).await?;
    let delegations = state
        .repo
        .list_parent_delegations(&parent_session_id)
        .await?;
    let mut views = Vec::with_capacity(delegations.len());
    for delegation in &delegations {
        let subagents = state
            .repo
            .list_delegation_subagents(&delegation.id)
            .await?
            .into_iter()
            .map(|subagent| {
                json!({
                    "id": subagent.session_id,
                    "status": subagent.activity,
                    "activity": subagent.activity,
                    "role": subagent.role,
                    "type": subagent.subagent_type,
                    "subagent_type": subagent.subagent_type,
                    "task": subagent.task,
                    "steerable": delegation.status == DelegationStatus::Running
                        && subagent.subagent_type == Some(SubagentType::Full),
                })
            })
            .collect::<Vec<_>>();
        views.push(json!({
            "delegation_id": delegation.id,
            "kind": delegation.kind,
            "status": delegation.status,
            "workflow": delegation.workflow,
            "label": delegation.label,
            "subagents": subagents,
            "handoff_dir": handoff_dir(&parent_config.outer_cwd, &delegation.id),
        }));
    }
    Ok(json!({
        "parent_session_id": parent_session_id,
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
    )
}

/// Model-facing dispatch: run the core fn for the named delegation tool and
/// wrap the result as a tool result message. The session id is the parent's.
pub(crate) async fn run_delegation_tool(
    state: &AppState,
    parent_session_id: &str,
    call: &ToolCall,
) -> ToolResultMessage {
    let params: Value = match serde_json::from_str(&call.args_json) {
        Ok(params) => params,
        Err(error) => {
            return ToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                format!("{} arguments were invalid JSON: {error}", call.tool_name),
            )
        }
    };
    let result = match call.tool_name.as_str() {
        "delegate_writing_task" => start_full_core(state, parent_session_id, params).await,
        "delegate_readonly_tasks" => {
            start_readonly_fanout_core(state, parent_session_id, params).await
        }
        "inspect_delegation" => status_core(state, parent_session_id, params).await,
        "cancel_delegation" => cancel_core(state, parent_session_id, params).await,
        "steer_subagent" => steer_subagent_core(state, parent_session_id, params).await,
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
    fn stale_stage_id_parameter_is_rejected() {
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
        let path =
            resolve_handoff_file_path(CWD, "delegation-1", Some("child-9"), "final_message.md")
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
    fn rejects_unknown_file_name() {
        let error =
            resolve_handoff_file_path(CWD, "delegation-1", None, "secrets.env").unwrap_err();
        assert_eq!(error.code, "invalid_params");
    }

    #[test]
    fn rejects_index_json_because_inspect_delegation_is_the_manifest() {
        let error = resolve_handoff_file_path(CWD, "delegation-1", None, "index.json").unwrap_err();
        assert_eq!(error.code, "invalid_params");
    }

    #[test]
    fn rejects_traversal_in_delegation_id() {
        for evil in ["..", "../other", "a/b", "/etc", "delegation/../..", "."] {
            let error =
                resolve_handoff_file_path(CWD, evil, Some("child"), "transcript.md").unwrap_err();
            assert_eq!(
                error.code, "invalid_params",
                "delegation_id {evil} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_traversal_in_subagent_id() {
        for evil in ["..", "../x", "a/b", "/abs"] {
            let error = resolve_handoff_file_path(CWD, "delegation-1", Some(evil), "transcript.md")
                .unwrap_err();
            assert_eq!(
                error.code, "invalid_params",
                "subagent_id {evil} must be rejected"
            );
        }
    }

    #[test]
    fn requires_subagent_id_for_subagent_files() {
        let error =
            resolve_handoff_file_path(CWD, "delegation-1", None, "transcript.md").unwrap_err();
        assert_eq!(error.code, "invalid_params");
    }
}
