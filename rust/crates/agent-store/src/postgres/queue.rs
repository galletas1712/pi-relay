use agent_vocab::UserMessage;
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    EnqueueUserInputResult, EventType, InputPriority, InputRecord, PromoteQueuedInputResult,
    QueueMutationError, QueuedInput, QueuedInputPreview, QueuedInputStatus,
};

use super::events::insert_event_with_activity_tx;
use super::rows::row_text;
use super::sql::{queued_input_is_active, queued_input_is_editable, QUEUED_INPUT_DISPATCH_ORDER};
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn enqueue_user_input(
        &self,
        session_id: &str,
        priority: InputPriority,
        content: &UserMessage,
        client_input_id: Option<&str>,
    ) -> Result<EnqueueUserInputResult> {
        let id = format!("input_{}", Uuid::new_v4());
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            r#"
                insert into queued_inputs (id, session_id, priority, content, status, client_input_id, origin)
                values (
                    $1,
                    $2,
                    $3,
                    $4,
                    'queued',
                    $5,
                    case
                        when $3 = 'steer' then jsonb_build_object('promoted_at', now()::text)
                        else null
                    end
                )
                on conflict (session_id, client_input_id) where client_input_id is not null
                do nothing
                returning id
                "#,
        )
        .bind(&id)
        .bind(session_id)
        .bind(priority.as_str())
        .bind(serde_json::to_value(content)?)
        .bind(client_input_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(inserted) = inserted else {
            let row = sqlx::query(
                "select id from queued_inputs where session_id=$1 and client_input_id=$2::text",
            )
            .bind(session_id)
            .bind(client_input_id)
            .fetch_one(&mut *tx)
            .await?;
            let input_id = row.get("id");
            tx.commit().await?;
            return Ok(EnqueueUserInputResult {
                input_id,
                event: None,
            });
        };

        let input_id = inserted.get("id");
        let event = insert_event_with_activity_tx(
            &mut tx,
            session_id,
            EventType::InputQueued,
            json!({
                "input_id": input_id,
                "priority": priority,
                "client_input_id": client_input_id,
                "content": content,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(EnqueueUserInputResult {
            input_id,
            event: Some(event),
        })
    }

    async fn peek_next_queued_input_matching(
        &self,
        session_id: &str,
        priority: Option<InputPriority>,
    ) -> Result<Option<QueuedInputPreview>> {
        let editable_queue = queued_input_is_editable(None);
        let priority_filter = priority.map(|priority| priority.as_str().to_string());
        let query = format!(
            r#"
                select id, priority, content, client_input_id
                from queued_inputs
                where session_id=$1
                    and {editable_queue}
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
        row.map(|row| {
            let content: UserMessage = serde_json::from_value(row.get::<Value, _>("content"))?;
            Ok(QueuedInputPreview {
                id: row.get("id"),
                priority: row_text::<InputPriority>(&row, "priority")?,
                content,
                client_input_id: row.get("client_input_id"),
            })
        })
        .transpose()
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

    pub async fn peek_next_queued_input(
        &self,
        session_id: &str,
    ) -> Result<Option<QueuedInputPreview>> {
        self.peek_next_queued_input_matching(session_id, None).await
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
        let claim_id = Uuid::new_v4().to_string();
        let mut tx = self.pool.begin().await?;
        let editable_queue = queued_input_is_editable(None);
        let priority_filter = priority.map(|priority| priority.as_str().to_string());
        let query = format!(
            r#"
                update queued_inputs
                set status='consuming',
                    origin=coalesce(origin, '{{}}'::jsonb)
                        || jsonb_build_object('claim_id', $2::text, 'claimed_at', now()::text)
                where id = (
                    select id
                    from queued_inputs
                    where session_id=$1
                        and {editable_queue}
                        and ($3::text is null or priority=$3::text)
                    order by {QUEUED_INPUT_DISPATCH_ORDER}
                    limit 1
                    for update skip locked
                )
                returning id, priority, content, client_input_id
                "#
        );
        let row = sqlx::query(&query)
            .bind(session_id)
            .bind(&claim_id)
            .bind(priority_filter.as_deref())
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let id: String = row.get("id");
        let content: UserMessage = serde_json::from_value(row.get::<Value, _>("content"))?;
        let priority = row_text::<InputPriority>(&row, "priority")?;
        tx.commit().await?;
        Ok(Some(QueuedInput {
            id,
            priority,
            content,
            client_input_id: row.get("client_input_id"),
            claim_id,
        }))
    }

    pub async fn promote_queued_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<PromoteQueuedInputResult> {
        let mut tx = self.pool.begin().await?;
        let editable_queue = queued_input_is_editable(None);
        let query = format!(
            r#"
                update queued_inputs
                set priority='steer',
                    origin=coalesce(origin, '{{}}'::jsonb)
                        || jsonb_build_object('promoted_at', now()::text)
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
            let result = PromoteQueuedInputResult {
                input_id: input_id.to_string(),
                priority: row_text::<InputPriority>(&row, "priority")?,
                status: row_text::<QueuedInputStatus>(&row, "status")?,
                promoted: false,
                event: None,
            };
            tx.commit().await?;
            return Ok(result);
        };
        let event = insert_event_with_activity_tx(
            &mut tx,
            session_id,
            EventType::InputPromoted,
            json!({
                "input_id": input_id,
                "priority": InputPriority::Steer,
                "client_input_id": row.get::<Option<String>, _>("client_input_id"),
                "content": row.get::<Value, _>("content"),
                "promoted_at": row.get::<Option<String>, _>("promoted_at"),
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

    pub async fn reset_abandoned_consuming_inputs(&self, session_id: &str) -> Result<()> {
        sqlx::query(
            r#"
                update queued_inputs
                set status='queued',
                    origin=(coalesce(origin, '{}'::jsonb) - 'claim_id' - 'claimed_at')
                where session_id=$1 and status='consuming'
                "#,
        )
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn reset_consuming_input(
        &self,
        session_id: &str,
        input_id: &str,
        claim_id: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
                update queued_inputs
                set status='queued',
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
