use agent_session::StoredTranscriptEntry;
use agent_store::{GlobalConfig, HistoryTree, Project, SessionSnapshot, SessionSummary};
use serde_json::{json, Value};

pub(crate) fn global_config(config: GlobalConfig) -> Value {
    json!({
        "system_prompt": config.system_prompt,
    })
}

pub(crate) fn project(project: Project) -> Value {
    json!({
        "project_id": project.project_id,
        "name": project.name,
        "starting_cwd": project.starting_cwd,
        "metadata": project.metadata,
        "created_at": project.created_at,
        "updated_at": project.updated_at,
    })
}

pub(crate) fn session_summary(summary: SessionSummary) -> Value {
    json!({
        "session_id": summary.session_id,
        "project_id": summary.project_id,
        "starting_cwd": summary.starting_cwd,
        "activity": summary.activity,
        "active_leaf_id": summary.active_leaf_id,
        "provider": summary.provider,
        "metadata": summary.metadata,
        "created_at": summary.created_at,
        "updated_at": summary.updated_at,
    })
}

pub(crate) fn session_snapshot(
    snapshot: SessionSnapshot,
    entries: Option<Vec<StoredTranscriptEntry>>,
) -> Value {
    let pending_actions = snapshot
        .pending_actions
        .into_iter()
        .map(|action| {
            json!({
                "action_row_id": action.action_row_id,
                "kind": action.kind,
                "status": action.status,
                "payload": action.payload,
            })
        })
        .collect::<Vec<_>>();
    let queued_inputs = snapshot
        .queued_inputs
        .into_iter()
        .map(|input| {
            json!({
                "input_id": input.input_id,
                "priority": input.priority,
                "status": input.status,
                "content": input.content.content,
                "client_input_id": input.client_input_id,
                "created_at": input.created_at,
                "promoted_at": input.promoted_at,
            })
        })
        .collect::<Vec<_>>();

    let mut value = json!({
        "session_id": snapshot.session_id,
        "project_id": snapshot.project_id,
        "starting_cwd": snapshot.starting_cwd,
        "activity": snapshot.activity,
        "active_leaf_id": snapshot.active_leaf_id,
        "provider": snapshot.provider,
        "metadata": snapshot.metadata,
        "pending_actions": pending_actions,
        "queued_inputs": queued_inputs,
        "last_event_id": snapshot.last_event_id,
    });
    if let Some(entries) = entries {
        value["entries"] = json!(entries);
    }
    value
}

pub(crate) fn history_tree(tree: HistoryTree) -> Value {
    json!({
        "session_id": tree.session_id,
        "active_leaf_id": tree.active_leaf_id,
        "entries": tree.entries,
    })
}
