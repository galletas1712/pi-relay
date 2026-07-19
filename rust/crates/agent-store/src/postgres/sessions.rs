use agent_session::{SessionAction, SessionEvent, TranscriptStorageNode};
use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use sqlx::Row;

use crate::{
    AcceptedInput, EventFrame, EventType, InputPriority, OutputBatch, PersistedAction,
    SessionActivity, SessionConfig, SessionGitConfig, SessionSummary, SessionWorkspace,
    SubagentType,
};
use agent_vocab::{ProviderConfig, UserMessage};

use super::events::insert_event_tx;
use super::mcp::install_session_manifest_tx;
use super::outputs::persist_outputs_tx;
use super::queue::bump_revisions_tx;
use super::sql::{
    action_is_unfinished, ensure_no_active_work_tx, freeze_legacy_routes_tx, lock_session_tx,
    queued_input_is_active, session_activity,
};
use super::PostgresAgentStore;

fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value.as_object_mut().expect("value was forced to object")
}

pub(super) fn next_auto_compaction_failure_metadata(
    mut metadata: Value,
    fallback_max_failures: usize,
    source_leaf_id: &str,
    error: &str,
) -> Value {
    let max_failures = metadata
        .pointer("/compaction/config/max_consecutive_failures")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(fallback_max_failures)
        .max(1);
    let state = ensure_compaction_auto_state_object(&mut metadata);
    let failures = state
        .get("consecutive_failures")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(1);
    state.insert("consecutive_failures".to_string(), json!(failures));
    state.insert("last_failure".to_string(), json!(error));
    state.insert("last_failure_leaf_id".to_string(), json!(source_leaf_id));
    state.insert(
        "suppressed".to_string(),
        json!(failures as usize >= max_failures),
    );
    metadata
}

pub(super) fn next_compaction_success_metadata(
    mut metadata: Value,
    source_leaf_id: &str,
    new_leaf_id: &str,
    manual: bool,
) -> Value {
    let state = ensure_compaction_auto_state_object(&mut metadata);
    let previous_leaf = state.get("last_success_leaf_id").and_then(Value::as_str);
    let previous_recompactions = state
        .get("consecutive_recompactions")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let consecutive_recompactions = if !manual && previous_leaf == Some(source_leaf_id) {
        previous_recompactions.saturating_add(1)
    } else {
        0
    };
    state.insert("consecutive_failures".to_string(), json!(0));
    state.insert("suppressed".to_string(), json!(false));
    state.insert("last_failure".to_string(), Value::Null);
    state.insert("last_failure_leaf_id".to_string(), Value::Null);
    state.insert(
        "consecutive_recompactions".to_string(),
        json!(consecutive_recompactions),
    );
    state.insert("last_success_leaf_id".to_string(), json!(new_leaf_id));
    metadata
}

pub(super) fn ensure_compaction_auto_state_object(metadata: &mut Value) -> &mut Map<String, Value> {
    let root = ensure_object(metadata);
    let compaction = root
        .entry("compaction".to_string())
        .or_insert_with(|| json!({}));
    let compaction = ensure_object(compaction);
    let auto_state = compaction
        .entry("auto_state".to_string())
        .or_insert_with(|| json!({}));
    ensure_object(auto_state)
}

enum TitleUpdateKind {
    Automatic,
    Manual,
}

