use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::{
    ActionKind, ActionStatus, ClaimedPostCompactionDispatch, CorruptPostCompactionDispatch,
    EventFrame, EventType, PendingDispatchAction, PostCompactionDispatchClaimError,
    PostCompactionDispatchFence, PostCompactionDispatchIntent, PostCompactionDispatchLease,
    ResumableModelAction, StoredAction,
};

use super::action_records::{
    model_action_context_leaf_id, post_compaction_dispatch_context_leaf_id,
    post_compaction_dispatch_lease, post_compaction_dispatch_lease_expires_at_ms,
    set_post_compaction_dispatch_lease, POST_COMPACTION_DISPATCH_KEY,
};
use super::events::insert_event_with_activity_tx;
use super::queue::bump_revisions_tx;
use super::rows::row_text;
use super::sql::{
    action_is_unfinished, lock_session_tx, stale_unfinished_actions,
    stale_unfinished_actions_for_session,
};
use super::PostgresAgentStore;

fn corrupt_post_compaction_claim(
    status: ActionStatus,
    marker: serde_json::Value,
    lease: Option<PostCompactionDispatchLease>,
    error: impl std::fmt::Display,
) -> PostCompactionDispatchClaimError {
    PostCompactionDispatchClaimError::Corrupt(CorruptPostCompactionDispatch::new(
        error.to_string(),
        PostCompactionDispatchFence::new(status, marker, lease),
    ))
}

fn transient_post_compaction_claim(
    error: impl Into<anyhow::Error>,
) -> PostCompactionDispatchClaimError {
    PostCompactionDispatchClaimError::Transient(error.into())
}

fn marker_lease_fence(marker: &serde_json::Value) -> Option<PostCompactionDispatchLease> {
    let owner_id = marker
        .pointer("/lease/owner_id")
        .and_then(serde_json::Value::as_str)?;
    let generation = marker
        .pointer("/lease/generation")
        .and_then(serde_json::Value::as_u64)?;
    let context_leaf_id = marker
        .get("context_leaf_id")
        .and_then(serde_json::Value::as_str)?;
    Some(PostCompactionDispatchLease {
        owner_id: owner_id.to_string(),
        generation,
        context_leaf_id: context_leaf_id.to_string(),
    })
}

impl PostgresAgentStore {
    pub async fn mark_all_unfinished_actions_stale(&self) -> Result<u64> {
        let stale_actions = stale_unfinished_actions();
        let query = format!(
            r#"
            with updated_actions as (
                update actions
                set status='stale',
                    payload=payload - '{POST_COMPACTION_DISPATCH_KEY}',
                    updated_at=now()
                where {stale_actions}
                  and not exists (
                      select 1
                      from queued_inputs q
                      join sessions s on s.id=q.session_id
                      join delegations d on d.id=s.delegation_id
                      where q.session_id=actions.session_id
                        and q.status in ('queued', 'consuming')
                        and q.priority='steer'
                        and q.origin->>'control_kind' in (
                            'scoped_subagent_steer',
                            'scoped_subagent_interrupt'
                        )
                        and q.origin->>'control_phase'='pending_interrupt'
                        and d.status='running'
                  )
                returning session_id
            ),
            updated_sessions as (
                update sessions
                set session_revision=session_revision + 1,
                    updated_at=now()
                where id in (select distinct session_id from updated_actions)
                returning id
            )
            select count(*)::bigint from updated_actions
            "#,
        );
        let updated: i64 = sqlx::query_scalar(&query).fetch_one(&self.pool).await?;
        Ok(updated as u64)
    }

