use std::path::Path;

use agent_store::{Delegation, DelegationStatus, SubagentType};
use serde_json::{json, Value};

use crate::handoff::{delegation_dir, refresh_delegation_handoff_artifacts, SubagentArtifact};
use crate::state::AppState;
use crate::types::RpcError;

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

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
/// per-subagent final messages / `suggested_next` values when available, and
/// reports artifact paths without inlining full transcript contents.
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
        delegation,
        json!(subagent_views),
        path_string(&handoff_dir_path),
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

/// Render the terminal wakeup delivered to the parent after a delegation
/// completes.
///
/// The message deliberately carries the same JSON snapshot as
/// `inspect_delegation`, instead of directing the parent to a manifest file.
/// Transcript artifact paths are present in the snapshot; transcript contents
/// are not inlined.
pub(crate) fn completion_wakeup_message(snapshot: &Value) -> std::result::Result<String, RpcError> {
    let json_snapshot = serde_json::to_string_pretty(snapshot).map_err(anyhow::Error::from)?;
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
    Ok(format!(
        "Delegation {delegation_id} ({kind}){label} completed with status {status}: {ok} ok, {failed} failed.\n\n\
         Snapshot JSON (equivalent to inspect_delegation at wakeup time):\n\
         ```json\n{json_snapshot}\n```\n\n\
         Final-message and transcript artifact paths are included in the snapshot. Full transcript contents are not inlined."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded_snapshot(message: &str) -> Value {
        let start = message
            .find("```json\n")
            .map(|index| index + "```json\n".len())
            .expect("json fence");
        let rest = &message[start..];
        let end = rest.find("\n```").expect("json fence end");
        serde_json::from_str(&rest[..end]).expect("valid snapshot json")
    }

    #[test]
    fn completion_wakeup_embeds_snapshot_paths_without_transcript_body() {
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
                "final_message": "Looks good.\n\nsuggested_next: approved",
                "suggested_next": "approved",
                "final_message_path": "/tmp/.pi-handoff/delegation_1/child_1/final_message.md",
                "transcript_path": "/tmp/.pi-handoff/delegation_1/child_1/transcript.md",
            }],
            "handoff_dir": "/tmp/.pi-handoff/delegation_1",
        });

        let message = completion_wakeup_message(&snapshot).expect("wakeup");

        assert!(message.contains("completed with status done"));
        assert!(message.contains("Snapshot JSON"));
        assert!(message.contains("Full transcript contents are not inlined"));
        assert!(!message.contains("index.json"));
        assert!(!message.contains("## User"));
        assert_eq!(embedded_snapshot(&message), snapshot);
    }
}
