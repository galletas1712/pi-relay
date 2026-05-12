use anyhow::{anyhow, Result};
use serde_json::json;
use sqlx::Row;

use crate::{ActionKind, EventFrame, EventType, StoredAction};

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
