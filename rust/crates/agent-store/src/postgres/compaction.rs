use std::time::{SystemTime, UNIX_EPOCH};

use agent_session::{StoredTranscriptEntry, TranscriptStore};
use agent_vocab::{CompactionSummary, TranscriptItem};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    ActionKind, ActionStatus, CompactionCompletion, CompactionJob, CompactionTrigger,
    CompleteCompactionResult, CreateCompactionResult, EventFrame, EventType,
};

use super::events::{
    insert_event_tx, insert_event_with_activity_tx, insert_transcript_item_events_tx,
};
use super::rows::row_to_stored_entry;
use super::sql::action_is_unfinished;
use super::transcript::insert_stored_entry_tx;
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn create_compaction_action(
        &self,
        session_id: &str,
        trigger: CompactionTrigger,
    ) -> Result<CreateCompactionResult> {
        let mut tx = self.pool.begin().await?;
        let active_leaf_id: Option<String> =
            sqlx::query_scalar("select active_leaf_id from sessions where id=$1 for update")
                .bind(session_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let source_leaf_id =
            active_leaf_id.ok_or_else(|| anyhow!("cannot compact an empty session"))?;
        let rows = sqlx::query(
            "select id, parent_id, timestamp_ms, item, provider_replay from transcript_entries where session_id=$1 order by sequence",
        )
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await?;
        let entries = rows
            .into_iter()
            .map(|row| row_to_stored_entry(&row))
            .collect::<Result<Vec<_>>>()?;
        let store = TranscriptStore::from_storage_entries(
            entries.into_iter().map(Into::into).collect(),
            Some(source_leaf_id.clone()),
        )
        .map_err(|error| anyhow!("invalid transcript store: {error:?}"))?;
        if !store.is_turn_boundary() {
            return Err(anyhow!("compaction source is not at a turn boundary"));
        }
        let model_context = store.model_context();
        if model_context.transcript_items().is_empty() {
            return Err(anyhow!("cannot compact an empty session"));
        }
        let source_entry = store
            .get_entry(&source_leaf_id)
            .ok_or_else(|| anyhow!("active transcript entry not found: {source_leaf_id}"))?;
        if matches!(source_entry.item, TranscriptItem::CompactionSummary(_)) {
            return Err(anyhow!(
                "active leaf is already a compaction summary; add a new turn before compacting again"
            ));
        }
        let last_turn_id = model_context.last_turn_id();
        let tokens_before = latest_context_usage_tx(&mut tx, session_id, &source_leaf_id).await?;
        let trigger_name = trigger.as_str();
        let reason = trigger.reason().map(str::to_string);
        let action_row_id = format!("action_{}", Uuid::new_v4());
        let attempt_id = Uuid::new_v4().to_string();
        let payload = json!({
            "source_session_id": session_id,
            "source_leaf_id": source_leaf_id,
            "last_turn_id": last_turn_id.0,
            "context_tokens": tokens_before,
            "trigger": trigger_name,
            "reason": reason,
        });
        sqlx::query(
            r#"
            insert into actions (id, session_id, turn_id, action_id, attempt_id, kind, status, payload)
            values ($1::text, $2::text, null, 0, $3::text, $4::text, 'running', $5)
            "#,
        )
        .bind(&action_row_id)
        .bind(session_id)
        .bind(&attempt_id)
        .bind(ActionKind::Compaction.as_str())
        .bind(&payload)
        .execute(&mut *tx)
        .await?;

        let events = vec![
            insert_event_with_activity_tx(
                &mut tx,
                session_id,
                EventType::ActionRequested,
                json!({
                    "kind": ActionKind::Compaction,
                    "action_id": 0,
                    "action_row_id": action_row_id,
                    "payload": payload,
                }),
            )
            .await?,
            insert_event_with_activity_tx(
                &mut tx,
                session_id,
                EventType::CompactionRequested,
                json!({
                    "action_row_id": action_row_id,
                    "source_session_id": session_id,
                    "source_leaf_id": source_leaf_id,
                    "trigger": trigger_name,
                    "reason": reason,
                }),
            )
            .await?,
        ];
        tx.commit().await?;

        Ok(CreateCompactionResult {
            job: CompactionJob {
                action_row_id,
                attempt_id,
                source_session_id: session_id.to_string(),
                source_leaf_id,
                model_context,
                tokens_before,
                last_turn_id,
                trigger,
                reason,
            },
            events,
        })
    }

    pub async fn complete_compaction_action(
        &self,
        job: &CompactionJob,
        completion: CompactionCompletion,
    ) -> Result<CompleteCompactionResult> {
        let mut tx = self.pool.begin().await?;
        let unfinished_actions = action_is_unfinished(None);
        let action_query = format!(
            r#"
            select 1
            from actions
            where session_id=$1 and id=$2::text and attempt_id=$3::text
                and kind=$4::text and {unfinished_actions}
            for update
            "#
        );
        if sqlx::query(&action_query)
            .bind(&job.source_session_id)
            .bind(&job.action_row_id)
            .bind(&job.attempt_id)
            .bind(ActionKind::Compaction.as_str())
            .fetch_optional(&mut *tx)
            .await?
            .is_none()
        {
            tx.commit().await?;
            return Ok(CompleteCompactionResult {
                new_root_id: None,
                events: Vec::new(),
            });
        }

        let active_leaf_id: Option<String> =
            sqlx::query_scalar("select active_leaf_id from sessions where id=$1 for update")
                .bind(&job.source_session_id)
                .fetch_one(&mut *tx)
                .await?;
        if active_leaf_id.as_deref() != Some(job.source_leaf_id.as_str()) {
            let events = self
                .mark_compaction_stale_tx(
                    &mut tx,
                    job,
                    "source leaf changed before compaction completed",
                )
                .await?;
            tx.commit().await?;
            return Ok(CompleteCompactionResult {
                new_root_id: None,
                events,
            });
        }

        let new_root_id = format!("entry_{}", Uuid::new_v4());
        let entry = StoredTranscriptEntry {
            id: new_root_id.clone(),
            parent_id: None,
            timestamp_ms: now_ms(),
            item: TranscriptItem::CompactionSummary(CompactionSummary::new(
                job.source_session_id.clone(),
                job.source_leaf_id.clone(),
                completion.summary.clone(),
                job.tokens_before,
                job.last_turn_id,
            )),
            provider_replay: completion.provider_replay.clone(),
        };
        insert_stored_entry_tx(&mut tx, &job.source_session_id, &entry).await?;
        sqlx::query("update sessions set active_leaf_id=$2::text, updated_at=now() where id=$1")
            .bind(&job.source_session_id)
            .bind(&new_root_id)
            .execute(&mut *tx)
            .await?;
        let result_payload = json!({
            "new_root_id": new_root_id,
            "source_session_id": job.source_session_id,
            "source_leaf_id": job.source_leaf_id,
            "trigger": job.trigger.as_str(),
            "reason": job.reason,
            "remote": completion.remote,
            "provider": completion.provider,
            "summary_kind": completion.summary_kind,
            "usage": completion.usage,
            "provider_replay_items": completion.provider_replay.len(),
        });
        let updated = sqlx::query(
            r#"
            update actions
            set status=$4::text,
                result=$5,
                updated_at=now()
            where session_id=$1 and id=$2::text and attempt_id=$3::text
            "#,
        )
        .bind(&job.source_session_id)
        .bind(&job.action_row_id)
        .bind(&job.attempt_id)
        .bind(ActionStatus::Completed.as_str())
        .bind(&result_payload)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            return Err(anyhow!(
                "compaction action attempt was not updated: {}",
                job.action_row_id
            ));
        }

        let mut events = insert_transcript_item_events_tx(
            &mut tx,
            &job.source_session_id,
            Some(&entry),
            &new_root_id,
            &entry.item,
        )
        .await?;
        events.push(
            insert_event_tx(
                &mut tx,
                &job.source_session_id,
                EventType::HistoryCompacted,
                json!({
                    "new_root_id": new_root_id,
                    "source_session_id": job.source_session_id,
                    "source_leaf_id": job.source_leaf_id,
                    "tokens_before": job.tokens_before,
                    "trigger": job.trigger.as_str(),
                    "reason": job.reason,
                    "remote": completion.remote,
                    "provider": completion.provider,
                    "summary_kind": completion.summary_kind,
                }),
            )
            .await?,
        );
        events.push(
            insert_event_with_activity_tx(
                &mut tx,
                &job.source_session_id,
                EventType::CompactionCompleted,
                json!({
                    "action_row_id": job.action_row_id,
                    "new_root_id": new_root_id,
                    "active_leaf_id": new_root_id,
                    "trigger": job.trigger.as_str(),
                    "reason": job.reason,
                    "remote": completion.remote,
                    "provider": completion.provider,
                    "summary_kind": completion.summary_kind,
                }),
            )
            .await?,
        );
        tx.commit().await?;
        Ok(CompleteCompactionResult {
            new_root_id: Some(new_root_id),
            events,
        })
    }

    pub async fn fail_compaction_action(
        &self,
        job: &CompactionJob,
        error: String,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        let events = self
            .finish_compaction_error_tx(&mut tx, job, ActionStatus::Error, error)
            .await?;
        tx.commit().await?;
        Ok(events)
    }

    async fn mark_compaction_stale_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        job: &CompactionJob,
        error: &str,
    ) -> Result<Vec<EventFrame>> {
        self.finish_compaction_error_tx(tx, job, ActionStatus::Stale, error.to_string())
            .await
    }

    async fn finish_compaction_error_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        job: &CompactionJob,
        status: ActionStatus,
        error: String,
    ) -> Result<Vec<EventFrame>> {
        let unfinished_actions = action_is_unfinished(None);
        let update_query = format!(
            r#"
            update actions
            set status=$4::text,
                result=$5,
                updated_at=now()
            where session_id=$1 and id=$2::text and attempt_id=$3::text and {unfinished_actions}
            "#
        );
        let updated = sqlx::query(&update_query)
            .bind(&job.source_session_id)
            .bind(&job.action_row_id)
            .bind(&job.attempt_id)
            .bind(status.as_str())
            .bind(json!({ "error": error }))
            .execute(&mut **tx)
            .await?
            .rows_affected();
        if updated != 1 {
            return Ok(Vec::new());
        }
        Ok(vec![
            insert_event_with_activity_tx(
                tx,
                &job.source_session_id,
                EventType::CompactionError,
                json!({
                    "action_row_id": job.action_row_id,
                    "error": error,
                    "status": status,
                }),
            )
            .await?,
        ])
    }
}

pub(super) async fn latest_context_usage_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    active_leaf_id: &str,
) -> Result<Option<usize>> {
    let row = sqlx::query(
        r#"
        select result
        from actions
        where session_id=$1
            and kind='model'
            and status='completed'
            and payload->>'context_leaf_id'=$2
            and result->'usage'->>'input_tokens' is not null
        order by updated_at desc, created_at desc
        limit 1
        "#,
    )
    .bind(session_id)
    .bind(active_leaf_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.and_then(|row| {
        let result: Value = row.get("result");
        result
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
    }))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}
