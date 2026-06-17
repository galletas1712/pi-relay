use std::time::{SystemTime, UNIX_EPOCH};

use agent_session::{ModelContext, StoredTranscriptEntry, TranscriptStore};
use agent_vocab::{CompactionSummary, TranscriptItem, TurnId};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    ActionKind, ActionStatus, CompactionCompletion, CompactionJob, CompactionScope,
    CompactionTrigger, CompleteCompactionResult, CreateCompactionResult, EventFrame, EventType,
    PersistedAction,
};

use super::action_records::{model_action_context_leaf_id, model_action_payload};
use super::events::{
    insert_event_tx, insert_event_with_activity_tx, insert_transcript_item_events_tx,
};
use super::queue::bump_revisions_tx;
use super::rows::row_to_stored_entry;
use super::sql::{action_is_unfinished, lock_session_tx};
use super::transcript::{
    branch_entries_to_leaf, insert_stored_entry_tx, model_context_from_entries,
};
use super::PostgresAgentStore;

impl PostgresAgentStore {
    /// Transition a model action from `expected_status` (Pending pre-dispatch
    /// or Running post-dispatch) to Blocked and create a sibling Compaction
    /// action. Callers pick the status based on whether the gate fired before
    /// or after the model call started.
    pub async fn block_model_action_for_compaction(
        &self,
        session_id: &str,
        model_action_row_id: &str,
        model_attempt_id: &str,
        expected_status: ActionStatus,
        trigger: CompactionTrigger,
        tokens_before: Option<usize>,
        auto_limit_tokens: Option<usize>,
    ) -> Result<CreateCompactionResult> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let row = sqlx::query(
            r#"
            select action_id, turn_id, payload
            from actions
            where session_id=$1
                and id=$2::text
                and attempt_id=$3::text
                and kind=$4::text
                and status=$5::text
            for update
            "#,
        )
        .bind(session_id)
        .bind(model_action_row_id)
        .bind(model_attempt_id)
        .bind(ActionKind::Model.as_str())
        .bind(expected_status.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "{} model action not found: {model_action_row_id}",
                expected_status.as_str()
            )
        })?;

        let payload: Value = row.get("payload");
        let source_leaf_id = model_action_context_leaf_id(&payload)
            .ok_or_else(|| anyhow!("model action has no compaction source leaf"))?;
        let source_entries =
            branch_entries_to_leaf(&mut *tx, session_id, &source_leaf_id).await?;
        if source_entries.is_empty() {
            return Err(anyhow!("transcript leaf not found: {source_leaf_id}"));
        }
        let model_context = model_context_from_entries(source_entries.clone());
        if model_context.transcript_items().is_empty() {
            return Err(anyhow!("cannot compact an empty model context"));
        }
        let action_id = agent_vocab::ActionId(row.get::<i64, _>("action_id") as u64);
        let turn_id = TurnId(row.get::<i64, _>("turn_id") as u64);
        let scope = if model_context.is_turn_boundary() {
            CompactionScope::Boundary {
                source_leaf_id: source_leaf_id.clone(),
            }
        } else {
            CompactionScope::MidTurn {
                source_leaf_id: source_leaf_id.clone(),
                turn_id,
                blocked_model_action_id: action_id,
                blocked_model_action_row_id: model_action_row_id.to_string(),
                blocked_model_attempt_id: model_attempt_id.to_string(),
            }
        };
        let last_turn_id = model_context.last_turn_id();
        let turn_started_at_ms = match &scope {
            CompactionScope::MidTurn { turn_id, .. } => Some(
                turn_started_at_ms_for_turn(&source_entries, *turn_id).ok_or_else(|| {
                    anyhow!(
                        "mid-turn compaction source is missing turn_started for turn {}",
                        turn_id.0
                    )
                })?,
            ),
            CompactionScope::Boundary { .. } => None,
        };
        let trigger_name = trigger.as_str();
        let reason = trigger.reason().map(str::to_string);
        let action_row_id = format!("action_{}", Uuid::new_v4());
        let attempt_id = Uuid::new_v4().to_string();
        let scope_value = serde_json::to_value(&scope)?;
        let compaction_payload = json!({
            "source_session_id": session_id,
            "source_leaf_id": source_leaf_id,
            "last_turn_id": last_turn_id.0,
            "context_tokens": tokens_before,
            "auto_limit_tokens": auto_limit_tokens,
            "turn_started_at_ms": turn_started_at_ms,
            "trigger": trigger_name,
            "reason": reason,
            "scope": scope_value,
            "blocked_model_action_row_id": model_action_row_id,
            "blocked_model_attempt_id": model_attempt_id,
        });

        let block_result = json!({
            "blocked_by_compaction": action_row_id,
            "reason": reason,
            "context_tokens": tokens_before,
            "auto_limit_tokens": auto_limit_tokens,
        });
        let updated = sqlx::query(
            r#"
            update actions
            set status=$4::text,
                result=$5,
                updated_at=now()
            where session_id=$1
                and id=$2::text
                and attempt_id=$3::text
                and kind='model'
                and status=$6::text
            "#,
        )
        .bind(session_id)
        .bind(model_action_row_id)
        .bind(model_attempt_id)
        .bind(ActionStatus::Blocked.as_str())
        .bind(&block_result)
        .bind(expected_status.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            return Err(anyhow!(
                "{} model action was not blocked: {model_action_row_id}",
                expected_status.as_str()
            ));
        }

        sqlx::query(
            r#"
            insert into actions (id, session_id, turn_id, action_id, attempt_id, kind, status, payload)
            values ($1::text, $2::text, null, 0, $3::text, $4::text, $5::text, $6)
            "#,
        )
        .bind(&action_row_id)
        .bind(session_id)
        .bind(&attempt_id)
        .bind(ActionKind::Compaction.as_str())
        .bind(ActionStatus::Running.as_str())
        .bind(&compaction_payload)
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
                    "payload": compaction_payload,
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
                    "scope": scope.kind(),
                    "tokens_before": tokens_before,
                    "auto_limit_tokens": auto_limit_tokens,
                    "turn_started_at_ms": turn_started_at_ms,
                    "blocked_model_action_row_id": model_action_row_id,
                }),
            )
            .await?,
        ];
        bump_revisions_tx(&mut tx, session_id, false, false).await?;
        tx.commit().await?;

        Ok(CreateCompactionResult {
            job: CompactionJob {
                action_row_id,
                attempt_id,
                source_session_id: session_id.to_string(),
                source_leaf_id,
                compaction_context: compaction_context_for_scope(&model_context, &scope),
                model_context,
                tokens_before,
                last_turn_id,
                turn_started_at_ms,
                trigger,
                reason,
                scope,
            },
            events,
        })
    }

    pub async fn create_compaction_action(
        &self,
        session_id: &str,
        trigger: CompactionTrigger,
    ) -> Result<CreateCompactionResult> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let active_leaf_id: Option<String> =
            sqlx::query_scalar("select active_leaf_id from sessions where id=$1")
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
        let scope = CompactionScope::Boundary {
            source_leaf_id: source_leaf_id.clone(),
        };
        let payload = json!({
            "source_session_id": session_id,
            "source_leaf_id": source_leaf_id,
            "last_turn_id": last_turn_id.0,
            "context_tokens": tokens_before,
            "trigger": trigger_name,
            "reason": reason,
            "scope": scope,
        });
        sqlx::query(
            r#"
            insert into actions (id, session_id, turn_id, action_id, attempt_id, kind, status, payload)
            values ($1::text, $2::text, null, 0, $3::text, $4::text, $5::text, $6)
            "#,
        )
        .bind(&action_row_id)
        .bind(session_id)
        .bind(&attempt_id)
        .bind(ActionKind::Compaction.as_str())
        .bind(ActionStatus::Running.as_str())
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
                    "scope": scope.kind(),
                }),
            )
            .await?,
        ];
        bump_revisions_tx(&mut tx, session_id, false, false).await?;
        tx.commit().await?;

        Ok(CreateCompactionResult {
            job: CompactionJob {
                action_row_id,
                attempt_id,
                source_session_id: session_id.to_string(),
                source_leaf_id,
                compaction_context: model_context.clone(),
                model_context,
                tokens_before,
                last_turn_id,
                turn_started_at_ms: None,
                trigger,
                reason,
                scope,
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
        lock_session_tx(&mut tx, &job.source_session_id).await?;
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
                active_leaf_id: None,
                resumed_model_action: None,
                events: Vec::new(),
            });
        }

        let active_leaf_id: Option<String> =
            sqlx::query_scalar("select active_leaf_id from sessions where id=$1")
                .bind(&job.source_session_id)
                .fetch_one(&mut *tx)
                .await?;
        let source_leaf_changed = active_leaf_id.as_deref() != Some(job.source_leaf_id.as_str());
        let blocked_model_still_blocked = match &job.scope {
            CompactionScope::MidTurn {
                blocked_model_action_row_id,
                blocked_model_attempt_id,
                ..
            } => {
                sqlx::query_scalar::<_, bool>(
                    r#"
                select exists(
                    select 1
                    from actions
                    where session_id=$1
                        and id=$2::text
                        and attempt_id=$3::text
                        and kind='model'
                        and status='blocked'
                )
                "#,
                )
                .bind(&job.source_session_id)
                .bind(blocked_model_action_row_id)
                .bind(blocked_model_attempt_id)
                .fetch_one(&mut *tx)
                .await?
            }
            CompactionScope::Boundary { .. } => false,
        };
        if source_leaf_changed && !blocked_model_still_blocked {
            let events = self
                .mark_compaction_stale_tx(
                    &mut tx,
                    job,
                    "source leaf changed before compaction completed and blocked model action is no longer blocked",
                )
                .await?;
            tx.commit().await?;
            return Ok(CompleteCompactionResult {
                new_root_id: None,
                active_leaf_id: None,
                resumed_model_action: None,
                events,
            });
        }

        let new_root_id = format!("entry_{}", Uuid::new_v4());
        let root_entry = StoredTranscriptEntry {
            id: new_root_id.clone(),
            parent_id: None,
            timestamp_ms: now_ms(),
            item: TranscriptItem::CompactionSummary(
                CompactionSummary::new(
                    job.source_session_id.clone(),
                    job.source_leaf_id.clone(),
                    completion.summary.clone(),
                    job.tokens_before,
                    job.last_turn_id,
                )
                .with_turn_started_at_ms(job.turn_started_at_ms),
            ),
            provider_replay: completion.provider_replay.clone(),
        };
        let _ = insert_stored_entry_tx(&mut tx, &job.source_session_id, &root_entry).await?;

        let mut installed_entries = vec![root_entry.clone()];
        let mut parent_id = new_root_id.clone();
        for mut suffix in completion.continuation_suffix {
            suffix.parent_id = Some(parent_id.clone());
            parent_id = suffix.id.clone();
            let stored = StoredTranscriptEntry::from(suffix);
            let _ = insert_stored_entry_tx(&mut tx, &job.source_session_id, &stored).await?;
            installed_entries.push(stored);
        }
        let installed_active_leaf_id = parent_id;

        sqlx::query("update sessions set active_leaf_id=$2::text, updated_at=now() where id=$1")
            .bind(&job.source_session_id)
            .bind(&installed_active_leaf_id)
            .execute(&mut *tx)
            .await?;

        let mut resumed_model_action = None;
        if let CompactionScope::MidTurn {
            blocked_model_action_row_id,
            blocked_model_attempt_id,
            ..
        } = &job.scope
        {
            let payload = model_action_payload(Some(&installed_active_leaf_id));
            let updated = sqlx::query(
                r#"
                update actions
                set status=$4::text,
                    payload = payload || $5::jsonb,
                    result=null,
                    updated_at=now()
                where session_id=$1
                    and id=$2::text
                    and attempt_id=$3::text
                    and kind='model'
                    and status='blocked'
                "#,
            )
            .bind(&job.source_session_id)
            .bind(blocked_model_action_row_id)
            .bind(blocked_model_attempt_id)
            .bind(ActionStatus::Pending.as_str())
            .bind(&payload)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if updated == 1 {
                if let Some(row) = sqlx::query(
                    r#"
                    select id, attempt_id, kind, action_id, turn_id, payload
                    from actions
                    where session_id=$1 and id=$2::text and attempt_id=$3::text
                    "#,
                )
                .bind(&job.source_session_id)
                .bind(blocked_model_action_row_id)
                .bind(blocked_model_attempt_id)
                .fetch_optional(&mut *tx)
                .await?
                {
                    let new_model_context =
                        model_context_from_installed_entries(&installed_entries);
                    resumed_model_action = Some(PersistedAction {
                        row_id: row.get("id"),
                        attempt_id: row.get("attempt_id"),
                        action: session_action_from_model_row(row, new_model_context)?,
                    });
                }
            }
        }

        let result_payload = json!({
            "new_root_id": new_root_id,
            "active_leaf_id": installed_active_leaf_id,
            "source_session_id": job.source_session_id,
            "source_leaf_id": job.source_leaf_id,
            "trigger": job.trigger.as_str(),
            "reason": job.reason,
            "scope": job.scope.kind(),
            "remote": completion.remote,
            "provider": completion.provider,
            "summary_kind": completion.summary_kind,
            "usage": completion.usage,
            "provider_replay_items": completion.provider_replay.len(),
            "continuation_suffix_items": installed_entries.len().saturating_sub(1),
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

        bump_revisions_tx(&mut tx, &job.source_session_id, false, true).await?;
        let mut events = Vec::new();
        for entry in &installed_entries {
            events.extend(
                insert_transcript_item_events_tx(
                    &mut tx,
                    &job.source_session_id,
                    None,
                    None,
                    &entry.id,
                    &entry.item,
                )
                .await?,
            );
        }
        events.push(
            insert_event_tx(
                &mut tx,
                &job.source_session_id,
                EventType::HistoryCompacted,
                json!({
                    "scope": job.scope.kind(),
                    "new_root_id": new_root_id,
                    "active_leaf_id": installed_active_leaf_id,
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
                    "scope": job.scope.kind(),
                    "new_root_id": new_root_id,
                    "active_leaf_id": installed_active_leaf_id,
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
            active_leaf_id: Some(installed_active_leaf_id),
            resumed_model_action,
            events,
        })
    }

    pub async fn fail_compaction_action(
        &self,
        job: &CompactionJob,
        error: String,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, &job.source_session_id).await?;
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
        bump_revisions_tx(tx, &job.source_session_id, false, false).await?;
        let mut payload = json!({
            "action_row_id": job.action_row_id,
            "error": error,
            "status": status,
            "scope": job.scope.kind(),
            "trigger": job.trigger.as_str(),
        });
        if let Some(reason) = job.trigger.reason() {
            payload["reason"] = json!(reason);
        }
        Ok(vec![
            insert_event_with_activity_tx(
                tx,
                &job.source_session_id,
                EventType::CompactionError,
                payload,
            )
            .await?,
        ])
    }
}

fn compaction_context_for_scope(
    model_context: &ModelContext,
    scope: &CompactionScope,
) -> ModelContext {
    match scope {
        CompactionScope::Boundary { .. } => model_context.clone(),
        CompactionScope::MidTurn { .. } => {
            // Reactive overflow can happen after a long tool-heavy turn has
            // already started. Summarizing only the pre-turn prefix and then
            // replaying the whole open-turn suffix is a no-op for context
            // size, because the large tool outputs are exactly the suffix. The
            // compaction request must summarize the full model-visible
            // context; the daemon will resume from the compacted root instead
            // of reinstalling the raw open-turn suffix.
            model_context.clone()
        }
    }
}

fn model_context_from_installed_entries(entries: &[StoredTranscriptEntry]) -> ModelContext {
    model_context_from_entries(entries.to_vec())
}

fn turn_started_at_ms_for_turn(entries: &[StoredTranscriptEntry], turn_id: TurnId) -> Option<u64> {
    entries.iter().rev().find_map(|entry| match &entry.item {
        TranscriptItem::TurnStarted {
            turn_id: entry_turn_id,
        } if *entry_turn_id == turn_id => Some(entry.timestamp_ms),
        TranscriptItem::CompactionSummary(summary) if summary.last_turn_id == turn_id => {
            summary.turn_started_at_ms
        }
        _ => None,
    })
}

fn session_action_from_model_row(
    row: sqlx::postgres::PgRow,
    model_context: ModelContext,
) -> Result<agent_session::SessionAction> {
    let payload: Value = row.get("payload");
    let context_leaf_id = model_action_context_leaf_id(&payload);
    Ok(agent_session::SessionAction::RequestModel {
        action_id: agent_vocab::ActionId(row.get::<i64, _>("action_id") as u64),
        turn_id: agent_vocab::TurnId(row.get::<i64, _>("turn_id") as u64),
        model_context,
        context_leaf_id,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_vocab::{
        ActionId, AssistantItem, AssistantMessage, CompactionSummary, ToolCall, ToolCallId,
        ToolResultMessage, ToolResultStatus, TurnOutcome, UserMessage,
    };

    fn tool_call(id: u64, name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::from_u64(id),
            tool_name: name.to_string(),
            args_json: "{}".to_string(),
        }
    }

    fn successful_tool_result(tool_call: &ToolCall, output: &str) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.tool_name.clone(),
            output: output.to_string(),
            status: ToolResultStatus::Success,
        }
    }

    fn stored_entry(id: &str, timestamp_ms: u64, item: TranscriptItem) -> StoredTranscriptEntry {
        StoredTranscriptEntry {
            id: id.to_string(),
            parent_id: None,
            timestamp_ms,
            item,
            provider_replay: Vec::new(),
        }
    }

    #[test]
    fn mid_turn_compaction_uses_persisted_turn_start_timestamp() {
        let entries = vec![
            stored_entry(
                "start_1",
                1_000,
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            ),
            stored_entry(
                "user_1",
                1_100,
                TranscriptItem::UserMessage(UserMessage::text("go")),
            ),
        ];

        assert_eq!(
            turn_started_at_ms_for_turn(&entries, TurnId(1)),
            Some(1_000)
        );
    }

    #[test]
    fn repeated_mid_turn_compaction_uses_prior_compaction_turn_start() {
        let entries = vec![stored_entry(
            "compact_1",
            5_000,
            TranscriptItem::CompactionSummary(
                CompactionSummary::new("session", "source", "summary", None, TurnId(7))
                    .with_turn_started_at_ms(Some(1_234)),
            ),
        )];

        assert_eq!(
            turn_started_at_ms_for_turn(&entries, TurnId(7)),
            Some(1_234)
        );
    }

    #[test]
    fn mid_turn_compaction_has_no_timestamp_without_a_persisted_anchor() {
        let entries = vec![stored_entry(
            "compact_1",
            5_000,
            TranscriptItem::CompactionSummary(CompactionSummary::new(
                "session",
                "source",
                "summary",
                None,
                TurnId(7),
            )),
        )];

        assert_eq!(turn_started_at_ms_for_turn(&entries, TurnId(7)), None);
    }

    #[test]
    fn mid_turn_compaction_summarizes_the_full_open_turn() {
        let tool_call = tool_call(7, "Bash");
        let context = ModelContext::from_transcript_items(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage(UserMessage::text("previous")),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("done".to_string())],
            }),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptItem::TurnStarted { turn_id: TurnId(2) },
            TranscriptItem::UserMessage(UserMessage::text("current task")),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            }),
            TranscriptItem::ToolCallStarted {
                turn_id: TurnId(2),
                tool_call: tool_call.clone(),
            },
            TranscriptItem::ToolResult(successful_tool_result(
                &tool_call,
                "large tool output that caused overflow",
            )),
        ]);
        let scope = CompactionScope::MidTurn {
            source_leaf_id: "leaf".to_string(),
            turn_id: TurnId(2),
            blocked_model_action_id: ActionId(3),
            blocked_model_action_row_id: "model_action".to_string(),
            blocked_model_attempt_id: "attempt".to_string(),
        };

        let compaction_context = compaction_context_for_scope(&context, &scope);

        assert_eq!(
            compaction_context.transcript_items(),
            context.transcript_items()
        );
        assert!(compaction_context
            .transcript_items()
            .iter()
            .any(|item| matches!(
                item,
                TranscriptItem::ToolResult(result)
                    if result.output.contains("caused overflow")
            )));
    }
}
