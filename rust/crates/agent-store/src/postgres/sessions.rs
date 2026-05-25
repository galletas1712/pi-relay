use agent_session::{SessionAction, SessionEvent, TranscriptStorageNode};
use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use sqlx::Row;

use crate::{
    AcceptedInput, EventFrame, EventType, InputPriority, OutputBatch, PersistedAction,
    SessionActivity, SessionConfig, SessionSummary, SessionWorkspace,
};
use agent_vocab::{ProviderConfig, UserMessage};

use super::events::insert_event_tx;
use super::outputs::persist_outputs_tx;
use super::sql::{action_is_unfinished, queued_input_is_active};
use super::PostgresAgentStore;

fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value.as_object_mut().expect("value was forced to object")
}

fn ensure_compaction_auto_state_object(metadata: &mut Value) -> &mut Map<String, Value> {
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

impl PostgresAgentStore {
    pub async fn create_session(
        &self,
        session_id: &str,
        config: &SessionConfig,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            insert into sessions (id, project_id, outer_cwd, workspaces, system_prompt, provider_config, metadata)
            values ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(session_id)
        .bind(config.project_id)
        .bind(&config.outer_cwd)
        .bind(serde_json::to_value(&config.workspaces)?)
        .bind(&config.system_prompt)
        .bind(serde_json::to_value(&config.provider)?)
        .bind(&config.metadata)
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

    pub async fn update_session_metadata(
        &self,
        session_id: &str,
        metadata: &Value,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query("update sessions set metadata=$2, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(metadata)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() == 0 {
            return Err(anyhow!("session not found: {session_id}"));
        }
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

    pub async fn record_auto_compaction_failure(
        &self,
        session_id: &str,
        config: &SessionConfig,
        source_leaf_id: &str,
        error: &str,
    ) -> Result<()> {
        let current = self.load_session_config(session_id).await?;
        let max_failures = current
            .metadata
            .pointer("/compaction/config/max_consecutive_failures")
            .and_then(Value::as_u64)
            .or_else(|| {
                config
                    .metadata
                    .pointer("/compaction/config/max_consecutive_failures")
                    .and_then(Value::as_u64)
            })
            .map(|value| value as usize)
            .unwrap_or(3)
            .max(1);
        let mut metadata = current.metadata;
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
        sqlx::query("update sessions set metadata=$2, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(metadata)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn reset_auto_compaction_failures(&self, session_id: &str) -> Result<()> {
        let current = self.load_session_config(session_id).await?;
        let mut metadata = current.metadata;
        let state = ensure_compaction_auto_state_object(&mut metadata);
        state.insert("consecutive_failures".to_string(), json!(0));
        state.insert("suppressed".to_string(), json!(false));
        state.insert("last_failure".to_string(), Value::Null);
        state.insert("last_failure_leaf_id".to_string(), Value::Null);
        sqlx::query("update sessions set metadata=$2, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(metadata)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn record_compaction_success(
        &self,
        session_id: &str,
        new_root_id: Option<&str>,
        _manual: bool,
    ) -> Result<()> {
        let current = self.load_session_config(session_id).await?;
        let mut metadata = current.metadata;
        let state = ensure_compaction_auto_state_object(&mut metadata);
        state.insert("consecutive_failures".to_string(), json!(0));
        state.insert("suppressed".to_string(), json!(false));
        state.insert("last_failure".to_string(), Value::Null);
        state.insert("last_failure_leaf_id".to_string(), Value::Null);
        state.insert("last_success_root_id".to_string(), json!(new_root_id));
        sqlx::query("update sessions set metadata=$2, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(metadata)
            .execute(&self.pool)
            .await?;
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
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            r#"
                insert into sessions (id, project_id, outer_cwd, workspaces, active_leaf_id, system_prompt, provider_config, metadata)
                values ($1, $2, $3, $4, $5::text, $6, $7, $8)
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
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
                update sessions
                set metadata = jsonb_set(metadata, '{title}', to_jsonb($2::text), true),
                    updated_at = now()
                where id = $1
                returning provider_config, metadata
            "#,
        )
        .bind(session_id)
        .bind(title)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(anyhow!("session not found: {session_id}"));
        };
        let provider: ProviderConfig = serde_json::from_value(row.get("provider_config"))?;
        let metadata: Value = row.get("metadata");
        let activity = self.activity(session_id).await?;
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
        let result = sqlx::query("delete from sessions where id=$1")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn configure_session(
        &self,
        session_id: &str,
        config: &SessionConfig,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
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
        let activity = self.activity(session_id).await?;
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
                    s.outer_cwd,
                    s.workspaces,
                    s.active_leaf_id,
                    s.provider_config,
                    s.metadata,
                    s.created_at::text as created_at,
                    s.updated_at::text as updated_at,
                    exists(select 1 from actions a where a.session_id=s.id and {running_actions}) as has_running_work,
                    exists(select 1 from queued_inputs q where q.session_id=s.id and {active_queue}) as has_queued_input,
                    exists(select 1 from transcript_entries t where t.session_id=s.id) as has_transcript_entries
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
                let activity = if row.get::<bool, _>("has_running_work") {
                    SessionActivity::Running
                } else if row.get::<bool, _>("has_queued_input") {
                    SessionActivity::Queued
                } else {
                    SessionActivity::Idle
                };
                Ok(SessionSummary {
                    session_id: id,
                    project_id: row.get("project_id"),
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
                    has_transcript_entries: row.get("has_transcript_entries"),
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
                s.metadata
            from sessions s
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
        })
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
