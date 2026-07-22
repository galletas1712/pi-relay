use agent_vocab::UserMessage;
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::{
    CancelQueuedInputResult, EnqueueUserInputResult, EventType, ExpectedActiveLeafMismatch,
    InputPriority, InputRecord, PromoteQueuedInputResult, QueueMutationError, QueuedInput,
    QueuedInputContent, QueuedInputStatus, ReorderQueuedFollowUpsResult, UpdateQueuedInputResult,
};

use super::events::insert_event_tx;
use super::queue_projection::{
    append_queued_content_event_fields, bump_revisions_tx, queue_event_payload, queue_state_tx,
    queued_follow_up_ids_from_state, queued_follow_up_ids_tx, queued_input_content_from_value,
    renumber_follow_ups_tx, revision_mismatch_tx,
};
use super::rows::row_text;
use super::sql::{
    lock_session_tx, queued_input_is_active, queued_input_is_editable, session_default_route_tx,
    steering_route_tx, unfinished_generation_route_tx, QUEUED_INPUT_DISPATCH_ORDER,
};
use super::PostgresAgentStore;

const QUEUE_CHANGED: &str = "queue_changed";
const NOT_EDITABLE: &str = "not_editable";

impl PostgresAgentStore {
    pub async fn enqueue_user_input(
        &self,
        session_id: &str,
        priority: InputPriority,
        content: &UserMessage,
        client_input_id: Option<&str>,
        expected_active_leaf_id: Option<Option<&str>>,
    ) -> Result<EnqueueUserInputResult> {
        let id = format!("input_{}", Uuid::new_v4());
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        if let Some(client_input_id) = client_input_id {
            if let Some(row) = sqlx::query(
                "select id, status from queued_inputs where session_id=$1 and client_input_id=$2::text",
            )
            .bind(session_id)
            .bind(client_input_id)
            .fetch_optional(&mut *tx)
            .await?
            {
                let input_id = row.get("id");
                let queue = queue_state_tx(&mut tx, session_id).await?;
                tx.commit().await?;
                return Ok(EnqueueUserInputResult {
                    input_id,
                    event: None,
                    queue: Some(queue),
                    replayed: true,
                    status: row_text::<QueuedInputStatus>(&row, "status")?,
                    control_interrupt_applied: false,
                    delegation_running: true,
                    control_phase: None,
                    control_interrupt_outcome: None,
                });
            }
        }
        ensure_expected_active_leaf_tx(&mut tx, session_id, expected_active_leaf_id).await?;
        let route = match priority {
            InputPriority::Steer => steering_route_tx(&mut tx, session_id).await?,
            InputPriority::FollowUp => session_default_route_tx(&mut tx, session_id).await?,
        };
        let inserted = sqlx::query(
            r#"
                insert into queued_inputs (
                    id,
                    session_id,
                    priority,
                    content,
                    status,
                    client_input_id,
                    origin,
                    follow_up_position,
                    provider_config
                )
                values (
                    $1,
                    $2,
                    $3,
                    $4,
                    'queued',
                    $5,
                    case
                        when $3 = 'steer' then jsonb_build_object('promoted_at', clock_timestamp()::text)
                        else null
                    end,
                    case
                        when $3 = 'follow_up' then (
                            select coalesce(max(follow_up_position), -1) + 1
                            from queued_inputs
                            where session_id=$2
                                and priority='follow_up'
                                and status='queued'
                        )
                        else null
                    end,
                    $6
                )
                on conflict (session_id, client_input_id) where client_input_id is not null
                do nothing
                returning id
                "#,
        )
        .bind(&id)
        .bind(session_id)
        .bind(priority.as_str())
        .bind(serde_json::to_value(QueuedInputContent::user_message(content.clone()))?)
        .bind(client_input_id)
        .bind(serde_json::to_value(route)?)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(inserted) = inserted else {
            let row = sqlx::query(
                "select id, status from queued_inputs where session_id=$1 and client_input_id=$2::text",
            )
            .bind(session_id)
            .bind(client_input_id)
            .fetch_one(&mut *tx)
            .await?;
            let input_id = row.get("id");
            let queue = queue_state_tx(&mut tx, session_id).await?;
            tx.commit().await?;
            return Ok(EnqueueUserInputResult {
                input_id,
                event: None,
                queue: Some(queue),
                replayed: true,
                status: row_text::<QueuedInputStatus>(&row, "status")?,
                control_interrupt_applied: false,
                delegation_running: true,
                control_phase: None,
                control_interrupt_outcome: None,
            });
        };

        bump_revisions_tx(&mut tx, session_id, true, false).await?;
        let queue = queue_state_tx(&mut tx, session_id).await?;
        let input_id = inserted.get("id");
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::InputQueued,
            queue_event_payload(
                &queue,
                json!({
                    "input_id": input_id,
                    "priority": priority,
                    "client_input_id": client_input_id,
                    "content": content.content.clone(),
                    "content_type": "user_message",
                }),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(EnqueueUserInputResult {
            input_id,
            event: Some(event),
            queue: Some(queue),
            replayed: false,
            status: QueuedInputStatus::Queued,
            control_interrupt_applied: false,
            delegation_running: true,
            control_phase: None,
            control_interrupt_outcome: None,
        })
    }

    pub async fn find_client_input(
        &self,
        session_id: &str,
        client_input_id: &str,
    ) -> Result<Option<InputRecord>> {
        let row = sqlx::query(
            r#"
                select id, status
                from queued_inputs
                where session_id=$1 and client_input_id=$2::text
                "#,
        )
        .bind(session_id)
        .bind(client_input_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(InputRecord {
                input_id: row.get("id"),
                status: row_text::<QueuedInputStatus>(&row, "status")?,
            })
        })
        .transpose()
    }

