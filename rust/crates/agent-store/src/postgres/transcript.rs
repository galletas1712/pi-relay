use std::collections::BTreeMap;

use agent_session::{StoredSession, StoredTranscriptEntry, TranscriptStorageNode};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sqlx::{Postgres, Row, Transaction};

use crate::{EventFrame, EventType, HistoryTree, SessionConfig};

use super::events::{insert_event_tx, insert_transcript_item_events_tx};
use super::rows::row_to_stored_entry;
use super::sql::action_is_unfinished;
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn load_stored_session(&self, session_id: &str) -> Result<StoredSession> {
        let session_row = sqlx::query("select active_leaf_id, metadata from sessions where id=$1")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let rows = sqlx::query(
            "select id, parent_id, timestamp_ms, item from transcript_entries where session_id=$1 order by sequence",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        let mut metadata = BTreeMap::new();
        if let Value::Object(map) = session_row.get::<Value, _>("metadata") {
            for (key, value) in map {
                if let Some(value) = value.as_str() {
                    metadata.insert(key, value.to_string());
                }
            }
        }
        Ok(StoredSession {
            session_id: session_id.to_string(),
            active_leaf_id: session_row.get("active_leaf_id"),
            metadata,
            entries: rows
                .into_iter()
                .map(|row| row_to_stored_entry(&row))
                .collect::<Result<Vec<_>>>()?,
        })
    }

    pub async fn history_tree(&self, session_id: &str) -> Result<HistoryTree> {
        let stored = self.load_stored_session(session_id).await?;
        Ok(HistoryTree {
            session_id: session_id.to_string(),
            active_leaf_id: stored.active_leaf_id,
            entries: stored.entries,
        })
    }

    pub async fn set_active_leaf(
        &self,
        session_id: &str,
        leaf_id: Option<&str>,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        if let Some(leaf_id) = leaf_id {
            let belongs_to_session: bool = sqlx::query_scalar(
                "select exists(select 1 from transcript_entries where session_id=$1 and id=$2::text)",
            )
            .bind(session_id)
            .bind(leaf_id)
            .fetch_one(&mut *tx)
            .await?;
            if !belongs_to_session {
                return Err(anyhow!("active leaf does not belong to session: {leaf_id}"));
            }
        }
        sqlx::query("update sessions set active_leaf_id=$2::text, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(leaf_id)
            .execute(&mut *tx)
            .await?;
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::HistoryRewound,
            json!({ "active_leaf_id": leaf_id }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn create_fork(
        &self,
        source_session_id: &str,
        new_session_id: &str,
        config: &SessionConfig,
        entries: &[TranscriptStorageNode],
        target_leaf_id: &str,
        active_leaf_id: Option<String>,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        let mut metadata = config.metadata.clone();
        if let Some(metadata) = metadata.as_object_mut() {
            metadata.insert(
                "fork".to_string(),
                json!({
                    "source_session_id": source_session_id,
                    "source_leaf_id": target_leaf_id,
                    "active_leaf_id": active_leaf_id,
                }),
            );
        } else {
            metadata = json!({
                "fork": {
                    "source_session_id": source_session_id,
                    "source_leaf_id": target_leaf_id,
                    "active_leaf_id": active_leaf_id,
                },
                "source_metadata": config.metadata.clone(),
            });
        }
        sqlx::query(
            "insert into sessions (id, active_leaf_id, provider_config, metadata) values ($1, $2::text, $3, $4)",
        )
        .bind(new_session_id)
        .bind(active_leaf_id.as_deref())
        .bind(serde_json::to_value(&config.provider)?)
        .bind(&metadata)
        .execute(&mut *tx)
        .await?;
        for entry in entries {
            insert_entry_tx(&mut tx, new_session_id, entry).await?;
        }
        let event = insert_event_tx(
            &mut tx,
            source_session_id,
            EventType::HistoryForked,
            json!({
                "new_session_id": new_session_id,
                "leaf_id": target_leaf_id,
                "active_leaf_id": active_leaf_id,
            }),
        )
        .await?;
        let created = insert_event_tx(
            &mut tx,
            new_session_id,
            EventType::SessionCreated,
            json!({
                "session_id": new_session_id,
                "forked_from": source_session_id,
                "source_leaf_id": target_leaf_id,
                "active_leaf_id": active_leaf_id,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event, created])
    }

    pub async fn recover_session(
        &self,
        session_id: &str,
        entries: &[StoredTranscriptEntry],
        active_leaf_id: Option<&str>,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        let mut frames = Vec::new();
        for entry in entries {
            insert_stored_entry_tx(&mut tx, session_id, entry).await?;
            frames.extend(
                insert_transcript_item_events_tx(&mut tx, session_id, &entry.id, &entry.item)
                    .await?,
            );
        }
        let unfinished_actions = action_is_unfinished(None);
        let query = format!(
            "update actions set status='stale', updated_at=now() where session_id=$1 and {unfinished_actions}",
        );
        sqlx::query(&query)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("update sessions set active_leaf_id=$2::text, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(active_leaf_id)
            .execute(&mut *tx)
            .await?;
        frames.push(
            insert_event_tx(
                &mut tx,
                session_id,
                EventType::SessionRecovered,
                json!({ "active_leaf_id": active_leaf_id }),
            )
            .await?,
        );
        tx.commit().await?;
        Ok(frames)
    }
}

pub(super) async fn insert_entry_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry: &TranscriptStorageNode,
) -> Result<()> {
    let stored = StoredTranscriptEntry {
        id: entry.id.clone(),
        parent_id: entry.parent_id.clone(),
        timestamp_ms: entry.timestamp_ms,
        item: entry.item.clone(),
    };
    insert_stored_entry_tx(tx, session_id, &stored).await
}

pub(super) async fn insert_stored_entry_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry: &StoredTranscriptEntry,
) -> Result<()> {
    let turn_id = entry.item.turn_id().map(|turn_id| turn_id.0 as i64);
    sqlx::query(
        r#"
        insert into transcript_entries (session_id, id, parent_id, timestamp_ms, item, turn_id)
        values ($1::text, $2::text, $3::text, $4, $5, $6::bigint)
        on conflict (session_id, id) do nothing
        "#,
    )
    .bind(session_id)
    .bind(&entry.id)
    .bind(&entry.parent_id)
    .bind(entry.timestamp_ms as i64)
    .bind(serde_json::to_value(&entry.item)?)
    .bind(turn_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
