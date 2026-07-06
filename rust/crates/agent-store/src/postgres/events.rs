use std::collections::HashMap;
#[cfg(test)]
use std::{cell::Cell, future::Future};

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

// Three fixed binds (session ID plus type and payload arrays) stay far below
// PostgreSQL's bind limit. The cap bounds accumulated event JSON per statement.
pub(super) const EVENT_INSERT_BATCH_CAPACITY: usize = 128;

const INSERT_EVENT_ROWS_SQL: &str = r#"
with input as materialized (
    select event_type, payload, input_ordinal
    from unnest($2::text[], $3::jsonb[])
        with ordinality as input(event_type, payload, input_ordinal)
),
allocated as materialized (
    select
        nextval(pg_get_serial_sequence('events', 'id')::regclass) as id,
        input_ordinal
    from (
        select input_ordinal
        from input
        order by input_ordinal
    ) ordered_input
),
inserted as (
    insert into events (id, session_id, type, payload)
    select allocated.id, $1::text, input.event_type, input.payload
    from allocated
    join input using (input_ordinal)
    returning id, session_id, type, payload
)
select inserted.id, inserted.session_id, inserted.type, inserted.payload
from inserted
join allocated using (id)
order by allocated.input_ordinal
"#;

#[derive(Debug)]
pub(super) struct EventRow {
    event_type: EventType,
    payload: Value,
}

impl EventRow {
    pub(super) fn new(event_type: EventType, payload: Value) -> Self {
        Self {
            event_type,
            payload,
        }
    }

    pub(super) fn with_activity_hint(event_type: EventType, mut payload: Value) -> Self {
        if let Some(activity) = event_activity_hint(event_type) {
            ensure_payload_object(&mut payload)
                .entry("activity".to_string())
                .or_insert_with(|| serde_json::to_value(activity).unwrap_or(Value::Null));
        }
        Self::new(event_type, payload)
    }
}

#[cfg(test)]
tokio::task_local! {
    static EVENT_INSERT_STATEMENTS: Cell<usize>;
}

#[cfg(test)]
pub(super) async fn with_event_insert_statement_count<F>(future: F) -> (F::Output, usize)
where
    F: Future,
{
    EVENT_INSERT_STATEMENTS
        .scope(Cell::new(0), async {
            let output = future.await;
            (output, EVENT_INSERT_STATEMENTS.with(Cell::get))
        })
        .await
}

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
        Ok(
            insert_event_row_batch(&self.pool, session_id, vec![EventRow::new(event, data)])
                .await?
                .pop()
                .expect("one input event returns one frame"),
        )
    }

    pub async fn insert_events(
        &self,
        session_id: &str,
        events: Vec<(EventType, Value)>,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        let rows = events
            .into_iter()
            .map(|(event_type, payload)| EventRow::new(event_type, payload))
            .collect::<Vec<_>>();
        let frames = insert_event_rows_tx(&mut tx, session_id, rows).await?;
        tx.commit().await?;
        Ok(frames)
    }

    /// Parent-visible
    /// `subagent.idle`. Delegation members do not call this on terminal completion;
    /// they use `claim_subagent_idle_once` below to fire cleanup/barrier work
    /// without surfacing a per-child idle event to the parent.
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
    /// uses), so a delegation member's barrier + RO-snapshot destroy still fire
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
    Ok(
        insert_event_rows_tx(tx, session_id, vec![EventRow::new(event_type, payload)])
            .await?
            .pop()
            .expect("one input event returns one frame"),
    )
}

pub(super) async fn insert_event_with_activity_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    event_type: EventType,
    payload: Value,
) -> Result<EventFrame> {
    Ok(insert_event_rows_tx(
        tx,
        session_id,
        vec![EventRow::with_activity_hint(event_type, payload)],
    )
    .await?
    .pop()
    .expect("one input event returns one frame"))
}

pub(super) async fn insert_event_rows_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    event_rows: Vec<EventRow>,
) -> Result<Vec<EventFrame>> {
    let mut frames = Vec::with_capacity(event_rows.len());
    let mut event_rows = event_rows.into_iter();
    loop {
        let batch = event_rows
            .by_ref()
            .take(EVENT_INSERT_BATCH_CAPACITY)
            .collect::<Vec<_>>();
        if batch.is_empty() {
            break;
        }
        frames.extend(insert_event_row_batch(&mut **tx, session_id, batch).await?);
    }
    Ok(frames)
}

