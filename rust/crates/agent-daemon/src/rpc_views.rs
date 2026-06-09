use std::time::{SystemTime, UNIX_EPOCH};

use agent_store::{
    ActiveBranchSync, HistoryTree, Project, QueueState, QueuedInputRecord, SessionRelationship,
    SessionSnapshot, SessionSummary, SwitchActiveLeafResult, TranscriptEntriesResult,
    TranscriptEntryRecord, TranscriptTreeIndex, TranscriptTurnDetailResult, TranscriptTurnsResult,
    TurnCardRecord,
};
use serde_json::{json, Value};

pub(crate) fn project(project: Project) -> Value {
    json!({
        "project_id": project.project_id,
        "name": project.name,
        "workspaces": project.workspaces,
        "metadata": project.metadata,
        "created_at": project.created_at,
        "updated_at": project.updated_at,
    })
}

pub(crate) fn session_summary(summary: SessionSummary) -> Value {
    json!({
        "session_id": summary.session_id,
        "project_id": summary.project_id,
        "outer_cwd": summary.outer_cwd,
        "workspaces": summary.workspaces,
        "activity": summary.activity,
        "active_leaf_id": summary.active_leaf_id,
        "provider": summary.provider,
        "metadata": summary.metadata,
        "created_at": summary.created_at,
        "updated_at": summary.updated_at,
        "last_user_message_timestamp_ms": summary.last_user_message_timestamp_ms,
        "has_transcript_entries": summary.has_transcript_entries,
    })
}

pub(crate) fn session_snapshot(
    snapshot: SessionSnapshot,
    entries: Option<Vec<TranscriptEntryRecord>>,
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
        .map(queued_input)
        .collect::<Vec<_>>();

    let mut value = json!({
        "session_id": snapshot.session_id,
        "project_id": snapshot.project_id,
        "outer_cwd": snapshot.outer_cwd,
        "workspaces": snapshot.workspaces,
        "activity": snapshot.activity,
        "active_leaf_id": snapshot.active_leaf_id,
        "provider": snapshot.provider,
        "metadata": snapshot.metadata,
        "pending_actions": pending_actions,
        "queued_inputs": queued_inputs,
        "session_revision": snapshot.session_revision,
        "queue_revision": snapshot.queue_revision,
        "transcript_revision": snapshot.transcript_revision,
        "last_event_id": snapshot.last_event_id,
        "last_user_message_timestamp_ms": snapshot.last_user_message_timestamp_ms,
        "has_transcript_entries": snapshot.has_transcript_entries,
        "server_time_ms": now_ms(),
    });
    if let Some(entries) = entries {
        value["entries"] = json!(transcript_entry_values(entries));
    }
    value
}

pub(crate) fn queue_state(queue: QueueState) -> Value {
    json!({
        "session_revision": queue.session_revision,
        "queue_revision": queue.queue_revision,
        "transcript_revision": queue.transcript_revision,
        "activity": queue.activity,
        "queued_inputs": queue
            .queued_inputs
            .into_iter()
            .map(queued_input)
            .collect::<Vec<_>>(),
    })
}

pub(crate) fn session_relationship(relationship: &SessionRelationship) -> Value {
    json!({
        "relationship_id": relationship.relationship_id,
        "parent_session_id": relationship.parent_session_id,
        "child_session_id": relationship.child_session_id,
        "created_at": relationship.created_at,
        "updated_at": relationship.updated_at,
    })
}

fn queued_input(input: QueuedInputRecord) -> Value {
    json!({
        "input_id": input.input_id,
        "priority": input.priority,
        "status": input.status,
        "content": input.content.content,
        "client_input_id": input.client_input_id,
        "created_at": input.created_at,
        "updated_at": input.updated_at,
        "promoted_at": input.promoted_at,
        "follow_up_position": input.follow_up_position,
    })
}

pub(crate) fn history_tree(tree: HistoryTree) -> Value {
    json!({
        "session_id": tree.session_id,
        "active_leaf_id": tree.active_leaf_id,
        "entries": transcript_entry_values(tree.entries),
    })
}