    pub async fn take_next_queued_input(&self, session_id: &str) -> Result<Option<QueuedInput>> {
        self.take_next_queued_input_matching(session_id, None).await
    }

    pub async fn take_next_queued_steer_input(
        &self,
        session_id: &str,
    ) -> Result<Option<QueuedInput>> {
        self.take_next_queued_input_matching(session_id, Some(InputPriority::Steer))
            .await
    }

    async fn take_next_queued_input_matching(
        &self,
        session_id: &str,
        priority: Option<InputPriority>,
    ) -> Result<Option<QueuedInput>> {
        let editable_queue = queued_input_is_editable(None);
        let priority_filter = priority.map(|priority| priority.as_str().to_string());
        let query = format!(
            r#"
                select id, priority, content,
                    coalesce(
                        provider_config,
                        (select provider_config from sessions where id=$1)
                    ) as provider_config,
                    client_input_id,
                    xmin::text as row_version
                from queued_inputs
                where session_id=$1
                    and {editable_queue}
                    and coalesce(origin->>'control_kind', '') <> 'scoped_subagent_interrupt'
                    and not (
                        coalesce((origin->>'control_interrupt')::boolean, false)
                        and coalesce(origin->>'control_phase', 'pending_interrupt') <> 'ready'
                    )
                    and not exists (
                        select 1
                        from queued_inputs blocked
                        where blocked.session_id=$1
                          and blocked.status in ('queued', 'consuming')
                          and coalesce((blocked.origin->>'control_interrupt')::boolean, false)
                          and blocked.origin->>'control_phase' in ('pending_interrupt', 'interrupt_applied')
                    )
                    and ($2::text is null or priority=$2::text)
                order by {QUEUED_INPUT_DISPATCH_ORDER}
                limit 1
                "#
        );
        let row = sqlx::query(&query)
            .bind(session_id)
            .bind(priority_filter.as_deref())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let content = queued_input_content_from_value(row.get::<Value, _>("content"))?;
        Ok(Some(QueuedInput {
            id: row.get("id"),
            priority: row_text::<InputPriority>(&row, "priority")?,
            content,
            route: serde_json::from_value(row.get("provider_config"))?,
            client_input_id: row.get("client_input_id"),
            claim_id: String::new(),
            row_version: row.get("row_version"),
        }))
    }