impl PostgresAgentStore {
    pub async fn create_session(
        &self,
        session_id: &str,
        config: &SessionConfig,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        if let Some(binding) = &config.mcp_manifest {
            install_session_manifest_tx(&mut tx, binding).await?;
        }
        sqlx::query(
            r#"
            insert into sessions (
                id, project_id, outer_cwd, workspaces, system_prompt,
                provider_config, metadata, mcp_manifest_fingerprint
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8::text)
            "#,
        )
        .bind(session_id)
        .bind(config.project_id)
        .bind(&config.outer_cwd)
        .bind(serde_json::to_value(&config.workspaces)?)
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
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::SessionCreated,
            json!({
                "session_id": session_id,
                "project_id": config.project_id,
                "provider": config.provider,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn load_session_git_config(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionGitConfig>> {
        let row = sqlx::query("select outer_cwd, workspaces from sessions where id=$1")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            Ok(SessionGitConfig {
                outer_cwd: row.get("outer_cwd"),
                workspaces: serde_json::from_value::<Vec<SessionWorkspace>>(
                    row.get::<Value, _>("workspaces"),
                )?,
            })
        })
        .transpose()
    }

    pub async fn update_session_metadata(
        &self,
        session_id: &str,
        metadata: &Value,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let result = sqlx::query("update sessions set metadata=$2, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(metadata)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() == 0 {
            return Err(anyhow!("session not found: {session_id}"));
        }
        bump_revisions_tx(&mut tx, session_id, false, false).await?;
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::SessionConfigured,
            json!({ "metadata": metadata }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn reset_auto_compaction_failures(&self, session_id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let mut metadata = session_metadata_tx(&mut tx, session_id).await?;
        let state = ensure_compaction_auto_state_object(&mut metadata);
        state.insert("consecutive_failures".to_string(), json!(0));
        state.insert("suppressed".to_string(), json!(false));
        state.insert("last_failure".to_string(), Value::Null);
        state.insert("last_failure_leaf_id".to_string(), Value::Null);
        state.insert("consecutive_recompactions".to_string(), json!(0));
        sqlx::query("update sessions set metadata=$2, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(metadata)
            .execute(&mut *tx)
            .await?;
        bump_revisions_tx(&mut tx, session_id, false, false).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn session_exists(&self, session_id: &str) -> Result<bool> {
        Ok(
            sqlx::query_scalar("select exists(select 1 from sessions where id=$1)")
                .bind(session_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    // Persistence entry point: each argument is a distinct session/transcript column.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_session_outputs(
        &self,
        session_id: &str,
        config: &SessionConfig,
        entries: &[TranscriptStorageNode],
        active_leaf_id: Option<&str>,
        session_events: &[SessionEvent],
        actions: &[SessionAction],
        priority: InputPriority,
        content: &UserMessage,
        client_input_id: Option<&str>,
    ) -> Result<(Vec<EventFrame>, Vec<PersistedAction>)> {
        self.start_session_outputs_with_parent(
            session_id,
            config,
            entries,
            active_leaf_id,
            session_events,
            actions,
            priority,
            content,
            client_input_id,
            None,
            None,
            None,
        )
        .await
    }

    // Persistence entry point: each argument is a distinct session/transcript column.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_session_outputs_with_parent(
        &self,
        session_id: &str,
        config: &SessionConfig,
        entries: &[TranscriptStorageNode],
        active_leaf_id: Option<&str>,
        session_events: &[SessionEvent],
        actions: &[SessionAction],
        priority: InputPriority,
        content: &UserMessage,
        client_input_id: Option<&str>,
        parent_session_id: Option<&str>,
        subagent_type: Option<SubagentType>,
        delegation_id: Option<&str>,
    ) -> Result<(Vec<EventFrame>, Vec<PersistedAction>)> {
        if parent_session_id == Some(session_id) {
            return Err(anyhow!(
                "child session id must differ from parent session id"
            ));
        }
        let mut tx = self.pool.begin().await?;
        if let Some(parent_session_id) = parent_session_id {
            let parent_fingerprint: Option<Option<String>> = sqlx::query_scalar(
                "select mcp_manifest_fingerprint from sessions where id=$1::text for key share",
            )
            .bind(parent_session_id)
            .fetch_optional(&mut *tx)
            .await?;
            let parent_fingerprint = parent_fingerprint
                .ok_or_else(|| anyhow!("parent session not found: {parent_session_id}"))?;
            let child_fingerprint = config
                .mcp_manifest
                .as_ref()
                .map(|binding| binding.manifest_fingerprint.as_str());
            if parent_fingerprint.as_deref() != child_fingerprint {
                return Err(anyhow!(
                    "child MCP manifest must exactly match parent session {parent_session_id}"
                ));
            }
        }
        if let Some(binding) = &config.mcp_manifest {
            install_session_manifest_tx(&mut tx, binding).await?;
        }
        let inserted = sqlx::query(
            r#"
                insert into sessions (
                    id, project_id, outer_cwd, workspaces, active_leaf_id,
                    system_prompt, provider_config, metadata, parent_session_id,
                    subagent_type, delegation_id, mcp_manifest_fingerprint
                )
                values (
                    $1, $2, $3, $4, $5::text, $6, $7, $8, $9::text,
                    $10::text, $11::text, $12::text
                )
                on conflict (id) do nothing
                returning id
                "#,
        )
        .bind(session_id)
        .bind(config.project_id)
        .bind(&config.outer_cwd)
        .bind(serde_json::to_value(&config.workspaces)?)
        .bind(active_leaf_id)
        .bind(&config.system_prompt)
        .bind(serde_json::to_value(&config.provider)?)
        .bind(&config.metadata)
        .bind(parent_session_id)
        .bind(subagent_type.map(|subagent_type| subagent_type.as_str()))
        .bind(delegation_id)
        .bind(
            config
                .mcp_manifest
                .as_ref()
                .map(|binding| &binding.manifest_fingerprint),
        )
        .fetch_optional(&mut *tx)
        .await?;
        if inserted.is_none() {
            tx.commit().await?;
            return Ok((Vec::new(), Vec::new()));
        }

        let mut frames = vec![
            insert_event_tx(
                &mut tx,
                session_id,
                EventType::SessionCreated,
                json!({
                    "session_id": session_id,
                    "project_id": config.project_id,
                    "parent_session_id": parent_session_id,
                    "provider": config.provider,
                }),
            )
            .await?,
        ];
        let batch = OutputBatch::new(entries, active_leaf_id, session_events, actions)
            .with_accepted_input(Some(AcceptedInput {
                priority,
                content: content.clone(),
                client_input_id: client_input_id.map(str::to_string),
            }));
        let (mut output_frames, dispatch) = persist_outputs_tx(&mut tx, session_id, batch).await?;
        frames.append(&mut output_frames);
        tx.commit().await?;
        Ok((frames, dispatch))
    }

    pub async fn rename_session(&self, session_id: &str, title: &str) -> Result<Vec<EventFrame>> {
        self.update_session_title(session_id, title, TitleUpdateKind::Automatic)
            .await
    }

    pub async fn rename_session_manually(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<Vec<EventFrame>> {
        self.update_session_title(session_id, title, TitleUpdateKind::Manual)
            .await
    }

    async fn update_session_title(
        &self,
        session_id: &str,
        title: &str,
        update_kind: TitleUpdateKind,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        let query = match update_kind {
            TitleUpdateKind::Automatic => {
                r#"
                update sessions
                set metadata = jsonb_set(metadata, '{title}', to_jsonb($2::text), true),
                    updated_at = now()
                where id = $1
                returning provider_config, metadata
            "#
            }
            TitleUpdateKind::Manual => {
                r#"
                update sessions
                set metadata = jsonb_set(
                        jsonb_set(metadata, '{title}', to_jsonb($2::text), true),
                        '{auto_title_disabled}',
                        'true'::jsonb,
                        true
                    ),
                    updated_at = now()
                where id = $1
                returning provider_config, metadata
            "#
            }
        };
        let row = sqlx::query(query)
            .bind(session_id)
            .bind(title)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            return Err(anyhow!("session not found: {session_id}"));
        };
        let provider: ProviderConfig = serde_json::from_value(row.get("provider_config"))?;
        let metadata: Value = row.get("metadata");
        let activity = activity_tx(&mut tx, session_id).await?;
        bump_revisions_tx(&mut tx, session_id, false, false).await?;
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::SessionConfigured,
            json!({
                "title": title,
                "metadata": metadata,
                "provider": provider,
                "activity": activity,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        if let Err(error) = lock_session_tx(&mut tx, session_id).await {
            if error.to_string().starts_with("session not found:") {
                tx.commit().await?;
                return Ok(false);
            }
            return Err(error);
        }
        ensure_no_active_work_tx(&mut tx, session_id).await?;
        let result = sqlx::query("delete from sessions where id=$1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn configure_session(
        &self,
        session_id: &str,
        config: &SessionConfig,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, session_id).await?;
        freeze_legacy_routes_tx(&mut tx, session_id).await?;
        let row = sqlx::query(
            "update sessions set provider_config=$2, metadata=$3, updated_at=now() where id=$1 returning metadata",
        )
        .bind(session_id)
        .bind(serde_json::to_value(&config.provider)?)
        .bind(&config.metadata)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(anyhow!("session not found: {session_id}"));
        };
        let metadata: Value = row.get("metadata");
        let activity = activity_tx(&mut tx, session_id).await?;
        bump_revisions_tx(&mut tx, session_id, false, false).await?;
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::SessionConfigured,
            json!({
                "provider": config.provider,
                "metadata": metadata,
                "activity": activity,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn list_sessions(
        &self,
        project_id: Option<uuid::Uuid>,
        limit: i64,
    ) -> Result<Vec<SessionSummary>> {
        let running_actions = action_is_unfinished(Some("a"));
        let active_queue = queued_input_is_active(Some("q"));
        let query = format!(
            r#"
                select
                    s.id,
                    s.project_id,
                    s.parent_session_id,
                    s.outer_cwd,
                    s.workspaces,
                    s.active_leaf_id,
                    s.provider_config,
                    s.metadata,
                    s.created_at::text as created_at,
                    s.updated_at::text as updated_at,
                    s.last_user_message_timestamp_ms,
                    exists(select 1 from actions a where a.session_id=s.id and {running_actions}) as has_running_work,
                    exists(select 1 from queued_inputs q where q.session_id=s.id and {active_queue}) as has_queued_input,
                    exists(select 1 from transcript_entries t where t.session_id=s.id) as has_transcript_entries,
                    exists(select 1 from delegations d where d.parent_session_id = s.id and d.status = 'running') as has_running_delegations
                from sessions s
                where s.metadata->>'hidden' is distinct from 'true'
                    and (
                        ($2::uuid is null and s.project_id is null)
                        or ($2::uuid is not null and s.project_id=$2)
                    )
                    and not (
                        s.metadata->>'created_by' = 'web'
                        and not exists(select 1 from transcript_entries t where t.session_id=s.id)
                        and not exists(select 1 from queued_inputs q where q.session_id=s.id and q.status <> 'cancelled')
                        and not exists(select 1 from actions a where a.session_id=s.id)
                    )
                order by
                    case when s.metadata->>'archived' = 'true' then 1 else 0 end asc,
                    last_user_message_timestamp_ms desc nulls last,
                    s.created_at desc,
                    s.id desc
                limit $1
                "#
        );
        let rows = sqlx::query(&query)
            .bind(limit)
            .bind(project_id)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let id: String = row.get("id");
                let provider: ProviderConfig =
                    serde_json::from_value(row.get::<Value, _>("provider_config"))?;
                let activity = session_activity(
                    row.get::<bool, _>("has_running_work"),
                    row.get::<bool, _>("has_queued_input"),
                );
                Ok(SessionSummary {
                    session_id: id,
                    project_id: row.get("project_id"),
                    parent_session_id: row.get("parent_session_id"),
                    outer_cwd: row.get("outer_cwd"),
                    workspaces: serde_json::from_value::<Vec<SessionWorkspace>>(
                        row.get::<Value, _>("workspaces"),
                    )?,
                    activity,
                    active_leaf_id: row.get("active_leaf_id"),
                    provider,
                    metadata: row.get("metadata"),
                    created_at: row.get("created_at"),
                    updated_at: row.get("updated_at"),
                    last_user_message_timestamp_ms: row.get("last_user_message_timestamp_ms"),
                    has_transcript_entries: row.get("has_transcript_entries"),
                    has_running_delegations: row.get("has_running_delegations"),
                })
            })
            .collect()
    }

    pub async fn load_session_config(&self, session_id: &str) -> Result<SessionConfig> {
        let row = sqlx::query(
            r#"
            select
                s.project_id,
                s.outer_cwd,
                s.workspaces,
                s.system_prompt,
                s.provider_config,
                s.metadata,
                s.mcp_manifest_fingerprint,
                m.manifest as mcp_manifest
            from sessions s
            left join mcp_session_manifests m
                on m.fingerprint=s.mcp_manifest_fingerprint
            where s.id=$1
            "#,
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        Ok(SessionConfig {
            project_id: row.get("project_id"),
            outer_cwd: row.get("outer_cwd"),
            workspaces: serde_json::from_value::<Vec<SessionWorkspace>>(
                row.get::<Value, _>("workspaces"),
            )?,
            system_prompt: row.get("system_prompt"),
            provider: serde_json::from_value(row.get("provider_config"))?,
            metadata: row.get("metadata"),
            mcp_manifest: row
                .get::<Option<String>, _>("mcp_manifest_fingerprint")
                .map(|manifest_fingerprint| crate::McpSessionManifestBinding {
                    manifest_fingerprint,
                    manifest: row
                        .get::<Option<Value>, _>("mcp_manifest")
                        .expect("MCP manifest foreign key must resolve"),
                }),
        })
    }

    pub async fn session_subagent_type(&self, session_id: &str) -> Result<Option<SubagentType>> {
        let raw: Option<String> =
            sqlx::query_scalar("select subagent_type from sessions where id=$1")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        raw.map(|raw| raw.parse::<SubagentType>().map_err(|error| anyhow!(error)))
            .transpose()
    }

    pub async fn session_delegation_id(&self, session_id: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("select delegation_id from sessions where id=$1")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten(),
        )
    }

    pub async fn activity(&self, session_id: &str) -> Result<SessionActivity> {
        if self.has_unfinished_actions(session_id).await? {
            return Ok(SessionActivity::Running);
        }
        let active_queue = queued_input_is_active(None);
        let query = format!(
            "select exists(select 1 from queued_inputs where session_id=$1 and {active_queue})"
        );
        let queued: bool = sqlx::query_scalar(&query)
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?;
        if queued {
            Ok(SessionActivity::Queued)
        } else {
            Ok(SessionActivity::Idle)
        }
    }
}

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;

pub(super) async fn session_metadata_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> Result<Value> {
    sqlx::query_scalar("select metadata from sessions where id=$1")
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| anyhow!("session not found: {session_id}"))
}

async fn activity_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> Result<SessionActivity> {
    let unfinished_actions = action_is_unfinished(None);
    let actions_query = format!(
        "select exists(select 1 from actions where session_id=$1 and {unfinished_actions})"
    );
    let running: bool = sqlx::query_scalar(&actions_query)
        .bind(session_id)
        .fetch_one(&mut **tx)
        .await?;
    if running {
        return Ok(SessionActivity::Running);
    }
    let active_queue = queued_input_is_active(None);
    let queued_query = format!(
        "select exists(select 1 from queued_inputs where session_id=$1 and {active_queue})"
    );
    let queued: bool = sqlx::query_scalar(&queued_query)
        .bind(session_id)
        .fetch_one(&mut **tx)
        .await?;
    if queued {
        Ok(SessionActivity::Queued)
    } else {
        Ok(SessionActivity::Idle)
    }
}
