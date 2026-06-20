use std::collections::HashMap;

use agent_session::{SessionAction, SessionActionKind, SessionEvent};
use agent_vocab::TranscriptItem;
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::{Executor, Postgres, Row, Transaction};

use crate::{
    EventFrame, EventType, SessionActivity, TranscriptEntryBodyMode, TranscriptEntryRecord,
};

use super::action_records::{action_payload, ActionKey};
use super::rows::row_to_event;
use super::transcript::{
    session_state_for_event_tx, transcript_entry_record_tx, tree_node_from_entry, SessionEventState,
};
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn last_event_id(&self, session_id: &str) -> Result<i64> {
        sqlx::query_scalar("select coalesce(max(id),0)::bigint from events where session_id=$1")
            .bind(session_id)
            .fetch_one(&self.pool)
            .await
            .map_err(Into::into)
    }

    pub async fn clear_session_events(&self, session_id: &str) -> Result<()> {
        sqlx::query("delete from events where session_id=$1")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_event(
        &self,
        session_id: &str,
        event: EventType,
        data: Value,
    ) -> Result<EventFrame> {
        insert_event_row(&self.pool, session_id, event, data).await
    }

    pub async fn insert_events(
        &self,
        session_id: &str,
        events: Vec<(EventType, Value)>,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        let mut frames = Vec::with_capacity(events.len());
        for (event_type, payload) in events {
            frames.push(insert_event_tx(&mut tx, session_id, event_type, payload).await?);
        }
        tx.commit().await?;
        Ok(frames)
    }

    pub async fn insert_subagent_idle_event_once(
        &self,
        parent_session_id: &str,
        child_session_id: &str,
        notification_key: &str,
        payload: Value,
    ) -> Result<Option<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("select metadata from sessions where id=$1 for update")
            .bind(child_session_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let mut metadata: Value = row.get("metadata");
        if metadata
            .get("subagent_parent_idle_notification_key")
            .and_then(Value::as_str)
            == Some(notification_key)
        {
            tx.commit().await?;
            return Ok(None);
        }

        let event =
            insert_event_tx(&mut tx, parent_session_id, EventType::SubagentIdle, payload).await?;
        ensure_payload_object(&mut metadata).insert(
            "subagent_parent_idle_notification_key".to_string(),
            Value::String(notification_key.to_string()),
        );
        sqlx::query("update sessions set metadata=$2, updated_at=now() where id=$1")
            .bind(child_session_id)
            .bind(&metadata)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(event))
    }

    /// Claim the once-only terminal-idle gate for a child WITHOUT writing a
    /// parent-visible `SubagentIdle` row. Returns `true` exactly once per
    /// `notification_key` (the same dedup `insert_subagent_idle_event_once`
    /// uses), so a stage member's barrier + RO-snapshot destroy still fire
    /// exactly once while no per-child idle ever surfaces to the parent.
    pub async fn claim_subagent_idle_once(
        &self,
        child_session_id: &str,
        notification_key: &str,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("select metadata from sessions where id=$1 for update")
            .bind(child_session_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        let mut metadata: Value = row.get("metadata");
        if metadata
            .get("subagent_parent_idle_notification_key")
            .and_then(Value::as_str)
            == Some(notification_key)
        {
            tx.commit().await?;
            return Ok(false);
        }
        ensure_payload_object(&mut metadata).insert(
            "subagent_parent_idle_notification_key".to_string(),
            Value::String(notification_key.to_string()),
        );
        sqlx::query("update sessions set metadata=$2, updated_at=now() where id=$1")
            .bind(child_session_id)
            .bind(&metadata)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn events_after(
        &self,
        session_id: &str,
        after: Option<i64>,
    ) -> Result<Vec<EventFrame>> {
        let after = after.unwrap_or(0);
        let rows = sqlx::query(
            "select id, session_id, type, payload from events where session_id=$1 and id>$2 order by id",
        )
        .bind(session_id)
        .bind(after)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_event).collect()
    }
}

pub(super) async fn insert_event_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    event_type: EventType,
    payload: Value,
) -> Result<EventFrame> {
    insert_event_row(&mut **tx, session_id, event_type, payload).await
}

pub(super) async fn insert_event_with_activity_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    event_type: EventType,
    mut payload: Value,
) -> Result<EventFrame> {
    if let Some(activity) = event_activity_hint(event_type) {
        ensure_payload_object(&mut payload)
            .entry("activity".to_string())
            .or_insert_with(|| serde_json::to_value(activity).unwrap_or(Value::Null));
    }
    insert_event_tx(tx, session_id, event_type, payload).await
}

async fn insert_event_row<'e, E>(
    executor: E,
    session_id: &str,
    event_type: EventType,
    payload: Value,
) -> Result<EventFrame>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query(
        "insert into events (session_id, type, payload) values ($1::text, $2::text, $3) returning id, session_id, type, payload",
    )
    .bind(session_id)
    .bind(event_type.as_str())
    .bind(payload)
    .fetch_one(executor)
    .await?;
    row_to_event(row)
}

fn ensure_payload_object(value: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value.as_object_mut().expect("value was forced to object")
}

fn event_activity_hint(event_type: EventType) -> Option<SessionActivity> {
    match event_type {
        EventType::InputQueued => Some(SessionActivity::Queued),
        EventType::InputAccepted
        | EventType::InputConsumed
        | EventType::ActionRequested
        | EventType::ModelRequested
        | EventType::ToolRequested
        | EventType::ToolStarted
        | EventType::CompactionRequested
        | EventType::CompactionCompleted
        | EventType::CompactionError => Some(SessionActivity::Running),
        EventType::SessionIdle => Some(SessionActivity::Idle),
        _ => None,
    }
}