    pub async fn promote_queued_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<PromoteQueuedInputResult> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let generation_route = unfinished_generation_route_tx(&mut tx, session_id).await?;
        let editable_queue = queued_input_is_editable(None);
        let query = format!(
            r#"
                update queued_inputs
                set priority='steer',
                    provider_config=coalesce($3, provider_config),
                    follow_up_position=null,
                    updated_at=now(),
                    origin=coalesce(origin, '{{}}'::jsonb)
                        || jsonb_build_object('promoted_at', clock_timestamp()::text)
                where session_id=$1
                    and id=$2::text
                    and {editable_queue}
                    and priority='follow_up'
                returning client_input_id, content, origin->>'promoted_at' as promoted_at
                "#
        );
        let row = sqlx::query(&query)
            .bind(session_id)
            .bind(input_id)
            .bind(generation_route.map(serde_json::to_value).transpose()?)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            let row = sqlx::query(
                r#"
                    select priority, status
                    from queued_inputs
                    where session_id=$1 and id=$2::text
                    "#,
            )
            .bind(session_id)
            .bind(input_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(row) = row else {
                return Err(QueueMutationError::not_found(input_id).into());
            };
            let queue = queue_state_tx(&mut tx, session_id).await?;
            let result = PromoteQueuedInputResult {
                input_id: input_id.to_string(),
                priority: row_text::<InputPriority>(&row, "priority")?,
                status: row_text::<QueuedInputStatus>(&row, "status")?,
                promoted: false,
                event: None,
                queue,
            };
            tx.commit().await?;
            return Ok(result);
        };
        renumber_follow_ups_tx(&mut tx, session_id).await?;
        bump_revisions_tx(&mut tx, session_id, true, false).await?;
        let queue = queue_state_tx(&mut tx, session_id).await?;
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::InputPromoted,
            queue_event_payload(&queue, {
                let content = queued_input_content_from_value(row.get::<Value, _>("content"))?;
                let mut payload = json!({
                "input_id": input_id,
                "priority": InputPriority::Steer,
                "client_input_id": row.get::<Option<String>, _>("client_input_id"),
                "promoted_at": row.get::<Option<String>, _>("promoted_at"),
                });
                append_queued_content_event_fields(&mut payload, &content);
                payload
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(PromoteQueuedInputResult {
            input_id: input_id.to_string(),
            priority: InputPriority::Steer,
            status: QueuedInputStatus::Queued,
            promoted: true,
            event: Some(event),
            queue,
        })
    }

    pub async fn update_queued_input(
        &self,
        session_id: &str,
        input_id: &str,
        content: &UserMessage,
        expected_queue_revision: Option<i64>,
    ) -> Result<UpdateQueuedInputResult> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        if revision_mismatch_tx(&mut tx, session_id, expected_queue_revision).await? {
            let queue = queue_state_tx(&mut tx, session_id).await?;
            tx.commit().await?;
            return Ok(UpdateQueuedInputResult {
                input_id: input_id.to_string(),
                updated: false,
                reason: Some(QUEUE_CHANGED.to_string()),
                priority: InputPriority::FollowUp,
                status: QueuedInputStatus::Queued,
                event: None,
                queue,
            });
        }

        let row = sqlx::query(
            r#"
                select priority, status, content
                from queued_inputs
                where session_id=$1 and id=$2::text
                "#,
        )
        .bind(session_id)
        .bind(input_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(QueueMutationError::not_found(input_id).into());
        };
        let priority = row_text::<InputPriority>(&row, "priority")?;
        let status = row_text::<QueuedInputStatus>(&row, "status")?;
        if priority != InputPriority::FollowUp || status != QueuedInputStatus::Queued {
            let queue = queue_state_tx(&mut tx, session_id).await?;
            tx.commit().await?;
            return Ok(UpdateQueuedInputResult {
                input_id: input_id.to_string(),
                updated: false,
                reason: Some(NOT_EDITABLE.to_string()),
                priority,
                status,
                event: None,
                queue,
            });
        }

        let previous_content = row.get::<Value, _>("content");
        let content_value =
            serde_json::to_value(QueuedInputContent::user_message(content.clone()))?;
        if previous_content == content_value || previous_content == serde_json::to_value(content)? {
            let queue = queue_state_tx(&mut tx, session_id).await?;
            tx.commit().await?;
            return Ok(UpdateQueuedInputResult {
                input_id: input_id.to_string(),
                updated: false,
                reason: None,
                priority,
                status,
                event: None,
                queue,
            });
        }

        sqlx::query(
            r#"
                update queued_inputs
                set content=$3,
                    updated_at=now(),
                    origin=coalesce(origin, '{}'::jsonb)
                        || jsonb_build_object('edited_at', now()::text)
                where session_id=$1 and id=$2::text and priority='follow_up' and status='queued'
                "#,
        )
        .bind(session_id)
        .bind(input_id)
        .bind(content_value)
        .execute(&mut *tx)
        .await?;
        bump_revisions_tx(&mut tx, session_id, true, false).await?;
        let queue = queue_state_tx(&mut tx, session_id).await?;
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::InputUpdated,
            queue_event_payload(
                &queue,
                json!({
                    "input_id": input_id,
                    "priority": priority,
                    "status": status,
                    "content": content.content.clone(),
                    "content_type": "user_message",
                }),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(UpdateQueuedInputResult {
            input_id: input_id.to_string(),
            updated: true,
            reason: None,
            priority,
            status,
            event: Some(event),
            queue,
        })
    }

    pub async fn cancel_queued_input(
        &self,
        session_id: &str,
        input_id: &str,
        expected_queue_revision: Option<i64>,
    ) -> Result<CancelQueuedInputResult> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        if revision_mismatch_tx(&mut tx, session_id, expected_queue_revision).await? {
            let queue = queue_state_tx(&mut tx, session_id).await?;
            tx.commit().await?;
            return Ok(CancelQueuedInputResult {
                input_id: input_id.to_string(),
                cancelled: false,
                reason: Some(QUEUE_CHANGED.to_string()),
                priority: InputPriority::FollowUp,
                status: QueuedInputStatus::Queued,
                event: None,
                queue,
            });
        }

        let row = sqlx::query(
            r#"
                select priority, status
                from queued_inputs
                where session_id=$1 and id=$2::text
                "#,
        )
        .bind(session_id)
        .bind(input_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(QueueMutationError::not_found(input_id).into());
        };
        let priority = row_text::<InputPriority>(&row, "priority")?;
        let status = row_text::<QueuedInputStatus>(&row, "status")?;
        if priority != InputPriority::FollowUp || status != QueuedInputStatus::Queued {
            let queue = queue_state_tx(&mut tx, session_id).await?;
            tx.commit().await?;
            return Ok(CancelQueuedInputResult {
                input_id: input_id.to_string(),
                cancelled: false,
                reason: Some(NOT_EDITABLE.to_string()),
                priority,
                status,
                event: None,
                queue,
            });
        }

        sqlx::query(
            r#"
                update queued_inputs
                set status='cancelled',
                    follow_up_position=null,
                    updated_at=now(),
                    origin=coalesce(origin, '{}'::jsonb)
                        || jsonb_build_object('cancelled_at', now()::text)
                where session_id=$1 and id=$2::text and priority='follow_up' and status='queued'
                "#,
        )
        .bind(session_id)
        .bind(input_id)
        .execute(&mut *tx)
        .await?;
        renumber_follow_ups_tx(&mut tx, session_id).await?;
        bump_revisions_tx(&mut tx, session_id, true, false).await?;
        let queue = queue_state_tx(&mut tx, session_id).await?;
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::InputCancelled,
            queue_event_payload(
                &queue,
                json!({
                    "input_id": input_id,
                    "priority": priority,
                    "status": QueuedInputStatus::Cancelled,
                }),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(CancelQueuedInputResult {
            input_id: input_id.to_string(),
            cancelled: true,
            reason: None,
            priority,
            status: QueuedInputStatus::Cancelled,
            event: Some(event),
            queue,
        })
    }

    pub async fn reorder_queued_follow_ups(
        &self,
        session_id: &str,
        input_ids: &[String],
        expected_queue_revision: Option<i64>,
    ) -> Result<ReorderQueuedFollowUpsResult> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        if revision_mismatch_tx(&mut tx, session_id, expected_queue_revision).await? {
            let queue = queue_state_tx(&mut tx, session_id).await?;
            let current = queued_follow_up_ids_from_state(&queue);
            tx.commit().await?;
            return Ok(ReorderQueuedFollowUpsResult {
                reordered: false,
                reason: Some(QUEUE_CHANGED.to_string()),
                input_ids: current,
                event: None,
                queue,
            });
        }

        let current = queued_follow_up_ids_tx(&mut tx, session_id).await?;
        if current == input_ids {
            let queue = queue_state_tx(&mut tx, session_id).await?;
            tx.commit().await?;
            return Ok(ReorderQueuedFollowUpsResult {
                reordered: false,
                reason: None,
                input_ids: current,
                event: None,
                queue,
            });
        }
        let mut sorted_current = current.clone();
        sorted_current.sort();
        let mut sorted_provided = input_ids.to_vec();
        sorted_provided.sort();
        if sorted_current != sorted_provided {
            let queue = queue_state_tx(&mut tx, session_id).await?;
            tx.commit().await?;
            return Ok(ReorderQueuedFollowUpsResult {
                reordered: false,
                reason: Some(QUEUE_CHANGED.to_string()),
                input_ids: current,
                event: None,
                queue,
            });
        }

        for (position, input_id) in input_ids.iter().enumerate() {
            sqlx::query(
                r#"
                    update queued_inputs
                    set follow_up_position=$3,
                        updated_at=now()
                    where session_id=$1 and id=$2::text and priority='follow_up' and status='queued'
                    "#,
            )
            .bind(session_id)
            .bind(input_id)
            .bind(position as i32)
            .execute(&mut *tx)
            .await?;
        }
        bump_revisions_tx(&mut tx, session_id, true, false).await?;
        let queue = queue_state_tx(&mut tx, session_id).await?;
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::InputReordered,
            queue_event_payload(
                &queue,
                json!({
                    "input_ids": input_ids,
                }),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(ReorderQueuedFollowUpsResult {
            reordered: true,
            reason: None,
            input_ids: input_ids.to_vec(),
            event: Some(event),
            queue,
        })
    }

    pub async fn has_queued_inputs(&self, session_id: &str) -> Result<bool> {
        let active_queue = queued_input_is_active(None);
        let query = format!(
            "select exists(select 1 from queued_inputs where session_id=$1 and {active_queue})"
        );
        Ok(sqlx::query_scalar(&query)
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn sessions_with_active_queued_inputs(&self) -> Result<Vec<String>> {
        let active_queue = queued_input_is_active(Some("q"));
        let query = format!(
            r#"
                select distinct q.session_id
                from queued_inputs q
                join sessions s on s.id = q.session_id
                left join delegations d on d.id = s.delegation_id
                where {active_queue}
                    and (
                        s.parent_session_id is null
                        or d.status = 'running'
                    )
                order by q.session_id
                "#
        );
        Ok(sqlx::query_scalar(&query).fetch_all(&self.pool).await?)
    }

    pub async fn reset_abandoned_consuming_inputs(&self, session_id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let updated = sqlx::query(
            r#"
                update queued_inputs
                set status='queued',
                    updated_at=now(),
                    origin=(coalesce(origin, '{}'::jsonb) - 'claim_id' - 'claimed_at')
                where session_id=$1 and status='consuming'
                "#,
        )
        .bind(session_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated > 0 {
            renumber_follow_ups_tx(&mut tx, session_id).await?;
            bump_revisions_tx(&mut tx, session_id, true, false).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn reset_consuming_input(
        &self,
        session_id: &str,
        input_id: &str,
        claim_id: &str,
    ) -> Result<()> {
        if claim_id.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let updated = sqlx::query(
            r#"
                update queued_inputs
                set status='queued',
                    updated_at=now(),
                    origin=(coalesce(origin, '{}'::jsonb) - 'claim_id' - 'claimed_at')
                where session_id=$1
                    and id=$2::text
                    and status='consuming'
                    and origin->>'claim_id'=$3
                "#,
        )
        .bind(session_id)
        .bind(input_id)
        .bind(claim_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated > 0 {
            renumber_follow_ups_tx(&mut tx, session_id).await?;
            bump_revisions_tx(&mut tx, session_id, true, false).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

async fn ensure_expected_active_leaf_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    expected_active_leaf_id: Option<Option<&str>>,
) -> Result<()> {
    let Some(expected_active_leaf_id) = expected_active_leaf_id else {
        return Ok(());
    };
    let current_active_leaf_id: Option<String> =
        sqlx::query_scalar("select active_leaf_id from sessions where id=$1")
            .bind(session_id)
            .fetch_one(&mut **tx)
            .await?;
    if current_active_leaf_id.as_deref() != expected_active_leaf_id {
        return Err(ExpectedActiveLeafMismatch::new(
            current_active_leaf_id,
            expected_active_leaf_id.map(str::to_string),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_session::StoredTranscriptEntry;
    use agent_vocab::{
        ProviderConfig, ProviderKind, ReasoningEffort, ToolCall, ToolCallId, UserMessage,
    };
    use serde_json::json;
    use uuid::Uuid;

    use crate::{
        DelegationKind, DelegationStatus, InputPriority, OutputBatch, QueueState, SessionConfig,
    };

    use super::*;

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(20_000);

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
            "pi_relay_queue_mutation_test_{}_{}",
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
            runtime_id: "runtime-test".to_string(),
            workspace_id: "/tmp".to_string(),
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
            .create_project(
                project_id,
                "queue mutation test",
                "runtime-test",
                &[],
                json!({}),
            )
            .await
            .expect("project creates");
        let config = session_config(project_id);
        store
            .create_session(session_id, &config)
            .await
            .expect("session creates");
        config
    }

    fn with_effort(config: &SessionConfig, effort: ReasoningEffort) -> SessionConfig {
        let mut config = config.clone();
        config.provider.reasoning_effort = effort;
        config
    }

    fn queued_follow_up_ids(queue: &QueueState) -> Vec<String> {
        queue
            .queued_inputs
            .iter()
            .filter(|input| input.priority == InputPriority::FollowUp)
            .map(|input| input.input_id.clone())
            .collect()
    }

    fn entry(
        id: &str,
        parent_id: Option<&str>,
        item: agent_vocab::TranscriptItem,
    ) -> agent_session::TranscriptStorageNode {
        agent_session::TranscriptStorageNode {
            id: id.to_string(),
            parent_id: parent_id.map(str::to_string),
            timestamp_ms: 1,
            item,
            provider_replay: Vec::new(),
        }
    }

    async fn stored_effort(store: &PostgresAgentStore, table: &str, id: &str) -> ReasoningEffort {
        let query = format!("select provider_config from {table} where id=$1");
        let value: Value = sqlx::query_scalar(&query)
            .bind(id)
            .fetch_one(&store.pool)
            .await
            .expect("route row");
        serde_json::from_value::<ProviderConfig>(value)
            .expect("provider route")
            .reasoning_effort
    }

    async fn persist_model_action(
        store: &PostgresAgentStore,
        session_id: &str,
        id: &str,
        turn_id: u64,
        route: &ProviderConfig,
    ) -> String {
        let context_entry = entry(
            id,
            None,
            agent_vocab::TranscriptItem::UserMessage(UserMessage::text("route context")),
        );
        let action = agent_session::SessionAction::RequestModel {
            action_id: agent_vocab::ActionId(turn_id),
            turn_id: agent_vocab::TurnId(turn_id),
            model_context: agent_session::ModelContext::new(),
            context_leaf_id: Some(context_entry.id.clone()),
        };
        let (_, persisted) = store
            .persist_outputs(
                session_id,
                OutputBatch::new(
                    std::slice::from_ref(&context_entry),
                    Some(&context_entry.id),
                    &[],
                    &[action],
                )
                .with_provider_route(route.clone().into()),
            )
            .await
            .expect("turn action persists");
        persisted[0].row_id.clone()
    }

    #[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
    #[tokio::test]
    async fn routes_follow_acceptance_open_generation_and_promotion_boundaries() {
        let Some(db) = test_store().await else {
            eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "queue-provider-route";
        let medium = create_session(store, session_id).await;
        let before = store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("before"),
                Some("before-effort"),
                None,
            )
            .await
            .expect("pre-change input enqueues");
        let low = with_effort(&medium, ReasoningEffort::Low);
        store
            .configure_session(session_id, &low)
            .await
            .expect("new default persists while work is queued");
        let after = store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("after"),
                Some("after-effort"),
                None,
            )
            .await
            .expect("post-change input enqueues");
        assert_eq!(
            stored_effort(store, "queued_inputs", &before.input_id).await,
            ReasoningEffort::Medium
        );
        assert_eq!(
            stored_effort(store, "queued_inputs", &after.input_id).await,
            ReasoningEffort::Low
        );

        store
            .promote_queued_input(session_id, &before.input_id)
            .await
            .expect("idle promotion");
        assert_eq!(
            stored_effort(store, "queued_inputs", &before.input_id).await,
            ReasoningEffort::Medium,
            "promotion without open work preserves submission route"
        );

        let action_id =
            persist_model_action(store, session_id, "entry-route", 1, &medium.provider).await;
        let (_, tool_actions) = store
            .persist_outputs(
                session_id,
                OutputBatch::new(
                    &[],
                    None,
                    &[],
                    &[agent_session::SessionAction::RequestTool {
                        action_id: agent_vocab::ActionId(2),
                        turn_id: agent_vocab::TurnId(1),
                        tool_call: ToolCall {
                            id: ToolCallId::from_u64(1),
                            tool_name: "Bash".to_string(),
                            args_json: r#"{"command":"true"}"#.to_string(),
                        },
                    }],
                )
                .with_unchanged_active_leaf()
                .with_provider_route(medium.provider.clone().into()),
            )
            .await
            .expect("tool action persists with the open generation route");
        let recovered = store
            .pending_actions_for_dispatch(session_id)
            .await
            .expect("recover pending model and tool actions");
        assert!(
            recovered.iter().all(|action| {
                let mut restored = with_effort(&medium, ReasoningEffort::High);
                action.route.apply_to(&mut restored);
                restored.provider.reasoning_effort == ReasoningEffort::Medium
            }),
            "recovered model and tool dispatches retain one generation route"
        );
        assert_eq!(
            stored_effort(store, "actions", &tool_actions[0].row_id).await,
            ReasoningEffort::Medium,
            "fresh tool continuation stores its route"
        );
        store
            .promote_queued_input(session_id, &after.input_id)
            .await
            .expect("active promotion");
        assert_eq!(
            stored_effort(store, "queued_inputs", &after.input_id).await,
            ReasoningEffort::Medium,
            "promotion into open work adopts its route"
        );
        let steer = store
            .enqueue_user_input(
                session_id,
                InputPriority::Steer,
                &UserMessage::text("steer current turn"),
                Some("steer-effort"),
                None,
            )
            .await
            .expect("steer enqueues");
        assert_eq!(
            stored_effort(store, "queued_inputs", &steer.input_id).await,
            ReasoningEffort::Medium
        );

        sqlx::query("update actions set status='completed' where id in ($1, $2)")
            .bind(action_id)
            .bind(&tool_actions[0].row_id)
            .execute(&store.pool)
            .await
            .expect("complete historical generation");
        let idle = store
            .enqueue_user_input(
                session_id,
                InputPriority::Steer,
                &UserMessage::text("idle steer"),
                Some("idle-steer"),
                None,
            )
            .await
            .expect("idle steer enqueues");
        assert_eq!(
            stored_effort(store, "queued_inputs", &idle.input_id).await,
            ReasoningEffort::Low,
            "completed history is never an active steer route"
        );

        db.cleanup().await;
    }

    #[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
    #[tokio::test]
    async fn config_update_freezes_legacy_active_rows_and_rejects_mixed_open_routes() {
        let Some(db) = test_store().await else {
            eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "legacy-provider-route";
        let medium = create_session(store, session_id).await;
        let queued = store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("legacy"),
                Some("legacy"),
                None,
            )
            .await
            .expect("input enqueues");
        let action_id =
            persist_model_action(store, session_id, "entry-legacy", 1, &medium.provider).await;
        sqlx::query("update queued_inputs set provider_config=null where id=$1")
            .bind(&queued.input_id)
            .execute(&store.pool)
            .await
            .expect("simulate rollback-era queue insert");
        sqlx::query("update actions set provider_config=null where id=$1")
            .bind(&action_id)
            .execute(&store.pool)
            .await
            .expect("simulate rollback-era action insert");
        let low = with_effort(&medium, ReasoningEffort::Low);
        store
            .configure_session(session_id, &low)
            .await
            .expect("configuration freezes old default");
        assert_eq!(
            stored_effort(store, "queued_inputs", &queued.input_id).await,
            ReasoningEffort::Medium
        );
        assert_eq!(
            stored_effort(store, "actions", &action_id).await,
            ReasoningEffort::Medium
        );

        let other = "action-mixed";
        sqlx::query(
            r#"
            insert into actions (
                id, session_id, turn_id, action_id, attempt_id, kind, status, payload,
                provider_config
            )
            select $2, session_id, turn_id, action_id + 1, 'attempt-mixed', kind, status,
                payload, $3
            from actions
            where id=$1
            "#,
        )
        .bind(&action_id)
        .bind(other)
        .bind(serde_json::to_value(&low.provider).expect("serialize low route"))
        .execute(&store.pool)
        .await
        .expect("inject inconsistent generation route");
        let error = match store
            .enqueue_user_input(
                session_id,
                InputPriority::Steer,
                &UserMessage::text("mixed"),
                Some("mixed"),
                None,
            )
            .await
        {
            Ok(_) => panic!("mixed generation routes must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("inconsistent provider routes"));
        sqlx::query("update actions set status='completed' where id in ($1, $2)")
            .bind(action_id)
            .bind(other)
            .execute(&store.pool)
            .await
            .expect("complete rows");

        db.cleanup().await;
    }

    #[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
    #[tokio::test]
    async fn enqueue_validates_expected_active_leaf_under_session_lock_and_lists_active_queue() {
        let Some(db) = test_store().await else {
            eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "queue-expected-leaf";
        create_session(store, session_id).await;
        let root = entry(
            "entry_root",
            None,
            agent_vocab::TranscriptItem::CompactionSummary(agent_vocab::CompactionSummary::new(
                session_id,
                "source",
                "summary",
                None,
                agent_vocab::TurnId(0),
            )),
        );
        store
            .recover_session(
                session_id,
                &[StoredTranscriptEntry {
                    id: root.id.clone(),
                    parent_id: root.parent_id.clone(),
                    timestamp_ms: root.timestamp_ms,
                    item: root.item.clone(),
                    provider_replay: Vec::new(),
                }],
                Some(&root.id),
            )
            .await
            .expect("seed transcript");

        let stale = store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("stale"),
                Some("stale-client-input"),
                Some(Some("not-the-active-leaf")),
            )
            .await
            .err()
            .expect("stale expected active leaf is rejected");
        assert!(stale.downcast_ref::<ExpectedActiveLeafMismatch>().is_some());

        let queued = store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("matched"),
                Some("matched-client-input"),
                Some(Some(&root.id)),
            )
            .await
            .expect("matching expected active leaf enqueues");
        assert_eq!(queued.queue.as_ref().expect("queue").queued_inputs.len(), 1);
        assert!(store
            .sessions_with_active_queued_inputs()
            .await
            .expect("queued sessions")
            .contains(&session_id.to_string()));

        let replay = store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("matched"),
                Some("matched-client-input"),
                Some(Some("not-the-active-leaf")),
            )
            .await
            .expect("idempotent replay returns accepted input despite stale expected leaf");
        assert_eq!(replay.input_id, queued.input_id);
        assert!(replay.event.is_none());

        db.cleanup().await;
    }

    #[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
    #[tokio::test]
    async fn boot_active_queue_sweep_skips_subagents_of_non_running_delegations() {
        let Some(db) = test_store().await else {
            eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let project_id = Uuid::new_v4();
        store
            .create_project(
                project_id,
                "boot queue sweep test",
                "runtime-test",
                &[],
                json!({}),
            )
            .await
            .expect("project creates");
        let config = session_config(project_id);
        store
            .create_session("main_with_queue", &config)
            .await
            .expect("main session creates");
        store
            .create_session("parent", &config)
            .await
            .expect("parent session creates");

        let cancelled = store
            .create_delegation("parent", DelegationKind::Full, None, Some("cancelled"), 1)
            .await
            .expect("cancelled delegation creates");
        store
            .set_delegation_status(&cancelled.id, DelegationStatus::Cancelled)
            .await
            .expect("cancel delegation");
        let done = store
            .create_delegation("parent", DelegationKind::Full, None, Some("done"), 1)
            .await
            .expect("done delegation creates");
        store
            .set_delegation_status(&done.id, DelegationStatus::Done)
            .await
            .expect("finish delegation");
        let failed = store
            .create_delegation("parent", DelegationKind::Full, None, Some("failed"), 1)
            .await
            .expect("failed delegation creates");
        store
            .set_delegation_status(&failed.id, DelegationStatus::Failed)
            .await
            .expect("fail delegation");
        let running = store
            .create_delegation("parent", DelegationKind::Full, None, Some("running"), 1)
            .await
            .expect("running delegation creates");

        for (session_id, delegation_id) in [
            ("running_child", running.id.as_str()),
            ("cancelled_child", cancelled.id.as_str()),
            ("done_child", done.id.as_str()),
            ("failed_child", failed.id.as_str()),
        ] {
            store
                .start_session_outputs_with_parent(
                    session_id,
                    &config,
                    &[],
                    None,
                    &[],
                    &[],
                    InputPriority::FollowUp,
                    &UserMessage::text("start"),
                    None,
                    Some("parent"),
                    Some(crate::SubagentType::Full),
                    Some(delegation_id),
                )
                .await
                .expect("subagent session creates");
        }
        store
            .start_session_outputs_with_parent(
                "legacy_child_without_delegation",
                &config,
                &[],
                None,
                &[],
                &[],
                InputPriority::FollowUp,
                &UserMessage::text("start"),
                None,
                Some("parent"),
                Some(crate::SubagentType::Full),
                None,
            )
            .await
            .expect("legacy subagent session creates");

        for session_id in [
            "main_with_queue",
            "running_child",
            "cancelled_child",
            "done_child",
            "failed_child",
            "legacy_child_without_delegation",
        ] {
            store
                .enqueue_user_input(
                    session_id,
                    InputPriority::FollowUp,
                    &UserMessage::text("queued"),
                    Some(&format!("{session_id}-client-input")),
                    None,
                )
                .await
                .expect("queued input enqueues");
        }

        let sessions = store
            .sessions_with_active_queued_inputs()
            .await
            .expect("queued sessions load");
        assert_eq!(sessions, vec!["main_with_queue", "running_child"]);

        db.cleanup().await;
    }

    #[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
    #[tokio::test]
    async fn queued_follow_ups_can_be_reordered_edited_and_cancelled() {
        let Some(db) = test_store().await else {
            eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "queue-mutate";
        create_session(store, session_id).await;

        let first = store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("first"),
                None,
                None,
            )
            .await
            .expect("first input enqueues");
        let second = store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("second"),
                None,
                None,
            )
            .await
            .expect("second input enqueues");
        let third = store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("third"),
                None,
                None,
            )
            .await
            .expect("third input enqueues");
        let queue = third.queue.clone().expect("enqueue returns queue state");
        let revision = queue.queue_revision;

        let reordered = store
            .reorder_queued_follow_ups(
                session_id,
                &[
                    third.input_id.clone(),
                    first.input_id.clone(),
                    second.input_id.clone(),
                ],
                Some(revision),
            )
            .await
            .expect("reorder succeeds");
        assert!(reordered.reordered);
        assert_eq!(
            queued_follow_up_ids(&reordered.queue),
            vec![
                third.input_id.clone(),
                first.input_id.clone(),
                second.input_id.clone()
            ]
        );

        let updated = store
            .update_queued_input(
                session_id,
                &first.input_id,
                &UserMessage::text("first edited"),
                Some(reordered.queue.queue_revision),
            )
            .await
            .expect("update succeeds");
        assert!(updated.updated);
        assert_eq!(
            updated
                .queue
                .queued_inputs
                .iter()
                .find(|input| input.input_id == first.input_id)
                .and_then(|input| input.content.as_text()),
            Some("first edited")
        );

        let cancelled = store
            .cancel_queued_input(
                session_id,
                &third.input_id,
                Some(updated.queue.queue_revision),
            )
            .await
            .expect("cancel succeeds");
        assert!(cancelled.cancelled);
        assert_eq!(
            queued_follow_up_ids(&cancelled.queue),
            vec![first.input_id.clone(), second.input_id.clone()]
        );
        assert_eq!(
            cancelled
                .queue
                .queued_inputs
                .iter()
                .filter_map(|input| input.follow_up_position)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        db.cleanup().await;
    }

    #[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
    #[tokio::test]
    async fn stale_revision_and_steering_mutations_return_canonical_queue() {
        let Some(db) = test_store().await else {
            eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "queue-stale";
        create_session(store, session_id).await;

        let follow_up = store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("follow"),
                None,
                None,
            )
            .await
            .expect("follow-up enqueues");
        let stale_revision = follow_up
            .queue
            .as_ref()
            .expect("enqueue returns queue")
            .queue_revision;
        let steer = store
            .enqueue_user_input(
                session_id,
                InputPriority::Steer,
                &UserMessage::text("steer"),
                None,
                None,
            )
            .await
            .expect("steer enqueues");

        let stale = store
            .update_queued_input(
                session_id,
                &follow_up.input_id,
                &UserMessage::text("stale edit"),
                Some(stale_revision),
            )
            .await
            .expect("stale edit returns queue");
        assert!(!stale.updated);
        assert_eq!(stale.reason.as_deref(), Some(QUEUE_CHANGED));
        assert_eq!(
            stale.queue.queue_revision,
            steer.queue.as_ref().expect("queue").queue_revision
        );

        let not_editable = store
            .cancel_queued_input(session_id, &steer.input_id, None)
            .await
            .expect("steer cancel returns not editable");
        assert!(!not_editable.cancelled);
        assert_eq!(not_editable.reason.as_deref(), Some(NOT_EDITABLE));
        assert_eq!(not_editable.priority, InputPriority::Steer);
        db.cleanup().await;
    }

    #[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
    #[tokio::test]
    async fn stale_queued_consumption_and_stale_active_leaf_are_rejected() {
        let Some(db) = test_store().await else {
            eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "queue-consume-fence";
        create_session(store, session_id).await;

        let first = store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("first"),
                None,
                None,
            )
            .await
            .expect("first input enqueues");
        store
            .enqueue_user_input(
                session_id,
                InputPriority::FollowUp,
                &UserMessage::text("second"),
                None,
                None,
            )
            .await
            .expect("second input enqueues");
        let taken = store
            .take_next_queued_input(session_id)
            .await
            .expect("take succeeds")
            .expect("input exists");
        assert_eq!(taken.id, first.input_id);

        store
            .enqueue_user_input(
                session_id,
                InputPriority::Steer,
                &UserMessage::text("steer"),
                None,
                None,
            )
            .await
            .expect("steer enqueues");
        let consumed = store
            .persist_outputs(
                session_id,
                OutputBatch::new(&[], None, &[], &[]).with_consumed_input(Some(taken)),
            )
            .await;
        assert!(consumed.is_err());

        let root = entry(
            "entry_root",
            None,
            agent_vocab::TranscriptItem::CompactionSummary(agent_vocab::CompactionSummary::new(
                session_id,
                "source",
                "summary",
                None,
                agent_vocab::TurnId(0),
            )),
        );
        store
            .recover_session(
                session_id,
                &[StoredTranscriptEntry {
                    id: root.id.clone(),
                    parent_id: root.parent_id.clone(),
                    timestamp_ms: root.timestamp_ms,
                    item: root.item.clone(),
                    provider_replay: Vec::new(),
                }],
                None,
            )
            .await
            .expect("seed transcript");
        let stale_append = store
            .persist_outputs(
                session_id,
                OutputBatch::new(
                    &[entry(
                        "entry_child",
                        Some(&root.id),
                        agent_vocab::TranscriptItem::UserMessage(UserMessage::text("late")),
                    )],
                    Some("entry_child"),
                    &[],
                    &[],
                ),
            )
            .await;
        assert!(stale_append.is_err());

        db.cleanup().await;
    }
}
