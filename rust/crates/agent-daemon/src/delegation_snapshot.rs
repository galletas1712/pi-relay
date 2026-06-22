use agent_store::{Delegation, DelegationStatus, SubagentType};
use agent_vocab::{DaemonToolObservation, ToolCallId};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::handoff::{
    delegation_dir, refresh_delegation_handoff_artifacts, refresh_task_prompt_artifact_if_present,
    safe_handoff_path_segment, task_prompt_rel, SubagentArtifact,
};
use crate::state::AppState;
use crate::types::RpcError;

fn count_failed_subagent_artifacts(artifacts: &[SubagentArtifact]) -> usize {
    artifacts
        .iter()
        .filter(|artifact| artifact.terminal_status == Some("failed"))
        .count()
}

async fn inspectable_handoff_artifacts(
    state: &AppState,
    delegation: &Delegation,
) -> std::result::Result<(std::path::PathBuf, Vec<SubagentArtifact>), RpcError> {
    let parent_config = state
        .repo
        .load_session_config(&delegation.parent_session_id)
        .await?;
    let dir = delegation_dir(&parent_config.outer_cwd, &delegation.id);
    let include_final_messages = matches!(
        delegation.status,
        DelegationStatus::Done | DelegationStatus::DoneWithFailures
    );
    match delegation.status {
        DelegationStatus::Running | DelegationStatus::Done | DelegationStatus::DoneWithFailures => {
            refresh_delegation_handoff_artifacts(state, delegation, include_final_messages).await
        }
        // Cancellation writes transcript-only files under `cancelled/` at the
        // point the cancellation CAS wins. Failed delegations represent startup
        // failures, not a normal barrier result. In both cases inspection must
        // not materialize normal per-subagent handoff files.
        DelegationStatus::Cancelled | DelegationStatus::Failed => Ok((dir, Vec::new())),
    }
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

async fn subagent_has_active_runtime(state: &AppState, subagent_id: &str) -> bool {
    state.active.lock().await.contains_key(subagent_id)
}

/// Build the rich delegation snapshot returned by `inspect_delegation`.
///
/// This is also the canonical payload for terminal parent wakeups. It refreshes
/// artifact files that are valid for the delegation's current status, includes
/// per-subagent `suggested_next` values when available, and reports compact
/// handoff file references without inlining transcript, task-prompt, or
/// final-message prose.
pub(crate) async fn build_delegation_snapshot(
    state: &AppState,
    delegation: &Delegation,
) -> std::result::Result<Value, RpcError> {
    let (handoff_dir_path, artifacts) = inspectable_handoff_artifacts(state, delegation).await?;
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
        let suggested_next = artifact.and_then(|artifact| artifact.suggested_next.clone());
        let final_message_file = artifact.and_then(SubagentArtifact::final_message_rel);
        let normal_transcript_file = artifact.map(SubagentArtifact::transcript_rel);
        let task_prompt_file = if let Some(artifact) = artifact {
            artifact.task_prompt_rel()
        } else {
            let task_prompt = refresh_task_prompt_artifact_if_present(
                &handoff_dir_path,
                &subagent.session_id,
                subagent.task.as_deref(),
            )
            .await?;
            task_prompt
                .as_ref()
                .map(|_| task_prompt_rel(&subagent.session_id))
        };
        let transcript_file = if delegation.status == DelegationStatus::Cancelled {
            let subagent_segment = safe_handoff_path_segment(&subagent.session_id, "subagent_id")?;
            let relative = format!("cancelled/{subagent_segment}.transcript.md");
            let path = handoff_dir_path.join(&relative);
            if path.exists() {
                Some(relative)
            } else {
                None
            }
        } else {
            normal_transcript_file
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
            "suggested_next": suggested_next,
            "final_message_file": final_message_file,
            "transcript_file": transcript_file,
            "task_prompt_file": task_prompt_file,
        }));
    }
    let expected_count = delegation.expected_subagents.max(0) as usize;
    if delegation.status == DelegationStatus::Running && spawned_count < expected_count {
        running_count += expected_count - spawned_count;
    }
    Ok(delegation_view(
        delegation,
        json!(subagent_views),
        handoff_dir_path.to_string_lossy().into_owned(),
        terminal_count,
        running_count,
        failed_count,
    ))
}

fn snapshot_string(snapshot: &Value, key: &str) -> Option<String> {
    snapshot
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn snapshot_progress_count(snapshot: &Value, key: &str) -> usize {
    snapshot
        .get("progress")
        .and_then(|progress| progress.get(key))
        .and_then(Value::as_u64)
        .unwrap_or_default() as usize
}

fn short_delegation_observation_call_id(delegation: &Delegation) -> ToolCallId {
    // OpenAI Responses rejects `call_id` values longer than 64 characters.
    // Delegation ids and attempt ids are both UUID-bearing strings, so spelling
    // both out (`call_inspect_delegation_<delegation>_<attempt>`) can exceed
    // that limit. Keep a human-recognizable prefix and a deterministic digest
    // of both durable ids; the full delegation id remains in args_json/result.
    let mut hasher = Sha256::new();
    hasher.update(delegation.id.as_bytes());
    hasher.update(b"\0");
    hasher.update(delegation.attempt_id.as_bytes());
    let digest = hasher.finalize();
    let mut suffix = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(&mut suffix, "{byte:02x}");
    }
    ToolCallId::new(format!("call_inspect_delegation_{suffix}"))
}