    pub async fn post_compaction_dispatch_session_ids(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(&format!(
            r#"
            select distinct session_id
            from actions
            where status in ('pending','running')
                and kind='model'
                and payload ? '{POST_COMPACTION_DISPATCH_KEY}'
            order by session_id
            "#
        ))
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn post_compaction_dispatch_intents(
        &self,
        session_id: &str,
    ) -> Result<Vec<PostCompactionDispatchIntent>> {
        let rows = sqlx::query(&format!(
            r#"
            select session_id, id, attempt_id
            from actions
            where session_id=$1
                and status in ('pending','running')
                and kind='model'
                and payload ? '{POST_COMPACTION_DISPATCH_KEY}'
            order by created_at, id
            "#
        ))
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| PostCompactionDispatchIntent {
                session_id: row.get("session_id"),
                row_id: row.get("id"),
                attempt_id: row.get("attempt_id"),
            })
            .collect())
    }

    pub async fn post_compaction_dispatch_intents_all(
        &self,
    ) -> Result<Vec<PostCompactionDispatchIntent>> {
        let rows = sqlx::query(&format!(
            r#"
            select session_id, id, attempt_id
            from actions
            where status in ('pending','running')
                and kind='model'
                and payload ? '{POST_COMPACTION_DISPATCH_KEY}'
            order by created_at, id
            "#
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| PostCompactionDispatchIntent {
                session_id: row.get("session_id"),
                row_id: row.get("id"),
                attempt_id: row.get("attempt_id"),
            })
            .collect())
    }

    pub async fn claim_post_compaction_model_action(
        &self,
        intent: &PostCompactionDispatchIntent,
        lease_duration: Duration,
    ) -> std::result::Result<Option<ClaimedPostCompactionDispatch>, PostCompactionDispatchClaimError>
    {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(transient_post_compaction_claim)?;
        lock_session_tx(&mut tx, &intent.session_id)
            .await
            .map_err(transient_post_compaction_claim)?;
        let Some(row) = sqlx::query(&format!(
            r#"
            select a.status, a.kind, a.action_id, a.turn_id, a.payload,
                coalesce(a.provider_config, s.provider_config) as provider_config,
                s.active_leaf_id,
                (extract(epoch from clock_timestamp()) * 1000)::bigint as now_ms
            from actions a
            join sessions s on s.id=a.session_id
            where a.session_id=$1
                and a.id=$2::text
                and a.attempt_id=$3::text
                and a.status in ('pending','running')
                and a.kind='model'
                and a.payload ? '{POST_COMPACTION_DISPATCH_KEY}'
            for update of a
            "#
        ))
        .bind(&intent.session_id)
        .bind(&intent.row_id)
        .bind(&intent.attempt_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(transient_post_compaction_claim)?
        else {
            tx.commit().await.map_err(transient_post_compaction_claim)?;
            return Ok(None);
        };
        let mut payload: serde_json::Value = row.get("payload");
        let original_marker = payload
            .get(POST_COMPACTION_DISPATCH_KEY)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let status = row_text::<ActionStatus>(&row, "status").map_err(|error| {
            corrupt_post_compaction_claim(
                ActionStatus::Running,
                original_marker.clone(),
                None,
                error,
            )
        })?;
        let corrupt = |error| {
            corrupt_post_compaction_claim(
                status,
                original_marker.clone(),
                marker_lease_fence(&original_marker),
                error,
            )
        };
        let context_leaf_id =
            post_compaction_dispatch_context_leaf_id(&payload, &intent.row_id, &intent.attempt_id)
                .map_err(&corrupt)?;
        let active_leaf_id = row.get::<Option<String>, _>("active_leaf_id");
        if active_leaf_id.as_deref() != Some(context_leaf_id.as_str()) {
            return Err(corrupt(anyhow!(
                "post-compaction dispatch context leaf is not the session active leaf"
            )));
        }
        let now_ms = row.get::<i64, _>("now_ms");
        let prior_lease =
            post_compaction_dispatch_lease(&payload, &intent.row_id, &intent.attempt_id)
                .map_err(&corrupt)?;
        let prior_expiration =
            post_compaction_dispatch_lease_expires_at_ms(&payload).map_err(&corrupt)?;
        let generation = match (status, prior_lease.as_ref(), prior_expiration) {
            (ActionStatus::Pending, None, None) => 1,
            (ActionStatus::Running, Some(lease), Some(expires_at_ms))
                if expires_at_ms <= now_ms =>
            {
                lease.generation.checked_add(1).ok_or_else(|| {
                    corrupt(anyhow!("post-compaction dispatch generation overflow"))
                })?
            }
            (ActionStatus::Running, Some(_), Some(_)) => {
                tx.commit().await.map_err(transient_post_compaction_claim)?;
                return Ok(None);
            }
            (ActionStatus::Pending, _, _) => {
                return Err(corrupt(anyhow!(
                    "pending post-compaction dispatch unexpectedly carries a lease"
                )));
            }
            (ActionStatus::Running, _, _) => {
                return Err(corrupt(anyhow!(
                    "running post-compaction dispatch has no valid lease"
                )));
            }
            _ => return Ok(None),
        };
        let lease = PostCompactionDispatchLease {
            owner_id: Uuid::new_v4().to_string(),
            generation,
            context_leaf_id: context_leaf_id.clone(),
        };
        let duration_ms = i64::try_from(lease_duration.as_millis())
            .map_err(|_| {
                transient_post_compaction_claim(anyhow!(
                    "post-compaction dispatch lease duration is too large"
                ))
            })?
            .max(1);
        let expires_at_ms = now_ms.checked_add(duration_ms).ok_or_else(|| {
            transient_post_compaction_claim(anyhow!(
                "post-compaction dispatch lease expiration overflow"
            ))
        })?;
        set_post_compaction_dispatch_lease(&mut payload, &lease, expires_at_ms)
            .map_err(&corrupt)?;
        let updated = sqlx::query(
            r#"
            update actions
            set status='running', payload=$4, updated_at=now()
            where session_id=$1
                and id=$2::text
                and attempt_id=$3::text
                and kind='model'
                and status=$5::text
            "#,
        )
        .bind(&intent.session_id)
        .bind(&intent.row_id)
        .bind(&intent.attempt_id)
        .bind(&payload)
        .bind(status.as_str())
        .execute(&mut *tx)
        .await
        .map_err(transient_post_compaction_claim)?
        .rows_affected();
        if updated != 1 {
            return Err(transient_post_compaction_claim(anyhow!(
                "post-compaction dispatch changed while claiming its lease"
            )));
        }
        bump_revisions_tx(&mut tx, &intent.session_id, false, false)
            .await
            .map_err(transient_post_compaction_claim)?;
        let action_id = row.get::<i64, _>("action_id");
        let turn_id = row
            .get::<Option<i64>, _>("turn_id")
            .ok_or_else(|| corrupt(anyhow!("post-compaction model action missing turn_id")))?;
        if action_id < 0 || turn_id < 0 {
            return Err(corrupt(anyhow!(
                "post-compaction model action has a negative id"
            )));
        }
        let model_context = self
            .model_context_for_leaf(&intent.session_id, &context_leaf_id)
            .await
            .map_err(transient_post_compaction_claim)?;
        tx.commit().await.map_err(transient_post_compaction_claim)?;
        Ok(Some(ClaimedPostCompactionDispatch {
            pending: PendingDispatchAction {
                row_id: intent.row_id.clone(),
                attempt_id: intent.attempt_id.clone(),
                action: agent_session::SessionAction::RequestModel {
                    action_id: agent_vocab::ActionId(action_id as u64),
                    turn_id: agent_vocab::TurnId(turn_id as u64),
                    model_context,
                    context_leaf_id: Some(context_leaf_id),
                },
                route: serde_json::from_value(row.get("provider_config"))
                    .map_err(transient_post_compaction_claim)?,
            },
            lease,
        }))
    }

    pub async fn renew_post_compaction_dispatch_lease(
        &self,
        session_id: &str,
        action_row_id: &str,
        attempt_id: &str,
        lease: &PostCompactionDispatchLease,
        lease_duration: Duration,
    ) -> Result<bool> {
        let duration_ms = i64::try_from(lease_duration.as_millis())
            .map_err(|_| anyhow!("post-compaction dispatch lease duration is too large"))?
            .max(1);
        let updated = sqlx::query(&format!(
            r#"
            update actions
            set payload=jsonb_set(
                    payload,
                    '{{{POST_COMPACTION_DISPATCH_KEY},lease,expires_at_ms}}',
                    to_jsonb(
                        (extract(epoch from clock_timestamp()) * 1000)::bigint
                        + $7::bigint
                    )
                ),
                updated_at=now()
            where session_id=$1
                and id=$2::text
                and attempt_id=$3::text
                and kind='model'
                and status='running'
                and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'action_row_id'=$2
                and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'attempt_id'=$3
                and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'context_leaf_id'=$4
                and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'owner_id'=$5
                and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'generation'=$6
                and (payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'expires_at_ms')::bigint
                    > (extract(epoch from clock_timestamp()) * 1000)::bigint
            "#,
        ))
        .bind(session_id)
        .bind(action_row_id)
        .bind(attempt_id)
        .bind(&lease.context_leaf_id)
        .bind(&lease.owner_id)
        .bind(lease.generation.to_string())
        .bind(duration_ms)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    pub async fn next_post_compaction_dispatch_lease_delay(&self) -> Result<Option<Duration>> {
        let delay_ms: Option<i64> = sqlx::query_scalar(&format!(
            r#"
            select min(
                greatest(
                    coalesce(
                        (payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'expires_at_ms')::bigint,
                        0
                    ) - (extract(epoch from clock_timestamp()) * 1000)::bigint,
                    0
                )
            )
            from actions
            where status in ('pending','running')
                and kind='model'
                and payload ? '{POST_COMPACTION_DISPATCH_KEY}'
            "#,
        ))
        .fetch_one(&self.pool)
        .await?;
        Ok(delay_ms.map(|delay_ms| Duration::from_millis(delay_ms.max(0) as u64)))
    }

    pub async fn mark_unfinished_actions_stale(&self, session_id: &str) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let query = stale_unfinished_actions_for_session();
        let updated = sqlx::query(&query)
            .bind(session_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if updated > 0 {
            bump_revisions_tx(&mut tx, session_id, false, false).await?;
        }
        tx.commit().await?;
        Ok(updated)
    }

    pub async fn has_unfinished_actions(&self, session_id: &str) -> Result<bool> {
        let unfinished_actions = action_is_unfinished(None);
        let query = format!(
            "select exists(select 1 from actions where session_id=$1 and {unfinished_actions})"
        );
        Ok(sqlx::query_scalar(&query)
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn load_harness_model_action(
        &self,
        session_id: &str,
        action_row_id: &str,
    ) -> Result<StoredAction> {
        let row = sqlx::query(
            "select kind, action_id, turn_id, attempt_id, payload from actions where session_id=$1 and id=$2::text and kind='model' and status in ('pending','running')",
        )
            .bind(session_id)
            .bind(action_row_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("model action not found or not active: {action_row_id}"))?;
        let payload: serde_json::Value = row.get("payload");
        let attempt_id: String = row.get("attempt_id");
        let (post_compaction_dispatch_context_leaf_id, post_compaction_dispatch_lease) =
            if payload.get(POST_COMPACTION_DISPATCH_KEY).is_some() {
                (
                    Some(post_compaction_dispatch_context_leaf_id(
                        &payload,
                        action_row_id,
                        &attempt_id,
                    )?),
                    post_compaction_dispatch_lease(&payload, action_row_id, &attempt_id)?,
                )
            } else {
                (None, None)
            };
        Ok(StoredAction {
            kind: row_text::<ActionKind>(&row, "kind")?,
            action_id: row.get("action_id"),
            turn_id: row.get("turn_id"),
            attempt_id,
            post_compaction_dispatch_context_leaf_id,
            post_compaction_dispatch_lease,
        })
    }

    pub async fn find_resumable_model_action(
        &self,
        session_id: &str,
        turn_id: agent_vocab::TurnId,
    ) -> Result<Option<ResumableModelAction>> {
        let statuses = [
            ActionStatus::Error,
            ActionStatus::Interrupted,
            ActionStatus::Stale,
        ]
        .into_iter()
        .map(|status| status.as_str().to_string())
        .collect::<Vec<_>>();
        let row = sqlx::query(
            r#"
            select action_id, turn_id, status, payload, result
            from actions
            where session_id=$1
                and turn_id=$2
                and kind=$3
                and status = any($4::text[])
            order by updated_at desc, created_at desc
            limit 1
            "#,
        )
        .bind(session_id)
        .bind(turn_id.0 as i64)
        .bind(ActionKind::Model.as_str())
        .bind(statuses)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let payload: serde_json::Value = row.get("payload");
        let context_leaf_id = payload
            .get("context_leaf_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("resumable model action has no context_leaf_id"))?
            .to_string();
        Ok(Some(ResumableModelAction {
            action_id: agent_vocab::ActionId(row.get::<i64, _>("action_id") as u64),
            turn_id: agent_vocab::TurnId(row.get::<i64, _>("turn_id") as u64),
            status: row_text::<ActionStatus>(&row, "status")?,
            context_leaf_id,
        }))
    }

    pub async fn action_can_complete(
        &self,
        session_id: &str,
        action_row_id: &str,
        attempt_id: &str,
        post_compaction_dispatch_lease: Option<&PostCompactionDispatchLease>,
    ) -> Result<bool> {
        let lease_owner = post_compaction_dispatch_lease.map(|lease| lease.owner_id.as_str());
        let lease_generation =
            post_compaction_dispatch_lease.map(|lease| lease.generation.to_string());
        let lease_context =
            post_compaction_dispatch_lease.map(|lease| lease.context_leaf_id.as_str());
        let query = format!(
            r#"
                select exists(
                    select 1
                    from actions
                    where session_id=$1
                        and id=$2::text
                        and attempt_id=$3::text
                        and status='running'
                        and (
                            (
                                $4::text is null
                                and not (payload ? '{POST_COMPACTION_DISPATCH_KEY}')
                            )
                            or (
                                payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'action_row_id'=$2
                                and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'attempt_id'=$3
                                and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'context_leaf_id'=$6
                                and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'owner_id'=$4
                                and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'generation'=$5
                                and (payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'expires_at_ms')::bigint
                                    > (extract(epoch from clock_timestamp()) * 1000)::bigint
                            )
                        )
                )
                "#
        );
        Ok(sqlx::query_scalar(&query)
            .bind(session_id)
            .bind(action_row_id)
            .bind(attempt_id)
            .bind(lease_owner)
            .bind(lease_generation)
            .bind(lease_context)
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn mark_action_running_and_event(
        &self,
        session_id: &str,
        action_row_id: &str,
        attempt_id: &str,
        event_type: EventType,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let query = "update actions set status='running', updated_at=now() where session_id=$1 and id=$2::text and attempt_id=$3::text and status='pending'";
        let updated = sqlx::query(query)
            .bind(session_id)
            .bind(action_row_id)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if updated != 1 {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        bump_revisions_tx(&mut tx, session_id, false, false).await?;
        let event = insert_event_with_activity_tx(
            &mut tx,
            session_id,
            event_type,
            json!({ "action_row_id": action_row_id }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn fail_corrupt_post_compaction_model_action(
        &self,
        intent: &PostCompactionDispatchIntent,
        fence: &PostCompactionDispatchFence,
        error: &str,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, &intent.session_id).await?;
        let lease_owner = fence.lease.as_ref().map(|lease| lease.owner_id.as_str());
        let lease_generation = fence
            .lease
            .as_ref()
            .map(|lease| lease.generation.to_string());
        let updated = sqlx::query(&format!(
            r#"
            update actions
            set status=$6::text,
                result=$7,
                payload=payload - $8::text,
                updated_at=now()
            where session_id=$1
                and id=$2::text
                and attempt_id=$3::text
                and kind='model'
                and status=$4::text
                and payload->'{POST_COMPACTION_DISPATCH_KEY}'=$5
                and (
                    $9::text is null
                    or (
                        payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'owner_id'=$9
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'generation'=$10
                    )
                )
            "#,
        ))
        .bind(&intent.session_id)
        .bind(&intent.row_id)
        .bind(&intent.attempt_id)
        .bind(fence.status.as_str())
        .bind(&fence.marker)
        .bind(ActionStatus::Error.as_str())
        .bind(serde_json::json!({ "error": error }))
        .bind(POST_COMPACTION_DISPATCH_KEY)
        .bind(lease_owner)
        .bind(lease_generation)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        bump_revisions_tx(&mut tx, &intent.session_id, false, false).await?;
        let event = insert_event_with_activity_tx(
            &mut tx,
            &intent.session_id,
            EventType::ModelError,
            serde_json::json!({
                "action_row_id": intent.row_id,
                "error": error,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn mark_action_stale(
        &self,
        session_id: &str,
        action_row_id: &str,
        attempt_id: &str,
        post_compaction_dispatch_lease: Option<&PostCompactionDispatchLease>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let unfinished_actions = action_is_unfinished(None);
        let lease_owner = post_compaction_dispatch_lease.map(|lease| lease.owner_id.as_str());
        let lease_generation =
            post_compaction_dispatch_lease.map(|lease| lease.generation.to_string());
        let lease_context =
            post_compaction_dispatch_lease.map(|lease| lease.context_leaf_id.as_str());
        let query = format!(
            r#"
            update actions
            set status='stale',
                payload=payload - '{POST_COMPACTION_DISPATCH_KEY}',
                updated_at=now()
            where session_id=$1
                and id=$2::text
                and attempt_id=$3::text
                and {unfinished_actions}
                and (
                    (
                        $4::text is null
                        and not (payload ? '{POST_COMPACTION_DISPATCH_KEY}')
                    )
                    or (
                        payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'action_row_id'=$2
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'attempt_id'=$3
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'context_leaf_id'=$6
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'owner_id'=$4
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'generation'=$5
                        and (payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'expires_at_ms')::bigint
                            > (extract(epoch from clock_timestamp()) * 1000)::bigint
                    )
                )
            "#,
        );
        let updated = sqlx::query(&query)
            .bind(session_id)
            .bind(action_row_id)
            .bind(attempt_id)
            .bind(lease_owner)
            .bind(lease_generation)
            .bind(lease_context)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if updated > 0 {
            bump_revisions_tx(&mut tx, session_id, false, false).await?;
        }
        tx.commit().await?;
        Ok(updated > 0)
    }

    pub async fn pending_actions_for_dispatch(
        &self,
        session_id: &str,
    ) -> Result<Vec<PendingDispatchAction>> {
        let rows = sqlx::query(&format!(
            r#"
            select a.session_id, a.id, a.attempt_id, a.kind, a.action_id, a.turn_id,
                a.payload, coalesce(a.provider_config, s.provider_config) as provider_config
            from actions a
            join sessions s on s.id=a.session_id
            where a.session_id=$1
                and a.status='pending'
                and not (a.kind='model' and a.payload ? '{POST_COMPACTION_DISPATCH_KEY}')
            order by a.created_at
            "#,
        ))
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        let mut actions = Vec::new();
        for row in rows {
            match row_text::<ActionKind>(&row, "kind") {
                Ok(ActionKind::Model) => {
                    actions.push(self.pending_model_dispatch_from_row(row, None).await?)
                }
                Ok(ActionKind::Tool) => actions.push(pending_tool_dispatch_from_row(row)?),
                Ok(ActionKind::Compaction) => {}
                Err(error) => return Err(anyhow!(error)),
            }
        }
        Ok(actions)
    }

    async fn pending_model_dispatch_from_row(
        &self,
        row: sqlx::postgres::PgRow,
        context_leaf_id: Option<String>,
    ) -> Result<PendingDispatchAction> {
        let payload: serde_json::Value = row.get("payload");
        let context_leaf_id = context_leaf_id
            .or_else(|| model_action_context_leaf_id(&payload))
            .ok_or_else(|| anyhow!("pending model action missing context_leaf_id"))?;
        let action_id = row.get::<i64, _>("action_id");
        let turn_id = row
            .get::<Option<i64>, _>("turn_id")
            .ok_or_else(|| anyhow!("pending model action missing turn_id"))?;
        if action_id < 0 || turn_id < 0 {
            return Err(anyhow!("pending model action has a negative id"));
        }
        let model_context = self
            .model_context_for_leaf(row.get("session_id"), &context_leaf_id)
            .await?;
        Ok(PendingDispatchAction {
            row_id: row.get("id"),
            attempt_id: row.get("attempt_id"),
            action: agent_session::SessionAction::RequestModel {
                action_id: agent_vocab::ActionId(action_id as u64),
                turn_id: agent_vocab::TurnId(turn_id as u64),
                model_context,
                context_leaf_id: Some(context_leaf_id),
            },
            route: serde_json::from_value(row.get("provider_config"))?,
        })
    }

    pub async fn claim_pending_model_action(
        &self,
        session_id: &str,
        action_row_id: &str,
        attempt_id: &str,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let updated = sqlx::query(
            "update actions set status='running', updated_at=now() where session_id=$1 and id=$2::text and attempt_id=$3::text and kind='model' and status='pending' and not (payload ? $4::text)",
        )
        .bind(session_id)
        .bind(action_row_id)
        .bind(attempt_id)
        .bind(POST_COMPACTION_DISPATCH_KEY)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 1 {
            bump_revisions_tx(&mut tx, session_id, false, false).await?;
        }
        tx.commit().await?;
        Ok(updated == 1)
    }

    pub async fn fail_unfinished_model_action(
        &self,
        session_id: &str,
        action_row_id: &str,
        attempt_id: &str,
        post_compaction_dispatch_lease: Option<&PostCompactionDispatchLease>,
        error: &str,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let lease_owner = post_compaction_dispatch_lease.map(|lease| lease.owner_id.as_str());
        let lease_generation =
            post_compaction_dispatch_lease.map(|lease| lease.generation.to_string());
        let lease_context =
            post_compaction_dispatch_lease.map(|lease| lease.context_leaf_id.as_str());
        let updated = sqlx::query(&format!(
            r#"
            update actions
            set status=$4::text,
                result=$5,
                payload=payload - $6::text,
                updated_at=now()
            where session_id=$1
                and id=$2::text
                and attempt_id=$3::text
                and kind='model'
                and status in ('pending','blocked','running')
                and (
                    (
                        $7::text is null
                        and (
                            not (payload ? '{POST_COMPACTION_DISPATCH_KEY}')
                            or status='pending'
                        )
                    )
                    or (
                        payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'action_row_id'=$2
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'attempt_id'=$3
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'context_leaf_id'=$9
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'owner_id'=$7
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'generation'=$8
                        and (payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'expires_at_ms')::bigint
                            > (extract(epoch from clock_timestamp()) * 1000)::bigint
                    )
                )
            "#,
        ))
        .bind(session_id)
        .bind(action_row_id)
        .bind(attempt_id)
        .bind(ActionStatus::Error.as_str())
        .bind(serde_json::json!({ "error": error }))
        .bind(POST_COMPACTION_DISPATCH_KEY)
        .bind(lease_owner)
        .bind(lease_generation)
        .bind(lease_context)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        bump_revisions_tx(&mut tx, session_id, false, false).await?;
        let event = insert_event_with_activity_tx(
            &mut tx,
            session_id,
            EventType::ModelError,
            serde_json::json!({
                "action_row_id": action_row_id,
                "error": error,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn cancel_unfinished_session_work(
        &self,
        session_id: &str,
        reason: &str,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let unfinished_actions = action_is_unfinished(None);
        let query = format!(
            r#"
            update actions
            set status=$2::text,
                result=$3,
                payload=payload - $4::text,
                updated_at=now()
            where session_id=$1 and {unfinished_actions}
            "#
        );
        let updated = sqlx::query(&query)
            .bind(session_id)
            .bind(ActionStatus::Interrupted.as_str())
            .bind(json!({ "reason": reason }))
            .bind(POST_COMPACTION_DISPATCH_KEY)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if updated == 0 {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        bump_revisions_tx(&mut tx, session_id, false, false).await?;
        let event = insert_event_with_activity_tx(
            &mut tx,
            session_id,
            EventType::SessionWorkCancelled,
            json!({ "reason": reason, "actions_interrupted": updated }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }
}

fn pending_tool_dispatch_from_row(row: sqlx::postgres::PgRow) -> Result<PendingDispatchAction> {
    let payload: serde_json::Value = row.get("payload");
    Ok(PendingDispatchAction {
        row_id: row.get("id"),
        attempt_id: row.get("attempt_id"),
        action: agent_session::SessionAction::RequestTool {
            action_id: agent_vocab::ActionId(row.get::<i64, _>("action_id") as u64),
            turn_id: agent_vocab::TurnId(row.get::<i64, _>("turn_id") as u64),
            tool_call: serde_json::from_value(payload)?,
        },
        route: serde_json::from_value(row.get("provider_config"))?,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_session::{ModelContext, SessionAction};
    use agent_vocab::{
        ActionId, ProviderConfig, ProviderKind, ReasoningEffort, ToolCall, ToolCallId, TurnId,
        UserMessage,
    };
    use serde_json::json;
    use uuid::Uuid;

    use crate::{InputPriority, SessionActivity, SessionConfig};

    use super::*;

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(1);

    struct TestDb {
        store: PostgresAgentStore,
        admin_url: String,
        name: String,
    }

    impl TestDb {
        async fn cleanup(self) {
            self.store.close().await;
            if let Ok(admin) = sqlx::PgPool::connect(&self.admin_url).await {
                let _ = sqlx::query(&format!(r#"drop database if exists "{}""#, self.name))
                    .execute(&admin)
                    .await;
                admin.close().await;
            }
        }
    }

    async fn test_store() -> Option<TestDb> {
        let admin_url = std::env::var("PI_RELAY_TEST_DATABASE_URL").ok()?;
        let name = format!(
            "pi_relay_cancel_test_{}_{}",
            std::process::id(),
            TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = sqlx::PgPool::connect(&admin_url)
            .await
            .expect("connect to PI_RELAY_TEST_DATABASE_URL");
        sqlx::query(&format!(r#"create database "{name}""#))
            .execute(&admin)
            .await
            .expect("create isolated test database");
        admin.close().await;
        let database_url = database_url_with_name(&admin_url, &name);
        let store = PostgresAgentStore::connect(&database_url)
            .await
            .expect("connect isolated test database");
        store
            .migrate()
            .await
            .expect("migrate isolated test database");
        Some(TestDb {
            store,
            admin_url,
            name,
        })
    }

    fn database_url_with_name(base: &str, name: &str) -> String {
        let (prefix, query) = base
            .split_once('?')
            .map(|(prefix, query)| (prefix, format!("?{query}")))
            .unwrap_or((base, String::new()));
        let Some((root, _)) = prefix.rsplit_once('/') else {
            return format!("{base}_{name}");
        };
        format!("{root}/{name}{query}")
    }

    fn session_config(project_id: Uuid) -> SessionConfig {
        SessionConfig {
            project_id: Some(project_id),
            outer_cwd: "/tmp".to_string(),
            workspaces: Vec::new(),
            system_prompt: "test prompt".to_string(),
            provider: ProviderConfig {
                kind: ProviderKind::OpenAi,
                model: "test-model".to_string(),
                reasoning_effort: ReasoningEffort::Medium,
                max_tokens: None,
                prompt_cache: None,
            },
            metadata: json!({}),
            mcp_manifest: None,
        }
    }

    async fn create_session(store: &PostgresAgentStore, session_id: &str) -> SessionConfig {
        let project_id = Uuid::new_v4();
        store
            .create_project(project_id, "test", &[], json!({}))
            .await
            .expect("project creates");
        let config = session_config(project_id);
        store
            .start_session_outputs(
                session_id,
                &config,
                &[],
                None,
                &[],
                &[],
                InputPriority::FollowUp,
                &UserMessage::text("seed"),
                None,
            )
            .await
            .expect("session starts");
        config
    }

    #[tokio::test]
    async fn tool_claim_is_a_single_pending_to_running_transition_with_one_start_event() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "tool-claim-once";
        create_session(store, session_id).await;
        let (_, actions) = store
            .persist_outputs(
                session_id,
                crate::OutputBatch::new(
                    &[],
                    None,
                    &[],
                    &[SessionAction::RequestTool {
                        action_id: ActionId(1),
                        turn_id: TurnId(1),
                        tool_call: ToolCall {
                            id: ToolCallId::from_u64(1),
                            tool_name: "Bash".to_string(),
                            args_json: r#"{"command":"true"}"#.to_string(),
                        },
                    }],
                ),
            )
            .await
            .expect("tool action persists");
        let action = actions.first().expect("tool dispatch persists");

        let first = store
            .mark_action_running_and_event(
                session_id,
                &action.row_id,
                &action.attempt_id,
                EventType::ToolStarted,
            )
            .await
            .expect("first claim succeeds");
        let duplicate = store
            .mark_action_running_and_event(
                session_id,
                &action.row_id,
                &action.attempt_id,
                EventType::ToolStarted,
            )
            .await
            .expect("duplicate claim is a no-op");

        assert_eq!(first.len(), 1);
        assert_eq!(first[0].event, EventType::ToolStarted);
        assert!(duplicate.is_empty());
        let status: String = sqlx::query_scalar("select status from actions where id=$1::text")
            .bind(&action.row_id)
            .fetch_one(&store.pool)
            .await
            .expect("tool status loads");
        assert_eq!(status, ActionStatus::Running.as_str());
        let started_count = store
            .events_after(session_id, None)
            .await
            .expect("events load")
            .into_iter()
            .filter(|event| event.event == EventType::ToolStarted)
            .count();
        assert_eq!(started_count, 1);
        db.cleanup().await;
    }

    #[tokio::test]
    async fn cancel_unfinished_session_work_marks_compaction_idle_without_active_runtime() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "cancel-compaction";
        create_session(store, session_id).await;

        sqlx::query(
            r#"
            insert into actions (id, session_id, turn_id, action_id, attempt_id, kind, status, payload)
            values ('compaction_1', $1, null, 0, 'attempt_1', 'compaction', 'running', '{}')
            "#,
        )
        .bind(session_id)
        .execute(&store.pool)
        .await
        .expect("insert compaction action");

        let events = store
            .cancel_unfinished_session_work(session_id, "session interrupted")
            .await
            .expect("cancel succeeds");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, EventType::SessionWorkCancelled);
        assert_eq!(
            store.activity(session_id).await.unwrap(),
            SessionActivity::Idle
        );
        let status: String =
            sqlx::query_scalar("select status from actions where id='compaction_1'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(status, "interrupted");
        db.cleanup().await;
    }

    #[tokio::test]
    async fn cancel_unfinished_session_work_marks_model_and_tool_interrupted() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "cancel-actions";
        create_session(store, session_id).await;
        let actions = vec![
            SessionAction::RequestModel {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                model_context: ModelContext::default(),
                context_leaf_id: None,
            },
            SessionAction::RequestTool {
                action_id: ActionId(2),
                turn_id: TurnId(1),
                tool_call: ToolCall {
                    id: ToolCallId::from_u64(1),
                    tool_name: "bash".to_string(),
                    args_json: "{}".to_string(),
                },
            },
        ];
        store
            .persist_outputs(
                session_id,
                crate::OutputBatch::new(&[], None, &[], &actions),
            )
            .await
            .expect("actions persist");

        let events = store
            .cancel_unfinished_session_work(session_id, "session interrupted")
            .await
            .expect("cancel succeeds");
        assert_eq!(events.len(), 1);
        let statuses: Vec<String> =
            sqlx::query_scalar("select status from actions where session_id=$1 order by action_id")
                .bind(session_id)
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(statuses, vec!["interrupted", "interrupted"]);
        db.cleanup().await;
    }

    #[tokio::test]
    async fn cancel_unfinished_session_work_is_idempotent_after_first_cancel() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "cancel-idempotent";
        create_session(store, session_id).await;
        sqlx::query(
            r#"
            insert into actions (id, session_id, turn_id, action_id, attempt_id, kind, status, payload)
            values ('compaction_1', $1, null, 0, 'attempt_1', 'compaction', 'running', '{}')
            "#,
        )
        .bind(session_id)
        .execute(&store.pool)
        .await
        .expect("insert compaction action");

        let first = store
            .cancel_unfinished_session_work(session_id, "session interrupted")
            .await
            .expect("first cancel succeeds");
        let second = store
            .cancel_unfinished_session_work(session_id, "session interrupted")
            .await
            .expect("second cancel succeeds");

        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        db.cleanup().await;
    }
}
