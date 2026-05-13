use anyhow::{anyhow, Result};
use serde_json::json;
use sqlx::Row;

use crate::{ActionKind, ActionStatus, EventFrame, EventType, ResumableModelAction, StoredAction};

use super::events::insert_event_tx;
use super::rows::row_text;
use super::sql::action_is_unfinished;
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn mark_all_unfinished_actions_stale(&self) -> Result<u64> {
        let unfinished_actions = action_is_unfinished(None);
        let query = format!(
            "update actions set status='stale', updated_at=now() where {unfinished_actions}",
        );
        Ok(sqlx::query(&query)
            .execute(&self.pool)
            .await?
            .rows_affected())
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

    pub async fn load_action(&self, session_id: &str, action_row_id: &str) -> Result<StoredAction> {
        let unfinished_actions = action_is_unfinished(None);
        let query = format!(
            "select kind, action_id, turn_id, attempt_id from actions where session_id=$1 and id=$2::text and {unfinished_actions}",
        );
        let row = sqlx::query(&query)
            .bind(session_id)
            .bind(action_row_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("action not found or not running: {action_row_id}"))?;
        Ok(StoredAction {
            kind: row_text::<ActionKind>(&row, "kind")?,
            action_id: row.get("action_id"),
            turn_id: row.get("turn_id"),
            attempt_id: row.get("attempt_id"),
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
            select action_id, turn_id, status, payload
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
        let context_tokens = payload
            .get("context_tokens")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize);
        Ok(Some(ResumableModelAction {
            action_id: agent_vocab::ActionId(row.get::<i64, _>("action_id") as u64),
            turn_id: agent_vocab::TurnId(row.get::<i64, _>("turn_id") as u64),
            status: row_text::<ActionStatus>(&row, "status")?,
            context_leaf_id,
            context_tokens,
        }))
    }

    pub async fn action_can_complete(
        &self,
        session_id: &str,
        action_row_id: &str,
        attempt_id: &str,
    ) -> Result<bool> {
        let unfinished_actions = action_is_unfinished(None);
        let query = format!(
            r#"
                select exists(
                    select 1
                    from actions
                    where session_id=$1 and id=$2::text and attempt_id=$3::text and {unfinished_actions}
                )
                "#
        );
        Ok(sqlx::query_scalar(&query)
            .bind(session_id)
            .bind(action_row_id)
            .bind(attempt_id)
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
        let unfinished_actions = action_is_unfinished(None);
        let query = format!(
            "update actions set status='running', updated_at=now() where session_id=$1 and id=$2::text and attempt_id=$3::text and {unfinished_actions}",
        );
        let updated = sqlx::query(&query)
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
        let event = insert_event_tx(
            &mut tx,
            session_id,
            event_type,
            json!({ "action_row_id": action_row_id }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn mark_action_stale(&self, session_id: &str, action_row_id: &str) -> Result<()> {
        let unfinished_actions = action_is_unfinished(None);
        let query = format!(
            "update actions set status='stale', updated_at=now() where session_id=$1 and id=$2::text and {unfinished_actions}",
        );
        sqlx::query(&query)
            .bind(session_id)
            .bind(action_row_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
