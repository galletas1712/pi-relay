use std::collections::HashMap;

use agent_session::{SessionAction, SessionEvent, StoredTranscriptEntry};
use anyhow::{anyhow, Context, Result};
use serde_json::json;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::{ActionStatus, ActionUpdate, EventFrame, EventType, OutputBatch, PersistedAction};

use super::action_records::{action_event_matches_row, action_payload, ActionKey};
use super::events::{insert_event_with_activity_tx, insert_session_event_tx};
use super::rows::row_text;
use super::sql::action_is_unfinished;
use super::transcript::insert_entry_tx;
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn persist_outputs(
        &self,
        session_id: &str,
        batch: OutputBatch<'_>,
    ) -> Result<(Vec<EventFrame>, Vec<PersistedAction>)> {
        let mut tx = self.pool.begin().await?;
        let (frames, dispatch) = persist_outputs_tx(&mut tx, session_id, batch).await?;
        tx.commit().await?;
        Ok((frames, dispatch))
    }
}

pub(super) async fn persist_outputs_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    batch: OutputBatch<'_>,
) -> Result<(Vec<EventFrame>, Vec<PersistedAction>)> {
    let OutputBatch {
        entries,
        active_leaf_id,
        session_events,
        actions,
        action_update,
        consumed_input,
        accepted_input,
    } = batch;

    for entry in entries {
        insert_entry_tx(tx, session_id, entry)
            .await
            .with_context(|| format!("insert transcript entry {}", entry.id))?;
    }
    sqlx::query("update sessions set active_leaf_id=$2::text, updated_at=now() where id=$1")
        .bind(session_id)
        .bind(active_leaf_id)
        .execute(&mut **tx)
        .await
        .context("update session active leaf")?;

    let mut frames = Vec::new();
    if let Some(input) = consumed_input {
        let updated = sqlx::query(
            r#"
                update queued_inputs
                set status='consumed',
                    origin=coalesce(origin, '{}'::jsonb)
                        || jsonb_build_object('consumed_at', now()::text)
                where id=$1
                    and session_id=$2::text
                    and status='consuming'
                    and origin->>'claim_id'=$3
                "#,
        )
        .bind(&input.id)
        .bind(session_id)
        .bind(&input.claim_id)
        .execute(&mut **tx)
        .await
        .context("mark queued input consumed")?
        .rows_affected();
        if updated != 1 {
            return Err(anyhow!("queued input was already consumed: {}", input.id));
        }
        frames.push(
            insert_event_with_activity_tx(
                tx,
                session_id,
                EventType::InputConsumed,
                json!({
                    "input_id": input.id,
                    "priority": input.priority,
                    "client_input_id": input.client_input_id,
                }),
            )
            .await
            .context("insert input.consumed event")?,
        );
    }

    if let Some(input) = accepted_input {
        let mut input_id = None;
        if let Some(client_input_id) = input.client_input_id.as_deref() {
            let id = format!("input_{}", Uuid::new_v4());
            let inserted = sqlx::query(
                r#"
                    insert into queued_inputs (id, session_id, priority, content, status, client_input_id)
                    values ($1, $2, $3, $4, 'consumed', $5)
                    on conflict (session_id, client_input_id) where client_input_id is not null
                    do nothing
                    returning id
                    "#,
            )
            .bind(&id)
            .bind(session_id)
            .bind(input.priority.as_str())
            .bind(serde_json::to_value(&input.content)?)
            .bind(client_input_id)
            .fetch_optional(&mut **tx)
            .await
            .context("record accepted input")?;
            let Some(row) = inserted else {
                return Err(anyhow!("input already recorded: {client_input_id}"));
            };
            input_id = Some(row.get::<String, _>("id"));
        }

        frames.push(
            insert_event_with_activity_tx(
                tx,
                session_id,
                EventType::InputAccepted,
                json!({
                    "input_id": input_id,
                    "priority": input.priority,
                    "client_input_id": input.client_input_id,
                    "content": input.content,
                }),
            )
            .await
            .context("insert input.accepted event")?,
        );
    }

    if let Some(mut update) = action_update {
        complete_action_tx(tx, session_id, &mut update, session_events).await?;
    }

    let mut action_rows = HashMap::<ActionKey, String>::new();
    let mut dispatch = Vec::new();
    for action in actions {
        if matches!(action, SessionAction::CancelSessionWork) {
            let unfinished_actions = action_is_unfinished(None);
            let query = format!(
                r#"
                update actions
                set status='interrupted',
                    result='{{"reason":"session interrupted"}}'::jsonb,
                    updated_at=now()
                where session_id=$1 and {unfinished_actions}
                "#
            );
            sqlx::query(&query)
                .bind(session_id)
                .execute(&mut **tx)
                .await
                .context("mark session work interrupted")?;
            continue;
        }

        let (kind, action_id, turn_id, payload) = action_payload(action)?;
        let row_id = format!("action_{}", Uuid::new_v4());
        let attempt_id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            insert into actions (id, session_id, turn_id, action_id, attempt_id, kind, status, payload)
            values ($1::text, $2::text, $3::bigint, $4, $5::text, $6::text, 'running', $7)
            "#,
        )
        .bind(&row_id)
        .bind(session_id)
        .bind(turn_id)
        .bind(action_id)
        .bind(&attempt_id)
        .bind(kind.as_str())
        .bind(&payload)
        .execute(&mut **tx)
        .await
        .context("insert action row")?;
        action_rows.insert(ActionKey::new(kind, action_id), row_id.clone());
        dispatch.push(PersistedAction {
            row_id,
            attempt_id,
            action: action.clone(),
        });
    }

    let entries_by_id = entries
        .iter()
        .map(|entry| {
            (
                entry.id.as_str(),
                StoredTranscriptEntry {
                    id: entry.id.clone(),
                    parent_id: entry.parent_id.clone(),
                    timestamp_ms: entry.timestamp_ms,
                    item: entry.item.clone(),
                    provider_replay: entry.provider_replay.clone(),
                },
            )
        })
        .collect::<HashMap<_, _>>();
    for event in session_events {
        frames.extend(
            insert_session_event_tx(tx, session_id, event, &entries_by_id, &action_rows)
                .await
                .with_context(|| format!("insert session event {event:?}"))?,
        );
    }
    Ok((frames, dispatch))
}