pub(super) async fn insert_session_event_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    event: &SessionEvent,
    state: Option<&SessionEventState>,
    entries_by_id: &HashMap<String, TranscriptEntryRecord>,
    action_rows: &HashMap<ActionKey, String>,
) -> Result<Vec<EventFrame>> {
    match event {
        SessionEvent::TranscriptItemAppended { entry_id, item } => {
            insert_transcript_item_events_tx(
                tx,
                session_id,
                state,
                entries_by_id.get(entry_id.as_str()),
                entry_id,
                item,
            )
            .await
        }
        SessionEvent::ActionRequested {
            action: SessionAction::CancelSessionWork,
        } => Ok(vec![
            insert_event_with_activity_tx(
                tx,
                session_id,
                EventType::SessionWorkCancelled,
                json!({}),
            )
            .await?,
        ]),
        SessionEvent::ActionRequested { action } => {
            let (kind, action_id, _, payload) = action_payload(action)?;
            let row_id = action_rows.get(&ActionKey::new(kind, action_id)).cloned();
            let mut frames = vec![insert_event_with_activity_tx(
                tx,
                session_id,
                EventType::ActionRequested,
                json!({ "kind": kind, "action_id": action_id, "action_row_id": row_id, "payload": payload }),
            )
            .await?];
            let event_name = match action {
                SessionAction::RequestModel { .. } => Some(EventType::ModelRequested),
                SessionAction::RequestTool { .. } => Some(EventType::ToolRequested),
                SessionAction::CancelSessionWork => None,
            };
            if let Some(event_name) = event_name {
                frames.push(
                    insert_event_with_activity_tx(
                        tx,
                        session_id,
                        event_name,
                        json!({ "action_row_id": row_id, "action_id": action_id }),
                    )
                    .await?,
                );
            }
            Ok(frames)
        }
        SessionEvent::ActionCompleted { kind, id } => {
            let event_name = match kind {
                SessionActionKind::Model => EventType::ModelCompleted,
                SessionActionKind::Tool => EventType::ToolCompleted,
            };
            Ok(vec![
                insert_event_with_activity_tx(
                    tx,
                    session_id,
                    event_name,
                    json!({ "action_id": id }),
                )
                .await?,
            ])
        }
        SessionEvent::ActionFailed { kind, id, error } => {
            let event_name = match kind {
                SessionActionKind::Model => EventType::ModelError,
                SessionActionKind::Tool => EventType::ToolError,
            };
            Ok(vec![
                insert_event_with_activity_tx(
                    tx,
                    session_id,
                    event_name,
                    json!({ "action_id": id, "error": error }),
                )
                .await?,
            ])
        }
    }
}

pub(super) async fn insert_transcript_item_events_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    state: Option<&SessionEventState>,
    entry: Option<&TranscriptEntryRecord>,
    entry_id: &str,
    item: &TranscriptItem,
) -> Result<Vec<EventFrame>> {
    let fallback_state;
    let state = if let Some(state) = state {
        state
    } else {
        fallback_state = session_state_for_event_tx(tx, session_id).await?;
        &fallback_state
    };
    let fallback_record;
    let record = if let Some(entry) = entry {
        Some(entry)
    } else {
        fallback_record =
            transcript_entry_record_tx(tx, session_id, entry_id, TranscriptEntryBodyMode::Ui)
                .await?;
        fallback_record.as_ref()
    };
    let entry_payload = record.as_ref().map(|entry| {
        json!({
            "id": entry.id,
            "parent_id": entry.parent_id,
            "timestamp_ms": entry.timestamp_ms,
            "sequence": entry.sequence,
            "item": entry.item,
        })
    });
    let tree_node = record.map(tree_node_from_entry);
    let mut frames = vec![
        insert_event_with_activity_tx(
            tx,
            session_id,
            EventType::TranscriptAppended,
            json!({
                "entry_id": entry_id,
                "item": item,
                "entry": entry_payload,
                "tree_node": tree_node,
                "active_leaf_id": state.active_leaf_id,
                "session_revision": state.session_revision,
                "queue_revision": state.queue_revision,
                "transcript_revision": state.transcript_revision,
            }),
        )
        .await?,
    ];
    match item {
        TranscriptItem::TurnStarted { turn_id } => {
            frames.push(
                insert_event_with_activity_tx(
                    tx,
                    session_id,
                    EventType::TurnStarted,
                    json!({ "turn_id": turn_id.0, "entry_id": entry_id }),
                )
                .await?,
            );
        }
        TranscriptItem::TurnFinished { turn_id, outcome } => {
            frames.push(
                insert_event_with_activity_tx(
                    tx,
                    session_id,
                    EventType::TurnFinished,
                    json!({ "turn_id": turn_id.0, "outcome": outcome, "entry_id": entry_id }),
                )
                .await?,
            );
        }
        TranscriptItem::AssistantMessage(message) => {
            frames.push(
                insert_event_with_activity_tx(
                    tx,
                    session_id,
                    EventType::AssistantMessage,
                    json!({ "entry_id": entry_id, "assistant": message }),
                )
                .await?,
            );
        }
        _ => {}
    }
    Ok(frames)
}
