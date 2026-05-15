use agent_session::{SessionAction, SessionEvent, TranscriptStorageNode};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    AcceptedInput, EventFrame, EventType, GlobalConfig, InputPriority, OutputBatch,
    PersistedAction, SessionActivity, SessionConfig, SessionSummary,
};
use agent_vocab::{ProviderConfig, UserMessage};

use super::events::insert_event_tx;
use super::outputs::persist_outputs_tx;
use super::sql::{action_is_unfinished, queued_input_is_active};
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn create_session(
        &self,
        session_id: &str,
        config: &SessionConfig,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("insert into sessions (id, provider_config, metadata) values ($1, $2, $3)")
            .bind(session_id)
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
                "provider": config.provider,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
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
                insert into sessions (id, active_leaf_id, provider_config, metadata)
                values ($1, $2::text, $3, $4)
                on conflict (id) do nothing
                returning id
                "#,
        )
        .bind(session_id)
        .bind(active_leaf_id)
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
        let result = sqlx::query(
            r#"
                update sessions
                set metadata = jsonb_set(metadata, '{title}', to_jsonb($2::text), true),
                    updated_at = now()
                where id = $1
            "#,
        )
        .bind(session_id)
        .bind(title)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(anyhow!("session not found: {session_id}"));
        }
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::SessionConfigured,
            json!({
                "title": title,
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
        let result = sqlx::query(
            "update sessions set provider_config=$2, metadata=$3, updated_at=now() where id=$1",
        )
        .bind(session_id)
        .bind(serde_json::to_value(&config.provider)?)
        .bind(&config.metadata)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(anyhow!("session not found: {session_id}"));
        }
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::SessionConfigured,
            json!({
                "provider": config.provider,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn list_sessions(&self, limit: i64) -> Result<Vec<SessionSummary>> {
        let running_actions = action_is_unfinished(Some("a"));
        let active_queue = queued_input_is_active(Some("q"));
        let query = format!(
            r#"
                select
                    s.id,
                    s.active_leaf_id,
                    s.provider_config,
                    s.metadata,
                    s.created_at::text as created_at,
                    s.updated_at::text as updated_at,
                    exists(select 1 from actions a where a.session_id=s.id and {running_actions}) as has_running_work,
                    exists(select 1 from queued_inputs q where q.session_id=s.id and {active_queue}) as has_queued_input
                from sessions s
                where s.metadata->>'hidden' is distinct from 'true'
                    and not (
                        s.metadata->>'created_by' = 'web'
                        and not exists(select 1 from transcript_entries t where t.session_id=s.id)
                        and not exists(select 1 from queued_inputs q where q.session_id=s.id and q.status <> 'cancelled')
                        and not exists(select 1 from actions a where a.session_id=s.id)
                        and not (s.metadata ? 'fork')
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
                    activity,
                    active_leaf_id: row.get("active_leaf_id"),
                    provider,
                    metadata: row.get("metadata"),
                    created_at: row.get("created_at"),
                    updated_at: row.get("updated_at"),
                })
            })
            .collect()
    }

    pub async fn load_session_config(&self, session_id: &str) -> Result<SessionConfig> {
        let row = sqlx::query("select provider_config, metadata from sessions where id=$1")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        Ok(SessionConfig {
            provider: serde_json::from_value(row.get("provider_config"))?,
            metadata: row.get("metadata"),
        })
    }

    pub async fn global_config(&self) -> Result<GlobalConfig> {
        Ok(GlobalConfig {
            system_prompt: self.global_system_prompt().await?,
        })
    }

    pub async fn global_system_prompt(&self) -> Result<Option<String>> {
        let row = sqlx::query("select value from daemon_config where key='system_prompt'")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|row| row.get::<Value, _>("value").as_str().map(str::to_string)))
    }

    pub async fn set_global_system_prompt(&self, system_prompt: Option<&str>) -> Result<()> {
        let value = system_prompt.map_or(Value::Null, |value| json!(value));
        sqlx::query(
            r#"
                insert into daemon_config (key, value, updated_at)
                values ('system_prompt', $1, now())
                on conflict (key) do update
                set value=excluded.value, updated_at=now()
                "#,
        )
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
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