async fn complete_action_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    update: &mut ActionUpdate,
    session_events: &[SessionEvent],
) -> Result<()> {
    let unfinished_actions = action_is_unfinished(None);
    let select_query = format!(
        r#"
            select kind, action_id
            from actions
            where session_id=$1 and id=$2::text and attempt_id=$3::text and {unfinished_actions}
            "#
    );
    if let Some(row) = sqlx::query(&select_query)
        .bind(session_id)
        .bind(&update.row_id)
        .bind(&update.attempt_id)
        .fetch_optional(&mut **tx)
        .await
        .context("load action row for completion")?
    {
        let row_kind = row_text(&row, "kind")?;
        let row_action_id: i64 = row.get("action_id");
        for event in session_events {
            match event {
                SessionEvent::ActionCompleted { kind, id }
                    if action_event_matches_row(row_kind, row_action_id, kind, id) =>
                {
                    update.status = ActionStatus::Completed;
                }
                SessionEvent::ActionFailed { kind, id, error }
                    if action_event_matches_row(row_kind, row_action_id, kind, id) =>
                {
                    update.status = ActionStatus::Error;
                    update.result = json!({ "error": error });
                }
                _ => {}
            }
        }
    }

    let unfinished_actions = action_is_unfinished(None);
    let update_query = format!(
        r#"
            update actions
            set status=$4, result=$5, updated_at=now()
            where session_id=$1 and id=$2::text and attempt_id=$3::text and {unfinished_actions}
            "#
    );
    let updated = sqlx::query(&update_query)
        .bind(session_id)
        .bind(&update.row_id)
        .bind(&update.attempt_id)
        .bind(update.status.as_str())
        .bind(&update.result)
        .execute(&mut **tx)
        .await
        .context("update completed action row")?
        .rows_affected();
    if updated != 1 {
        return Err(anyhow!(
            "action attempt is no longer running: {}",
            update.row_id
        ));
    }
    Ok(())
}
