use anyhow::Result;
use serde_json::json;

use crate::{
    CreateContextForkRequest, CreateForkRequest, EventType, ForkSessionResult, InputPriority,
    QueuedInputContent,
};

use super::events::insert_event_tx;
use super::history_target::validate_history_target_tx;
use super::mcp::install_session_manifest_tx;
use super::queue::{
    append_queued_content_event_fields, bump_revisions_tx, queue_event_payload, queue_state_tx,
};
use super::sql::lock_session_tx;
use super::transcript::session_state_for_event_tx;
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn create_context_fork(
        &self,
        request: CreateContextForkRequest<'_>,
    ) -> Result<ForkSessionResult> {
        let CreateContextForkRequest {
            source_session_id,
            child_session_id,
            reservation_owner_id,
            config,
            parent_session_id,
            subagent_type,
            delegation_id,
            task,
        } = request;
        if source_session_id != parent_session_id {
            return Err(anyhow::anyhow!(
                "context fork source and parent session must match"
            ));
        }
        let mut tx = self.pool.begin().await?;
        if subagent_type == crate::SubagentType::ReadOnly {
            let Some(reservation_owner_id) = reservation_owner_id else {
                return Err(anyhow::anyhow!(
                    "read-only context fork requires a workspace reservation"
                ));
            };
            let reservation_deleted = sqlx::query(
                r#"
                delete from context_fork_workspace_reservations
                where child_session_id=$1 and parent_session_id=$2 and runtime_id=$3
                  and workspace_id=$4 and owner_id=$5 and state='materializing'
                "#,
            )
            .bind(child_session_id)
            .bind(parent_session_id)
            .bind(&config.runtime_id)
            .bind(&config.workspace_id)
            .bind(reservation_owner_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if reservation_deleted != 1 {
                return Err(anyhow::anyhow!(
                    "context fork workspace reservation is missing or does not match child"
                ));
            }
        }
        lock_session_tx(&mut tx, source_session_id).await?;
        let source_fingerprint: Option<String> =
            sqlx::query_scalar("select mcp_manifest_fingerprint from sessions where id=$1")
                .bind(source_session_id)
                .fetch_one(&mut *tx)
                .await?;
        if source_fingerprint.as_deref()
            != config
                .mcp_manifest
                .as_ref()
                .map(|binding| binding.manifest_fingerprint.as_str())
        {
            return Err(crate::SessionConfigChanged.into());
        }
        let source_leaf_id: Option<String> =
            sqlx::query_scalar("select active_leaf_id from sessions where id=$1")
                .bind(source_session_id)
                .fetch_one(&mut *tx)
                .await?;
        if let Some(binding) = &config.mcp_manifest {
            install_session_manifest_tx(&mut tx, binding).await?;
        }
        sqlx::query(
            r#"
            insert into sessions (
                id, project_id, runtime_id, workspace_id, workspaces, active_leaf_id,
                system_prompt, provider_config, metadata, parent_session_id,
                subagent_type, delegation_id, mcp_manifest_fingerprint,
                session_revision, transcript_revision
            )
            values ($1, $2, $3, $4, $5, $6::text, $7, $8, $9, $10::text,
                    $11::text, $12::text, $13::text, 1, 1)
            "#,
        )
        .bind(child_session_id)
        .bind(config.project_id)
        .bind(&config.runtime_id)
        .bind(&config.workspace_id)
        .bind(serde_json::to_value(&config.workspaces)?)
        .bind(&source_leaf_id)
        .bind(&config.system_prompt)
        .bind(serde_json::to_value(&config.provider)?)
        .bind(&config.metadata)
        .bind(parent_session_id)
        .bind(subagent_type.as_str())
        .bind(delegation_id)
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

        let input_id = format!("input_{}", uuid::Uuid::new_v4());
        let content = QueuedInputContent::user_message(task.clone());
        let route = serde_json::to_value(&config.provider)?;
        sqlx::query(
            r#"
            insert into queued_inputs (
                id, session_id, priority, content, status, follow_up_position, provider_config
            )
            values ($1, $2, $3, $4, 'queued', 0, $5)
            "#,
        )
        .bind(&input_id)
        .bind(child_session_id)
        .bind(InputPriority::FollowUp.as_str())
        .bind(serde_json::to_value(&content)?)
        .bind(route)
        .execute(&mut *tx)
        .await?;
        bump_revisions_tx(&mut tx, child_session_id, true, false).await?;
        let queue = queue_state_tx(&mut tx, child_session_id).await?;
        let created = insert_event_tx(
            &mut tx,
            child_session_id,
            EventType::SessionCreated,
            json!({
                "session_id": child_session_id,
                "project_id": config.project_id,
                "parent_session_id": parent_session_id,
                "provider": config.provider,
                "source_session_id": source_session_id,
                "source_leaf_id": source_leaf_id,
                "active_leaf_id": source_leaf_id,
            }),
        )
        .await?;
        let mut queued_payload = queue_event_payload(
            &queue,
            json!({
                "input_id": input_id,
                "priority": InputPriority::FollowUp,
                "client_input_id": null,
            }),
        );
        append_queued_content_event_fields(&mut queued_payload, &content);
        let queued = insert_event_tx(
            &mut tx,
            child_session_id,
            EventType::InputQueued,
            queued_payload,
        )
        .await?;
        let state = session_state_for_event_tx(&mut tx, child_session_id).await?;
        tx.commit().await?;
        Ok(ForkSessionResult {
            session_id: child_session_id.to_string(),
            source_session_id: source_session_id.to_string(),
            source_leaf_id: source_leaf_id.clone(),
            active_leaf_id: source_leaf_id,
            session_revision: state.session_revision,
            queue_revision: state.queue_revision,
            transcript_revision: state.transcript_revision,
            last_event_id: queued.event_id,
            events: vec![created, queued],
        })
    }

    pub async fn create_fork(&self, request: CreateForkRequest<'_>) -> Result<ForkSessionResult> {
        let CreateForkRequest {
            source_session_id,
            child_session_id,
            config,
            target,
        } = request;
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, source_session_id).await?;
        validate_history_target_tx(&mut tx, source_session_id, target).await?;
        let source_fingerprint: Option<String> =
            sqlx::query_scalar("select mcp_manifest_fingerprint from sessions where id=$1")
                .bind(source_session_id)
                .fetch_one(&mut *tx)
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
        .bind(target.leaf_id)
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
                "active_leaf_id": target.leaf_id,
                "source_session_id": source_session_id,
                "source_leaf_id": target.leaf_id,
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
            source_leaf_id: target.leaf_id.map(str::to_string),
            active_leaf_id: target.leaf_id.map(str::to_string),
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