async fn insert_event_row_batch<'e, E>(
    executor: E,
    session_id: &str,
    event_rows: Vec<EventRow>,
) -> Result<Vec<EventFrame>>
where
    E: Executor<'e, Database = Postgres>,
{
    if event_rows.is_empty() {
        return Ok(Vec::new());
    }
    let (event_types, payloads) = event_rows
        .into_iter()
        .map(|row| (row.event_type.as_str(), row.payload))
        .unzip::<_, _, Vec<_>, Vec<_>>();
    #[cfg(test)]
    let _ = EVENT_INSERT_STATEMENTS.try_with(|count| count.set(count.get() + 1));
    let rows = sqlx::query(INSERT_EVENT_ROWS_SQL)
        .bind(session_id)
        .bind(event_types)
        .bind(payloads)
        .fetch_all(executor)
        .await?;
    rows.into_iter().map(row_to_event).collect()
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

#[allow(dead_code)]
pub(super) async fn insert_session_event_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    event: &SessionEvent,
    state: Option<&SessionEventState>,
    entries_by_id: &HashMap<String, TranscriptEntryRecord>,
    action_rows: &HashMap<ActionKey, String>,
) -> Result<Vec<EventFrame>> {
    let rows =
        session_event_rows_tx(tx, session_id, event, state, entries_by_id, action_rows).await?;
    insert_event_rows_tx(tx, session_id, rows).await
}

pub(super) async fn session_event_rows_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    event: &SessionEvent,
    state: Option<&SessionEventState>,
    entries_by_id: &HashMap<String, TranscriptEntryRecord>,
    action_rows: &HashMap<ActionKey, String>,
) -> Result<Vec<EventRow>> {
    match event {
        SessionEvent::TranscriptItemAppended { entry_id, item } => {
            transcript_item_event_rows_tx(
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
        } => Ok(vec![EventRow::with_activity_hint(
            EventType::SessionWorkCancelled,
            json!({}),
        )]),
        SessionEvent::ActionRequested { action } => {
            let (kind, action_id, _, payload) = action_payload(action)?;
            let row_id = action_rows.get(&ActionKey::new(kind, action_id)).cloned();
            let mut rows = vec![EventRow::with_activity_hint(
                EventType::ActionRequested,
                json!({ "kind": kind, "action_id": action_id, "action_row_id": row_id, "payload": payload }),
            )];
            let event_name = match action {
                SessionAction::RequestModel { .. } => Some(EventType::ModelRequested),
                SessionAction::RequestTool { .. } => Some(EventType::ToolRequested),
                SessionAction::CancelSessionWork => None,
            };
            if let Some(event_name) = event_name {
                rows.push(EventRow::with_activity_hint(
                    event_name,
                    json!({ "action_row_id": row_id, "action_id": action_id }),
                ));
            }
            Ok(rows)
        }
        SessionEvent::ActionCompleted { kind, id } => {
            let event_name = match kind {
                SessionActionKind::Model => EventType::ModelCompleted,
                SessionActionKind::Tool => EventType::ToolCompleted,
            };
            Ok(vec![EventRow::with_activity_hint(
                event_name,
                json!({ "action_id": id }),
            )])
        }
        SessionEvent::ActionFailed { kind, id, error } => {
            let event_name = match kind {
                SessionActionKind::Model => EventType::ModelError,
                SessionActionKind::Tool => EventType::ToolError,
            };
            Ok(vec![EventRow::with_activity_hint(
                event_name,
                json!({ "action_id": id, "error": error }),
            )])
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
    let rows = transcript_item_event_rows_tx(tx, session_id, state, entry, entry_id, item).await?;
    insert_event_rows_tx(tx, session_id, rows).await
}

async fn transcript_item_event_rows_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    state: Option<&SessionEventState>,
    entry: Option<&TranscriptEntryRecord>,
    entry_id: &str,
    item: &TranscriptItem,
) -> Result<Vec<EventRow>> {
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
    let mut rows = vec![EventRow::with_activity_hint(
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
    )];
    match item {
        TranscriptItem::TurnStarted { turn_id } => {
            rows.push(EventRow::with_activity_hint(
                EventType::TurnStarted,
                json!({ "turn_id": turn_id.0, "entry_id": entry_id }),
            ));
        }
        TranscriptItem::TurnFinished { turn_id, outcome } => {
            rows.push(EventRow::with_activity_hint(
                EventType::TurnFinished,
                json!({ "turn_id": turn_id.0, "outcome": outcome, "entry_id": entry_id }),
            ));
        }
        TranscriptItem::AssistantMessage(message) => {
            rows.push(EventRow::with_activity_hint(
                EventType::AssistantMessage,
                json!({ "entry_id": entry_id, "assistant": message }),
            ));
        }
        _ => {}
    }
    Ok(rows)
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