pub(crate) fn active_branch_sync(sync: ActiveBranchSync, overview: SessionSnapshot) -> Value {
    let base_leaf_id = sync.base_leaf_id;
    let active_leaf_id = sync.active_leaf_id;
    let status = sync.status;
    let entries = sync.entries;
    json!({
        "session_id": sync.session_id,
        "base_leaf_id": base_leaf_id,
        "active_leaf_id": active_leaf_id,
        "status": status,
        "entries": transcript_entry_values(entries),
        "overview": session_snapshot(overview, None),
    })
}

pub(crate) fn transcript_tree_index(index: TranscriptTreeIndex) -> Value {
    json!({
        "session_id": index.session_id,
        "active_leaf_id": index.active_leaf_id,
        "session_revision": index.session_revision,
        "transcript_revision": index.transcript_revision,
        "after_sequence": index.after_sequence,
        "max_sequence": index.max_sequence,
        "complete": index.complete,
        "nodes": index.nodes,
    })
}

pub(crate) fn transcript_entries(result: TranscriptEntriesResult) -> Value {
    json!({
        "session_id": result.session_id,
        "session_revision": result.session_revision,
        "transcript_revision": result.transcript_revision,
        "entries": transcript_entry_values(result.entries),
    })
}

pub(crate) fn transcript_turns(result: TranscriptTurnsResult) -> Value {
    json!({
        "session_id": result.session_id,
        "active_leaf_id": result.active_leaf_id,
        "session_revision": result.session_revision,
        "transcript_revision": result.transcript_revision,
        "before_entry_id": result.before_entry_id,
        "next_before_entry_id": result.next_before_entry_id,
        "has_more_before": result.has_more_before,
        "limit": result.limit,
        "cards": result.cards.into_iter().map(turn_card).collect::<Vec<_>>(),
    })
}

pub(crate) fn transcript_turn_detail(result: TranscriptTurnDetailResult) -> Value {
    json!({
        "session_id": result.session_id,
        "active_leaf_id": result.active_leaf_id,
        "session_revision": result.session_revision,
        "transcript_revision": result.transcript_revision,
        "card_id": result.card_id,
        "entries": transcript_entry_values(result.entries),
    })
}

pub(crate) fn switch_active_leaf(result: SwitchActiveLeafResult) -> Value {
    json!({
        "session_id": result.session_id,
        "active_leaf_id": result.active_leaf_id,
        "activity": result.activity,
        "session_revision": result.session_revision,
        "queue_revision": result.queue_revision,
        "transcript_revision": result.transcript_revision,
        "last_event_id": result.last_event_id,
        "active_branch_entry_ids": result.active_branch_entry_ids,
        "active_branch_entries": result.active_branch_entries.map(transcript_entry_values),
    })
}

fn turn_card(card: TurnCardRecord) -> Value {
    json!({
        "id": card.id,
        "turn_id": card.turn_id,
        "status": card.status,
        "outcome": card.outcome,
        "start_entry_id": card.start_entry_id,
        "boundary_entry_id": card.boundary_entry_id,
        "active_leaf_id": card.active_leaf_id,
        "start_sequence": card.start_sequence,
        "end_sequence": card.end_sequence,
        "start_timestamp_ms": card.start_timestamp_ms,
        "timestamp_ms": card.timestamp_ms,
        "user_messages": transcript_entry_values(card.user_messages),
        "assistant_message": card.assistant_message.map(transcript_entry),
        "summary": card.summary,
        "can_resume": card.can_resume,
    })
}

fn transcript_entry_values(entries: Vec<TranscriptEntryRecord>) -> Vec<Value> {
    entries.into_iter().map(transcript_entry).collect()
}

fn transcript_entry(entry: TranscriptEntryRecord) -> Value {
    json!({
        "id": entry.id,
        "parent_id": entry.parent_id,
        "timestamp_ms": entry.timestamp_ms,
        "sequence": entry.sequence,
        "item": entry.item,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}
