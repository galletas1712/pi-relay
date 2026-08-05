use anyhow::Result;
use serde_json::json;

use crate::{CreateForkRequest, EventType, ForkSessionResult};

use super::events::insert_event_tx;
use super::history_target::ensure_leaf_is_turn_boundary_tx;
use super::mcp::install_session_manifest_tx;
use super::sql::{ensure_no_active_work_tx, ensure_no_running_delegation_tx, lock_session_tx};
use super::transcript::session_state_for_event_tx;
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn create_fork(&self, request: CreateForkRequest<'_>) -> Result<ForkSessionResult> {
        let CreateForkRequest {
            source_session_id,
            child_session_id,
            config,
        } = request;
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, source_session_id).await?;
        ensure_no_running_delegation_tx(&mut tx, source_session_id).await?;
        ensure_no_active_work_tx(&mut tx, source_session_id).await?;
        // The source row lock above makes this read of the source's current
        // active leaf race-safe: the child duplicates the source as it is now.
        let (active_leaf_id, source_fingerprint): (Option<String>, Option<String>) =
            sqlx::query_as(
                "select active_leaf_id, mcp_manifest_fingerprint from sessions where id=$1",
            )
            .bind(source_session_id)
            .fetch_one(&mut *tx)
            .await?;
        // A mid-turn tail can survive `ensure_idle_without_recovery` (an already
        // active session skips recovery). Duplicating one would leave the child
        // ready to continue and spontaneously drive a model turn on first touch.
        ensure_leaf_is_turn_boundary_tx(&mut tx, source_session_id, active_leaf_id.as_deref())
            .await?;
        if source_fingerprint.as_deref()
            != config
                .mcp_manifest
                .as_ref()
                .map(|binding| binding.manifest_fingerprint.as_str())
        {
            return Err(crate::SessionConfigChanged.into());
        }
        if let Some(binding) = &config.mcp_manifest {
            install_session_manifest_tx(&mut tx, binding).await?;
        }
        sqlx::query(
            r#"
            insert into sessions (
                id, project_id, runtime_id, workspace_id, workspaces, active_leaf_id,
                system_prompt, provider_config, metadata, mcp_manifest_fingerprint,
                session_revision, transcript_revision
            )
            values ($1, $2, $3, $4, $5, $6::text, $7, $8, $9, $10::text, 1, 1)
            "#,
        )
        .bind(child_session_id)
        .bind(config.project_id)
        .bind(&config.runtime_id)
        .bind(&config.workspace_id)
        .bind(serde_json::to_value(&config.workspaces)?)
        .bind(active_leaf_id.as_deref())
        .bind(&config.system_prompt)
        .bind(serde_json::to_value(&config.provider)?)
        .bind(&config.metadata)
        .bind(
            config
                .mcp_manifest
                .as_ref()
                .map(|binding| &binding.manifest_fingerprint),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            insert into transcript_entries (
                session_id, id, parent_id, timestamp_ms, item, provider_replay, turn_id
            )
            select $2::text, id, parent_id, timestamp_ms, item, provider_replay, turn_id
            from transcript_entries
            where session_id=$1
            order by sequence
            "#,
        )
        .bind(source_session_id)
        .bind(child_session_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            update sessions
            set last_user_message_timestamp_ms = (
                select max(timestamp_ms)
                from transcript_entries
                where session_id=$1 and item->>'type' = 'user_message'
            )
            where id=$1
            "#,
        )
        .bind(child_session_id)
        .execute(&mut *tx)
        .await?;
        let state = session_state_for_event_tx(&mut tx, child_session_id).await?;
        let event = insert_event_tx(
            &mut tx,
            child_session_id,
            EventType::SessionCreated,
            json!({
                "session_id": child_session_id,
                "project_id": config.project_id,
                "provider": config.provider,
                "active_leaf_id": &active_leaf_id,
                "source_session_id": source_session_id,
                "session_revision": state.session_revision,
                "queue_revision": state.queue_revision,
                "transcript_revision": state.transcript_revision,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(ForkSessionResult {
            session_id: child_session_id.to_string(),
            source_session_id: source_session_id.to_string(),
            active_leaf_id,
            session_revision: state.session_revision,
            queue_revision: state.queue_revision,
            transcript_revision: state.transcript_revision,
            last_event_id: event.event_id,
            events: vec![event],
        })
    }
}

#[cfg(test)]
#[path = "history_fork_tests.rs"]
mod tests;