/// Build the terminal daemon observation delivered to the parent after a
/// delegation completes.
///
/// This is represented as a daemon-authored observation rather than a
/// fabricated assistant tool call. The observation deliberately carries the same
/// JSON snapshot as `inspect_delegation`, instead of directing the parent to a
/// root artifact file. Compact handoff file references are present in the
/// snapshot; transcript contents are not inlined.
pub(crate) fn completion_wakeup_observation(
    snapshot: &Value,
    delegation: &Delegation,
) -> std::result::Result<DaemonToolObservation, RpcError> {
    let delegation_id =
        snapshot_string(snapshot, "delegation_id").unwrap_or_else(|| "<unknown>".to_string());
    let kind = match snapshot.get("kind").and_then(Value::as_str) {
        Some("full") => "full subagent",
        Some("readonly_fanout") => "read-only fan-out",
        Some(other) => other,
        None => "delegation",
    };
    let label = snapshot_string(snapshot, "label")
        .map(|label| format!(" ({label})"))
        .unwrap_or_default();
    let status = snapshot_string(snapshot, "status").unwrap_or_else(|| "terminal".to_string());
    let terminal = snapshot_progress_count(snapshot, "terminal");
    let failed = snapshot_progress_count(snapshot, "failed");
    let ok = terminal.saturating_sub(failed);
    let summary = format!(
        "Delegation {delegation_id} ({kind}){label} completed with status {status}: {ok} ok, {failed} failed."
    );
    let tool_call_id = short_delegation_observation_call_id(delegation);
    Ok(DaemonToolObservation::inspect_delegation(
        tool_call_id,
        delegation_id,
        Some(summary),
        snapshot.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_wakeup_observation_carries_bounded_snapshot_and_summary() {
        let snapshot = json!({
            "delegation_id": "delegation_1",
            "kind": "readonly_fanout",
            "status": "done",
            "label": "review",
            "progress": {
                "expected": 1,
                "spawned": 1,
                "terminal": 1,
                "running": 0,
                "failed": 0,
            },
            "subagents": [{
                "id": "child_1",
                "status": "done",
                "final_message_file": "child_1/final_message.md",
                "suggested_next": "approved",
                "task_prompt_file": "child_1/task_prompt.md",
                "transcript_file": "child_1/transcript.md",
            }],
            "handoff_dir": "/tmp/.pi-handoff/delegation_1",
        });
        let delegation = Delegation {
            id: "delegation_1".to_string(),
            parent_session_id: "parent".to_string(),
            workflow: None,
            label: Some("review".to_string()),
            kind: agent_store::DelegationKind::ReadonlyFanout,
            status: DelegationStatus::Done,
            attempt_id: "attempt-1".to_string(),
            expected_subagents: 1,
        };

        let observation =
            completion_wakeup_observation(&snapshot, &delegation).expect("observation");

        assert_eq!(observation.tool_name, "inspect_delegation");
        assert!(observation
            .tool_call_id
            .as_str()
            .starts_with("call_inspect_delegation_"));
        assert!(observation.tool_call_id.as_str().len() <= 64);
        assert_eq!(
            observation.args_json,
            "{\"delegation_id\":\"delegation_1\"}"
        );
        assert_eq!(observation.result_json, snapshot);
        assert!(observation
            .summary
            .as_deref()
            .unwrap()
            .contains("completed with status done"));
        assert!(observation
            .render_text()
            .unwrap()
            .contains("large prompts/messages are not inlined"));
    }

    #[test]
    fn completion_wakeup_observation_call_id_stays_under_provider_limit_for_uuid_ids() {
        let snapshot = json!({
            "delegation_id": "delegation_6d17ff90-6e46-4c3f-88ad-d92d77350d52",
            "kind": "readonly_fanout",
            "status": "done",
            "progress": {
                "terminal": 4,
                "failed": 0,
            },
            "subagents": [],
            "handoff_dir": "/tmp/.pi-handoff/delegation_6d17ff90-6e46-4c3f-88ad-d92d77350d52",
        });
        let delegation = Delegation {
            id: "delegation_6d17ff90-6e46-4c3f-88ad-d92d77350d52".to_string(),
            parent_session_id: "parent".to_string(),
            workflow: None,
            label: None,
            kind: agent_store::DelegationKind::ReadonlyFanout,
            status: DelegationStatus::Done,
            attempt_id: "62847e1a-b705-48ee-899b-b062ccdf38f6".to_string(),
            expected_subagents: 4,
        };

        let first = completion_wakeup_observation(&snapshot, &delegation)
            .expect("observation")
            .tool_call_id;
        let second = completion_wakeup_observation(&snapshot, &delegation)
            .expect("observation")
            .tool_call_id;

        assert_eq!(first, second);
        assert!(first.as_str().starts_with("call_inspect_delegation_"));
        assert!(first.as_str().len() <= 64);
        assert_ne!(
            first.as_str(),
            "call_inspect_delegation_delegation_6d17ff90_6e46_4c3f_88ad_d92d77350d52_62847e1a_b705_48ee_899b_b062ccdf38f6"
        );
    }
}
