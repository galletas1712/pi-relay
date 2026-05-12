use agent_vocab::ProviderConfig;
use anyhow::Result;
use serde_json::Value;
use sqlx::Row;

use crate::{
    ActionKind, ActionStatus, PendingActionRecord, QueuedInputRecord, QueuedInputStatus,
    SessionActivity, SessionSnapshot,
};

use super::rows::{queued_input_record_content, row_text};
use super::sql::{action_is_unfinished, queued_input_is_active, QUEUED_INPUT_DISPATCH_ORDER};
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn session_snapshot(&self, session_id: &str) -> Result<SessionSnapshot> {
        let session = sqlx::query(
            "select id, active_leaf_id, provider_config, metadata from sessions where id=$1",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        let provider: ProviderConfig =
            serde_json::from_value(session.get::<Value, _>("provider_config"))?;

        let unfinished_actions = action_is_unfinished(None);
        let actions_query = format!(
            "select id, kind, status, payload from actions where session_id=$1 and {unfinished_actions} order by created_at"
        );
        let actions = sqlx::query(&actions_query)
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?;

        let last_event_id: i64 = sqlx::query_scalar(
            "select coalesce(max(id),0)::bigint from events where session_id=$1",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;

        let active_queue = queued_input_is_active(None);
        let queued_query = format!(
            "select exists(select 1 from queued_inputs where session_id=$1 and {active_queue})"
        );
        let queued: bool = sqlx::query_scalar(&queued_query)
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?;

        let active_queue = queued_input_is_active(None);
        let queued_inputs_query = format!(
            r#"
                select id,
                    priority,
                    status,
                    content,
                    client_input_id,
                    created_at::text as created_at,
                    origin->>'promoted_at' as promoted_at
                from queued_inputs
                where session_id=$1 and {active_queue}
                order by {QUEUED_INPUT_DISPATCH_ORDER}
                "#
        );
        let queued_inputs = sqlx::query(&queued_inputs_query)
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?;

        let activity = if !actions.is_empty() {
            SessionActivity::Running
        } else if queued {
            SessionActivity::Queued
        } else {
            SessionActivity::Idle
        };
        Ok(SessionSnapshot {
            session_id: session.get("id"),
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
            queued_inputs: queued_inputs
                .into_iter()
                .map(|row| {
                    let content_value = row.get::<Value, _>("content");
                    Ok(QueuedInputRecord {
                        input_id: row.get("id"),
                        priority: row_text(&row, "priority")?,
                        status: row_text::<QueuedInputStatus>(&row, "status")?,
                        content: queued_input_record_content(content_value)?,
                        client_input_id: row.get("client_input_id"),
                        created_at: row.get("created_at"),
                        promoted_at: row.get("promoted_at"),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            last_event_id,
        })
    }
}
