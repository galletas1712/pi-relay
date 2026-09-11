use anyhow::{anyhow, Result};
use serde_json::json;
use sqlx::{Postgres, Transaction};

use crate::{
    CreateContextForkRequest, CreateForkRequest, EventType, ForkSessionResult, InputPriority,
    QueuedInputContent,
};

use super::events::insert_event_tx;
use super::history_target::{branch_entry_ids_tx, ensure_leaf_is_turn_boundary_tx};
use super::mcp::install_session_manifest_tx;
use super::queue::{
    append_queued_content_event_fields, bump_revisions_tx, queue_event_payload, queue_state_tx,
};
use super::sql::{ensure_no_active_work_tx, ensure_no_running_delegation_tx, lock_session_tx};
use super::transcript::session_state_for_event_tx;
use super::PostgresAgentStore;

/// A completed conversational boundary and its active-branch prefix.
#[derive(Debug)]
struct CompletedContext {
    leaf_id: String,
    entry_ids: Vec<String>,
}

async fn latest_completed_context_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    active_leaf_id: Option<&str>,
) -> Result<Option<CompletedContext>> {
    let mut entry_ids = branch_entry_ids_tx(tx, session_id, active_leaf_id).await?;
    if entry_ids.is_empty() {
        return Ok(None);
    }
    // Boundary summaries omit turn_started_at_ms; mid-turn summaries persist
    // it so the compacted open turn can be restored.
    let leaf_id: Option<String> = sqlx::query_scalar(
        r#"
        select id
        from transcript_entries
        where session_id=$1 and id=any($2)
          and (
              item->>'type' = 'turn_finished'
              or (
                  item->>'type' = 'compaction_summary'
                  and not (item ? 'turn_started_at_ms')
              )
          )
        order by sequence desc
        limit 1
        "#,
    )
    .bind(session_id)
    .bind(&entry_ids)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    let Some(leaf_id) = leaf_id else {
        return Ok(None);
    };
    let boundary = entry_ids
        .iter()
        .position(|entry_id| entry_id == &leaf_id)
        .ok_or_else(|| anyhow!("completed context boundary is not on the active branch"))?;
    entry_ids.truncate(boundary + 1);
    Ok(Some(CompletedContext { leaf_id, entry_ids }))
}

async fn copy_history_tx(
    tx: &mut Transaction<'_, Postgres>,
    source_session_id: &str,
    child_session_id: &str,
    entry_ids: Option<&[String]>,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into transcript_entries (
            session_id, id, parent_id, timestamp_ms, item, provider_replay, turn_id
        )
        select $2::text, id, parent_id, timestamp_ms, item, provider_replay, turn_id
        from transcript_entries
        where session_id=$1 and ($3::text[] is null or id=any($3))
        order by sequence
        "#,
    )
    .bind(source_session_id)
    .bind(child_session_id)
    .bind(entry_ids)
    .execute(&mut **tx)
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
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl PostgresAgentStore {
    pub async fn create_context_fork(
        &self,
        request: CreateContextForkRequest<'_>,
    ) -> Result<ForkSessionResult> {
        let CreateContextForkRequest {
            child_session_id,
            config,
            parent_session_id,
            subagent_type,
            delegation_id,
            task,
        } = request;
        if child_session_id == parent_session_id {
            return Err(anyhow!(
                "child session id must differ from parent session id"
            ));
        }
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, parent_session_id).await?;
        let (active_leaf_id, parent_fingerprint): (Option<String>, Option<String>) =
            sqlx::query_as(
                "select active_leaf_id, mcp_manifest_fingerprint from sessions where id=$1",
            )
            .bind(parent_session_id)
            .fetch_one(&mut *tx)
            .await?;
        if parent_fingerprint.as_deref()
            != config
                .mcp_manifest
                .as_ref()
                .map(|binding| binding.manifest_fingerprint.as_str())
        {
            return Err(crate::SessionConfigChanged.into());
        }
        let completed =
            latest_completed_context_tx(&mut tx, parent_session_id, active_leaf_id.as_deref())
                .await?;
        let inherited_leaf_id = completed.as_ref().map(|context| context.leaf_id.as_str());
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
            values (
                $1, $2, $3, $4, $5, $6::text, $7, $8, $9, $10::text,
                $11::text, $12::text, $13::text, 1, 1
            )
            "#,
        )
        .bind(child_session_id)
        .bind(config.project_id)
        .bind(&config.runtime_id)
        .bind(&config.workspace_id)
        .bind(serde_json::to_value(&config.workspaces)?)
        .bind(inherited_leaf_id)
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
        let inherited_entry_ids = completed
            .as_ref()
            .map(|context| context.entry_ids.as_slice())
            .unwrap_or_default();
        copy_history_tx(
            &mut tx,
            parent_session_id,
            child_session_id,
            Some(inherited_entry_ids),
        )
        .await?;

        let input_id = format!("input_{}", uuid::Uuid::new_v4());
        let content = QueuedInputContent::user_message(task.clone());
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
        .bind(serde_json::to_value(&config.provider)?)
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
                "source_session_id": parent_session_id,
                "active_leaf_id": inherited_leaf_id,
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
            source_session_id: parent_session_id.to_string(),
            active_leaf_id: inherited_leaf_id.map(str::to_string),
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
        copy_history_tx(&mut tx, source_session_id, child_session_id, None).await?;
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
