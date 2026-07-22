use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use sqlx::{Postgres, Row, Transaction};

use crate::{InputPriority, QueueState, QueuedInputContent, QueuedInputRecord, QueuedInputStatus};

use super::rows::row_text;
use super::sql::{queued_input_is_active, session_activity, QUEUED_INPUT_DISPATCH_ORDER};
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn queue_state(&self, session_id: &str) -> Result<QueueState> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read read only")
            .execute(&mut *tx)
            .await?;
        let queue = queue_state_tx(&mut tx, session_id).await?;
        tx.commit().await?;
        Ok(queue)
    }
}

pub(super) async fn queue_state_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
) -> Result<QueueState> {
    let session = sqlx::query(
        r#"
            select session_revision, queue_revision, transcript_revision
            from sessions
            where id=$1
            "#,
    )
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
    let active_queue = queued_input_is_active(None);
    let queued_inputs_query = format!(
        r#"
            select id,
                priority,
                status,
                content,
                client_input_id,
                created_at::text as created_at,
                updated_at::text as updated_at,
                origin->>'promoted_at' as promoted_at,
                follow_up_position
            from queued_inputs
            where session_id=$1 and {active_queue}
            order by {QUEUED_INPUT_DISPATCH_ORDER}
            "#
    );
    let queued_rows = sqlx::query(&queued_inputs_query)
        .bind(session_id)
        .fetch_all(&mut **tx)
        .await?;
    let unfinished_actions = sqlx::query_scalar::<_, bool>(
        "select exists(select 1 from actions where session_id=$1 and status in ('pending','blocked','running'))",
    )
    .bind(session_id)
    .fetch_one(&mut **tx)
    .await?;
    let activity = session_activity(unfinished_actions, !queued_rows.is_empty());
    Ok(QueueState {
        session_revision: session.get("session_revision"),
        queue_revision: session.get("queue_revision"),
        transcript_revision: session.get("transcript_revision"),
        activity,
        queued_inputs: queued_rows
            .into_iter()
            .map(|row| {
                let content_value = row.get::<Value, _>("content");
                Ok(QueuedInputRecord {
                    input_id: row.get("id"),
                    priority: row_text(&row, "priority")?,
                    status: row_text::<QueuedInputStatus>(&row, "status")?,
                    content: queued_input_content_from_value(content_value)?,
                    client_input_id: row.get("client_input_id"),
                    created_at: row.get("created_at"),
                    updated_at: row.get("updated_at"),
                    promoted_at: row.get("promoted_at"),
                    follow_up_position: row.get("follow_up_position"),
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

pub(super) fn queue_state_payload(queue: &QueueState) -> Value {
    let queued_inputs = queue
        .queued_inputs
        .iter()
        .map(queued_input_value)
        .collect::<Vec<_>>();
    json!({
        "session_revision": queue.session_revision,
        "queue_revision": queue.queue_revision,
        "transcript_revision": queue.transcript_revision,
        "activity": queue.activity,
        "queued_inputs": queued_inputs,
    })
}

pub(super) fn queue_event_payload(queue: &QueueState, mut extra: Value) -> Value {
    let mut payload = match queue_state_payload(queue) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    if let Value::Object(extra) = &mut extra {
        payload.append(extra);
    }
    Value::Object(payload)
}

pub(super) async fn bump_revisions_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    queue_changed: bool,
    transcript_changed: bool,
) -> Result<()> {
    sqlx::query(
        r#"
            update sessions
            set session_revision=session_revision + 1,
                queue_revision=queue_revision + $2::bigint,
                transcript_revision=transcript_revision + $3::bigint,
                updated_at=now()
            where id=$1
            "#,
    )
    .bind(session_id)
    .bind(if queue_changed { 1_i64 } else { 0_i64 })
    .bind(if transcript_changed { 1_i64 } else { 0_i64 })
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) fn queued_input_value(input: &QueuedInputRecord) -> Value {
    let (content, editable) = match &input.content {
        QueuedInputContent::UserMessage(message) => (json!(message.content.clone()), true),
        QueuedInputContent::DaemonToolObservation(_) | QueuedInputContent::SubagentControl => {
            (json!([]), false)
        }
    };
    json!({
        "input_id": input.input_id,
        "priority": input.priority,
        "status": input.status,
        "content": content,
        "content_type": input.content.as_kind(),
        "editable": editable,
        "client_input_id": input.client_input_id,
        "created_at": input.created_at,
        "updated_at": input.updated_at,
        "promoted_at": input.promoted_at,
        "follow_up_position": input.follow_up_position,
    })
}

pub(super) fn queued_input_content_from_value(value: Value) -> Result<QueuedInputContent> {
    if value.get("type").and_then(Value::as_str).is_some() {
        Ok(serde_json::from_value(value)?)
    } else {
        Ok(QueuedInputContent::user_message(serde_json::from_value(
            value,
        )?))
    }
}

pub(super) fn append_queued_content_event_fields(
    payload: &mut Value,
    content: &QueuedInputContent,
) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    object.insert(
        "content_type".to_string(),
        Value::String(content.as_kind().to_string()),
    );
    match content {
        QueuedInputContent::UserMessage(message) => {
            object.insert("content".to_string(), json!(message.content.clone()));
            object.insert("editable".to_string(), Value::Bool(true));
        }
        QueuedInputContent::DaemonToolObservation(_) => {
            object.insert("content".to_string(), json!([]));
            object.insert("editable".to_string(), Value::Bool(false));
        }
        QueuedInputContent::SubagentControl => {
            object.insert("content".to_string(), json!([]));
            object.insert("editable".to_string(), Value::Bool(false));
        }
    }
}

pub(super) async fn revision_mismatch_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    expected_queue_revision: Option<i64>,
) -> Result<bool> {
    let Some(expected) = expected_queue_revision else {
        return Ok(false);
    };
    let current: i64 = sqlx::query_scalar("select queue_revision from sessions where id=$1")
        .bind(session_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(current != expected)
}

pub(super) async fn queued_follow_up_ids_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
) -> Result<Vec<String>> {
    let rows = sqlx::query(
        r#"
            select id
            from queued_inputs
            where session_id=$1
                and priority='follow_up'
                and status='queued'
            order by follow_up_position nulls last, created_at, id
            "#,
    )
    .bind(session_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().map(|row| row.get("id")).collect())
}

pub(super) fn queued_follow_up_ids_from_state(queue: &QueueState) -> Vec<String> {
    queue
        .queued_inputs
        .iter()
        .filter(|input| {
            input.priority == InputPriority::FollowUp && input.status == QueuedInputStatus::Queued
        })
        .map(|input| input.input_id.clone())
        .collect()
}

pub(super) async fn renumber_follow_ups_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
) -> Result<()> {
    let ids = queued_follow_up_ids_tx(tx, session_id).await?;
    for (position, input_id) in ids.iter().enumerate() {
        sqlx::query(
            r#"
                update queued_inputs
                set follow_up_position=$3,
                    updated_at=now()
                where session_id=$1
                    and id=$2::text
                    and follow_up_position is distinct from $3
                "#,
        )
        .bind(session_id)
        .bind(input_id)
        .bind(position as i32)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}
