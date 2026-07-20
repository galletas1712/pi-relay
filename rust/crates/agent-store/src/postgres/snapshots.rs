use agent_vocab::ProviderConfig;
use anyhow::Result;
use serde_json::Value;
use sqlx::Row;

use crate::{
    ActionKind, ActionStatus, PendingActionRecord, SessionActivity, SessionSnapshot,
    SessionWorkspace,
};

use super::queue::queue_state_tx;
use super::rows::row_text;
use super::sql::action_is_unfinished;
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn session_snapshot(&self, session_id: &str) -> Result<SessionSnapshot> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read read only")
            .execute(&mut *tx)
            .await?;
        let session = sqlx::query(
            r#"
            select id, project_id, parent_session_id, runtime_id, workspace_id, workspaces, active_leaf_id,
                provider_config, metadata, last_user_message_timestamp_ms
            from sessions
            where id=$1
            "#,
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;
        let provider: ProviderConfig =
            serde_json::from_value(session.get::<Value, _>("provider_config"))?;

        let unfinished_actions = action_is_unfinished(None);
        let actions_query = format!(
            "select id, kind, status, payload from actions where session_id=$1 and {unfinished_actions} order by created_at"
        );
        let actions = sqlx::query(&actions_query)
            .bind(session_id)
            .fetch_all(&mut *tx)
            .await?;

        let last_event_id: i64 = sqlx::query_scalar(
            "select coalesce(max(id),0)::bigint from events where session_id=$1",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;

        let has_transcript_entries: bool = sqlx::query_scalar(
            "select exists(select 1 from transcript_entries where session_id=$1)",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;

        let queue = queue_state_tx(&mut tx, session_id).await?;

        tx.commit().await?;

        let activity = if !actions.is_empty() {
            SessionActivity::Running
        } else {
            queue.activity
        };
        Ok(SessionSnapshot {
            session_id: session.get("id"),
            project_id: session.get("project_id"),
            parent_session_id: session.get("parent_session_id"),
            runtime_id: session.get("runtime_id"),
            workspace_id: session.get("workspace_id"),
            workspaces: serde_json::from_value::<Vec<SessionWorkspace>>(
                session.get::<Value, _>("workspaces"),
            )?,
            activity,
            active_leaf_id: session.get("active_leaf_id"),
            provider,
            metadata: session.get("metadata"),
            pending_actions: actions
                .into_iter()
                .map(|row| {
                    Ok(PendingActionRecord {
                        action_row_id: row.get("id"),
                        kind: row_text::<ActionKind>(&row, "kind")?,
                        status: row_text::<ActionStatus>(&row, "status")?,
                        payload: row.get("payload"),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            queued_inputs: queue.queued_inputs,
            session_revision: queue.session_revision,
            queue_revision: queue.queue_revision,
            transcript_revision: queue.transcript_revision,
            last_event_id,
            last_user_message_timestamp_ms: session.get("last_user_message_timestamp_ms"),
            has_transcript_entries,
        })
    }
}
