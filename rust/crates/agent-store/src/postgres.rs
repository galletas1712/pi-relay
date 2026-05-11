use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use agent_session::{
    SessionAction, SessionActionKind, SessionEvent, StoredSession, StoredTranscriptEntry,
    TranscriptItem, TranscriptStorageNode, UserMessage,
};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use sqlx::{postgres::PgRow, PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::{
    AcceptedInput, ActionKind, ActionStatus, ActionUpdate, DispatchAction, EnqueueUserInputResult,
    EventFrame, EventType, InputPriority, InputRecord, ProviderConfig, QueuedInput,
    QueuedInputStatus, SessionActivity, SessionConfig, StoredAction,
};

pub struct PostgresAgentStore {
    pool: PgPool,
}

impl PostgresAgentStore {
    pub async fn connect(database_url: &str) -> Result<Self> {
        Ok(Self {
            pool: PgPool::connect(database_url).await?,
        })
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::raw_sql(
            r#"
                create table if not exists sessions (
                    id text primary key,
                    created_at timestamptz not null default now(),
                    updated_at timestamptz not null default now(),
                    active_leaf_id text null,
                    provider_config jsonb not null,
                    metadata jsonb not null default '{}'::jsonb
                );
                create table if not exists daemon_config (
                    key text primary key,
                    value jsonb not null,
                    updated_at timestamptz not null default now()
                );
                create table if not exists transcript_entries (
                    session_id text not null references sessions(id) on delete cascade,
                    id text not null,
                    parent_id text null,
                    timestamp_ms bigint not null,
                    item jsonb not null,
                    turn_id bigint null,
                    sequence bigserial not null,
                    primary key (session_id, id)
                );
                create index if not exists transcript_entries_session_sequence_idx
                    on transcript_entries(session_id, sequence);
                create table if not exists queued_inputs (
                    id text primary key,
                    session_id text not null references sessions(id) on delete cascade,
                    priority text not null,
                    content jsonb not null,
                    origin jsonb null,
                    status text not null,
                    created_at timestamptz not null default now(),
                    client_input_id text null
                );
                create unique index if not exists queued_inputs_client_input_idx
                    on queued_inputs(session_id, client_input_id)
                    where client_input_id is not null;
                create table if not exists actions (
                    id text primary key,
                    session_id text not null references sessions(id) on delete cascade,
                    turn_id bigint null,
                    action_id bigint not null,
                    attempt_id text not null,
                    kind text not null,
                    status text not null,
                    payload jsonb not null,
                    result jsonb null,
                    created_at timestamptz not null default now(),
                    updated_at timestamptz not null default now()
                );
                create index if not exists actions_session_status_idx
                    on actions(session_id, status);
                create table if not exists events (
                    id bigserial primary key,
                    session_id text not null references sessions(id) on delete cascade,
                    type text not null,
                    payload jsonb not null,
                    created_at timestamptz not null default now()
                );
                create index if not exists events_session_id_idx on events(session_id, id);
                "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

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
    ) -> Result<(Vec<EventFrame>, Vec<DispatchAction>)> {
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
        let (mut output_frames, dispatch) = persist_outputs_tx(
            &mut tx,
            session_id,
            entries,
            active_leaf_id,
            session_events,
            actions,
            None,
            None,
            Some(AcceptedInput {
                priority,
                content: content.clone(),
                client_input_id: client_input_id.map(str::to_string),
            }),
            config,
        )
        .await?;
        frames.append(&mut output_frames);
        tx.commit().await?;
        Ok((frames, dispatch))
    }

    pub async fn configure_session(
        &self,
        session_id: &str,
        config: &SessionConfig,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "update sessions set provider_config=$2, metadata=$3, updated_at=now() where id=$1",
        )
        .bind(session_id)
        .bind(serde_json::to_value(&config.provider)?)
        .bind(&config.metadata)
        .execute(&mut *tx)
        .await?;
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

    pub async fn list_sessions(&self, limit: i64) -> Result<Vec<Value>> {
        let rows = sqlx::query(
            r#"
                select
                    s.id,
                    s.active_leaf_id,
                    s.provider_config,
                    s.metadata,
                    s.updated_at::text as updated_at,
                    exists(select 1 from actions a where a.session_id=s.id and a.status in ('pending','running')) as has_running_work,
                    exists(select 1 from queued_inputs q where q.session_id=s.id and q.status in ('queued','consuming')) as has_queued_input
                from sessions s
                where s.metadata->>'hidden' is distinct from 'true'
                    and not (
                        s.metadata->>'created_by' = 'web'
                        and not exists(select 1 from transcript_entries t where t.session_id=s.id)
                        and not exists(select 1 from queued_inputs q where q.session_id=s.id and q.status <> 'cancelled')
                        and not exists(select 1 from actions a where a.session_id=s.id)
                        and not exists(
                            select 1
                            from events e
                            where e.session_id=s.id
                                and e.type='session.created'
                                and e.payload ? 'forked_from'
                        )
                    )
                order by s.updated_at desc
                limit $1
                "#,
        )
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
                Ok(json!({
                    "session_id": id,
                    "activity": activity,
                    "active_leaf_id": row.get::<Option<String>, _>("active_leaf_id"),
                    "provider": provider,
                    "metadata": row.get::<Value, _>("metadata"),
                    "updated_at": row.get::<String, _>("updated_at"),
                }))
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

    pub async fn global_config(&self) -> Result<Value> {
        Ok(json!({
            "system_prompt": self.global_system_prompt().await?,
        }))
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

    pub async fn load_stored_session(&self, session_id: &str) -> Result<StoredSession> {
        let session_row = sqlx::query("select active_leaf_id, metadata from sessions where id=$1")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let rows = sqlx::query(
            "select id, parent_id, timestamp_ms, item from transcript_entries where session_id=$1 order by sequence",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        let mut metadata = BTreeMap::new();
        if let Value::Object(map) = session_row.get::<Value, _>("metadata") {
            for (key, value) in map {
                if let Some(value) = value.as_str() {
                    metadata.insert(key, value.to_string());
                }
            }
        }
        Ok(StoredSession {
            session_id: session_id.to_string(),
            active_leaf_id: session_row.get("active_leaf_id"),
            metadata,
            entries: rows
                .into_iter()
                .map(|row| row_to_stored_entry(&row))
                .collect::<Result<Vec<_>>>()?,
        })
    }

    pub async fn enqueue_user_input(
        &self,
        session_id: &str,
        priority: InputPriority,
        content: &UserMessage,
        client_input_id: Option<&str>,
    ) -> Result<EnqueueUserInputResult> {
        let id = format!("input_{}", Uuid::new_v4());
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            r#"
                insert into queued_inputs (id, session_id, priority, content, status, client_input_id, origin)
                values (
                    $1,
                    $2,
                    $3,
                    $4,
                    'queued',
                    $5,
                    case
                        when $3 = 'steer' then jsonb_build_object('promoted_at', now()::text)
                        else null
                    end
                )
                on conflict (session_id, client_input_id) where client_input_id is not null
                do nothing
                returning id
                "#,
            )
            .bind(&id)
            .bind(session_id)
            .bind(priority.as_str())
            .bind(serde_json::to_value(content)?)
            .bind(client_input_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(inserted) = inserted else {
            let row = sqlx::query(
                "select id from queued_inputs where session_id=$1 and client_input_id=$2::text",
            )
            .bind(session_id)
            .bind(client_input_id)
            .fetch_one(&mut *tx)
            .await?;
            let input_id = row.get("id");
            tx.commit().await?;
            return Ok(EnqueueUserInputResult {
                input_id,
                event: None,
            });
        };

        let input_id = inserted.get("id");
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::InputQueued,
            json!({
                "input_id": input_id,
                "priority": priority,
                "client_input_id": client_input_id,
                "content": content,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(EnqueueUserInputResult {
            input_id,
            event: Some(event),
        })
    }

    pub async fn find_client_input(
        &self,
        session_id: &str,
        client_input_id: &str,
    ) -> Result<Option<InputRecord>> {
        let row = sqlx::query(
            r#"
                select id, status
                from queued_inputs
                where session_id=$1 and client_input_id=$2::text
                "#,
        )
        .bind(session_id)
        .bind(client_input_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(InputRecord {
                input_id: row.get("id"),
                status: row_text::<QueuedInputStatus>(&row, "status")?,
            })
        })
        .transpose()
    }

    pub async fn take_next_queued_input(&self, session_id: &str) -> Result<Option<QueuedInput>> {
        let claim_id = Uuid::new_v4().to_string();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
                update queued_inputs
                set status='consuming',
                    origin=coalesce(origin, '{}'::jsonb)
                        || jsonb_build_object('claim_id', $2::text, 'claimed_at', now()::text)
                where id = (
                    select id
                    from queued_inputs
                    where session_id=$1 and status='queued'
                    order by
                        case priority when 'steer' then 0 else 1 end,
                        case
                            when priority='steer'
                            then coalesce((origin->>'promoted_at')::timestamptz, created_at)
                            else created_at
                        end,
                        created_at
                    limit 1
                    for update skip locked
                )
                returning id, priority, content, client_input_id
                "#,
        )
        .bind(session_id)
        .bind(&claim_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let id: String = row.get("id");
        let content: UserMessage = serde_json::from_value(row.get::<Value, _>("content"))?;
        let priority = row_text::<InputPriority>(&row, "priority")?;
        tx.commit().await?;
        Ok(Some(QueuedInput {
            id,
            priority,
            content,
            client_input_id: row.get("client_input_id"),
            claim_id,
        }))
    }

    pub async fn promote_queued_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<EventFrame> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
                update queued_inputs
                set priority='steer',
                    origin=coalesce(origin, '{}'::jsonb)
                        || jsonb_build_object('promoted_at', now()::text)
                where session_id=$1
                    and id=$2::text
                    and status='queued'
                    and priority='follow_up'
                returning client_input_id, content, origin->>'promoted_at' as promoted_at
                "#,
        )
        .bind(session_id)
        .bind(input_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(anyhow!(
                "queued input is no longer editable or was not found: {input_id}"
            ));
        };
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::InputPromoted,
            json!({
                "input_id": input_id,
                "priority": InputPriority::Steer,
                "client_input_id": row.get::<Option<String>, _>("client_input_id"),
                "content": row.get::<Value, _>("content"),
                "promoted_at": row.get::<Option<String>, _>("promoted_at"),
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(event)
    }

    pub async fn replace_queued_input(
        &self,
        session_id: &str,
        input_id: &str,
        content: &UserMessage,
    ) -> Result<EventFrame> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
                update queued_inputs
                set content=$3
                where session_id=$1 and id=$2::text and status='queued'
                returning priority, client_input_id
                "#,
        )
        .bind(session_id)
        .bind(input_id)
        .bind(serde_json::to_value(content)?)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(anyhow!(
                "queued input is no longer editable or was not found: {input_id}"
            ));
        };
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::InputReplaced,
            json!({
                "input_id": input_id,
                "priority": row_text::<InputPriority>(&row, "priority")?,
                "client_input_id": row.get::<Option<String>, _>("client_input_id"),
                "content": content,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(event)
    }

    pub async fn cancel_queued_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<EventFrame> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
                update queued_inputs
                set status='cancelled'
                where session_id=$1 and id=$2::text and status='queued'
                returning priority, client_input_id, content
                "#,
        )
        .bind(session_id)
        .bind(input_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(anyhow!(
                "queued input is no longer editable or was not found: {input_id}"
            ));
        };
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::InputCancelled,
            json!({
                "input_id": input_id,
                "priority": row_text::<InputPriority>(&row, "priority")?,
                "client_input_id": row.get::<Option<String>, _>("client_input_id"),
                "content": row.get::<Value, _>("content"),
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(event)
    }

    pub async fn persist_outputs(
        &self,
        session_id: &str,
        entries: &[TranscriptStorageNode],
        active_leaf_id: Option<&str>,
        session_events: &[SessionEvent],
        actions: &[SessionAction],
        action_update: Option<ActionUpdate>,
        consumed_input: Option<QueuedInput>,
        accepted_input: Option<AcceptedInput>,
        config: &SessionConfig,
    ) -> Result<(Vec<EventFrame>, Vec<DispatchAction>)> {
        let mut tx = self.pool.begin().await?;
        let (frames, dispatch) = persist_outputs_tx(
            &mut tx,
            session_id,
            entries,
            active_leaf_id,
            session_events,
            actions,
            action_update,
            consumed_input,
            accepted_input,
            config,
        )
        .await?;
        tx.commit().await?;
        Ok((frames, dispatch))
    }

    pub async fn insert_event(
        &self,
        session_id: &str,
        event: EventType,
        data: Value,
    ) -> Result<EventFrame> {
        insert_event_pool(&self.pool, session_id, event, data).await
    }

    pub async fn events_after(
        &self,
        session_id: &str,
        after: Option<i64>,
    ) -> Result<Vec<EventFrame>> {
        let after = after.unwrap_or(0);
        let rows = sqlx::query(
            "select id, session_id, type, payload from events where session_id=$1 and id>$2 order by id",
        )
        .bind(session_id)
        .bind(after)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_event).collect()
    }

    pub async fn activity(&self, session_id: &str) -> Result<SessionActivity> {
        if self.has_unfinished_actions(session_id).await? {
            return Ok(SessionActivity::Running);
        }
        let queued: bool = sqlx::query_scalar(
            "select exists(select 1 from queued_inputs where session_id=$1 and status in ('queued','consuming'))",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        if queued {
            Ok(SessionActivity::Queued)
        } else {
            Ok(SessionActivity::Idle)
        }
    }

    pub async fn has_unfinished_actions(&self, session_id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "select exists(select 1 from actions where session_id=$1 and status in ('pending','running'))",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn has_queued_inputs(&self, session_id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "select exists(select 1 from queued_inputs where session_id=$1 and status in ('queued','consuming'))",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn reset_abandoned_consuming_inputs(&self, session_id: &str) -> Result<()> {
        sqlx::query(
            r#"
                update queued_inputs
                set status='queued',
                    origin=(coalesce(origin, '{}'::jsonb) - 'claim_id' - 'claimed_at')
                where session_id=$1 and status='consuming'
                "#,
        )
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn reset_consuming_input(
        &self,
        session_id: &str,
        input_id: &str,
        claim_id: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
                update queued_inputs
                set status='queued',
                    origin=(coalesce(origin, '{}'::jsonb) - 'claim_id' - 'claimed_at')
                where session_id=$1
                    and id=$2::text
                    and status='consuming'
                    and origin->>'claim_id'=$3
                "#,
        )
        .bind(session_id)
        .bind(input_id)
        .bind(claim_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn session_snapshot(&self, session_id: &str) -> Result<Value> {
        let session = sqlx::query(
            "select id, active_leaf_id, provider_config, metadata from sessions where id=$1",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        let provider: ProviderConfig =
            serde_json::from_value(session.get::<Value, _>("provider_config"))?;
        let actions = sqlx::query(
            "select id, kind, status, payload from actions where session_id=$1 and status in ('pending','running') order by created_at",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        let last_event_id: i64 = sqlx::query_scalar(
            "select coalesce(max(id),0)::bigint from events where session_id=$1",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        let queued: bool = sqlx::query_scalar(
            "select exists(select 1 from queued_inputs where session_id=$1 and status in ('queued','consuming'))",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        let queued_inputs = sqlx::query(
            r#"
                select id,
                    priority,
                    status,
                    content,
                    client_input_id,
                    created_at::text as created_at,
                    origin->>'promoted_at' as promoted_at
                from queued_inputs
                where session_id=$1 and status in ('queued','consuming')
                order by
                    case priority when 'steer' then 0 else 1 end,
                    case
                        when priority='steer'
                        then coalesce((origin->>'promoted_at')::timestamptz, created_at)
                        else created_at
                    end,
                    created_at
                "#,
        )
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
        Ok(json!({
            "session_id": session.get::<String, _>("id"),
            "activity": activity,
            "active_leaf_id": session.get::<Option<String>, _>("active_leaf_id"),
            "provider": provider,
            "metadata": session.get::<Value, _>("metadata"),
            "pending_actions": actions.into_iter().map(|row| {
                Ok(json!({
                    "action_row_id": row.get::<String, _>("id"),
                    "kind": row_text::<ActionKind>(&row, "kind")?,
                    "status": row_text::<ActionStatus>(&row, "status")?,
                    "payload": row.get::<Value, _>("payload"),
                }))
            }).collect::<Result<Vec<_>>>()?,
            "queued_inputs": queued_inputs.into_iter().map(|row| {
                let content = row
                    .get::<Value, _>("content")
                    .get("content")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                Ok(json!({
                    "input_id": row.get::<String, _>("id"),
                    "priority": row_text::<InputPriority>(&row, "priority")?,
                    "status": row_text::<QueuedInputStatus>(&row, "status")?,
                    "content": content,
                    "client_input_id": row.get::<Option<String>, _>("client_input_id"),
                    "created_at": row.get::<String, _>("created_at"),
                    "promoted_at": row.get::<Option<String>, _>("promoted_at"),
                }))
            }).collect::<Result<Vec<_>>>()?,
            "last_event_id": last_event_id,
        }))
    }

    pub async fn history_tree(&self, session_id: &str) -> Result<Value> {
        let stored = self.load_stored_session(session_id).await?;
        Ok(json!({
            "session_id": session_id,
            "active_leaf_id": stored.active_leaf_id,
            "entries": stored.entries,
        }))
    }

    pub async fn set_active_leaf(
        &self,
        session_id: &str,
        leaf_id: Option<&str>,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("update sessions set active_leaf_id=$2::text, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(leaf_id)
            .execute(&mut *tx)
            .await?;
        let event = insert_event_tx(
            &mut tx,
            session_id,
            EventType::HistoryRewound,
            json!({ "active_leaf_id": leaf_id }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn create_fork(
        &self,
        source_session_id: &str,
        new_session_id: &str,
        config: &SessionConfig,
        entries: &[TranscriptStorageNode],
        target_leaf_id: &str,
        active_leaf_id: Option<String>,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into sessions (id, active_leaf_id, provider_config, metadata) values ($1, $2::text, $3, $4)",
        )
        .bind(new_session_id)
        .bind(active_leaf_id.as_deref())
        .bind(serde_json::to_value(&config.provider)?)
        .bind(&config.metadata)
        .execute(&mut *tx)
        .await?;
        for entry in entries {
            insert_entry_tx(&mut tx, new_session_id, entry).await?;
        }
        let event = insert_event_tx(
            &mut tx,
            source_session_id,
            EventType::HistoryForked,
            json!({
                "new_session_id": new_session_id,
                "leaf_id": target_leaf_id,
                "active_leaf_id": active_leaf_id,
            }),
        )
        .await?;
        let created = insert_event_tx(
            &mut tx,
            new_session_id,
            EventType::SessionCreated,
            json!({
                "session_id": new_session_id,
                "forked_from": source_session_id,
                "source_leaf_id": target_leaf_id,
                "active_leaf_id": active_leaf_id,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event, created])
    }

    pub async fn load_action(&self, session_id: &str, action_row_id: &str) -> Result<StoredAction> {
        let row = sqlx::query(
            "select kind, action_id, turn_id, attempt_id from actions where session_id=$1 and id=$2::text and status in ('pending','running')",
        )
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
        Ok(sqlx::query_scalar(
            r#"
                select exists(
                    select 1
                    from actions
                    where session_id=$1 and id=$2::text and attempt_id=$3::text and status in ('pending','running')
                )
                "#,
            )
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
        let updated = sqlx::query(
            "update actions set status='running', updated_at=now() where session_id=$1 and id=$2::text and attempt_id=$3::text and status in ('pending','running')",
        )
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
        sqlx::query(
            "update actions set status='stale', updated_at=now() where session_id=$1 and id=$2::text and status in ('pending','running')",
        )
        .bind(session_id)
        .bind(action_row_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn recover_session(
        &self,
        session_id: &str,
        entries: &[StoredTranscriptEntry],
        active_leaf_id: Option<&str>,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        let mut frames = Vec::new();
        for entry in entries {
            insert_stored_entry_tx(&mut tx, session_id, entry).await?;
            frames.extend(
                insert_transcript_item_events_tx(&mut tx, session_id, &entry.id, &entry.item)
                    .await?,
            );
        }
        sqlx::query(
            "update actions set status='stale', updated_at=now() where session_id=$1 and status in ('pending','running')",
        )
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("update sessions set active_leaf_id=$2::text, updated_at=now() where id=$1")
            .bind(session_id)
            .bind(active_leaf_id)
            .execute(&mut *tx)
            .await?;
        frames.push(
            insert_event_tx(
                &mut tx,
                session_id,
                EventType::SessionRecovered,
                json!({ "active_leaf_id": active_leaf_id }),
            )
            .await?,
        );
        tx.commit().await?;
        Ok(frames)
    }
}

fn action_event_matches_row(
    row_kind: ActionKind,
    row_action_id: i64,
    event_kind: &SessionActionKind,
    event_id: &str,
) -> bool {
    let event_kind = match event_kind {
        SessionActionKind::Model => ActionKind::Model,
        SessionActionKind::Tool => ActionKind::Tool,
        SessionActionKind::Compaction => ActionKind::Compaction,
    };
    row_kind == event_kind && event_id.parse::<i64>().ok() == Some(row_action_id)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ActionKey {
    kind: ActionKind,
    action_id: i64,
}

impl ActionKey {
    pub fn new(kind: ActionKind, action_id: i64) -> Self {
        Self { kind, action_id }
    }
}

fn action_payload(action: &SessionAction) -> Result<(ActionKind, i64, Option<i64>, Value)> {
    match action {
        SessionAction::RequestModel {
            action_id,
            turn_id,
            model_context,
            context_leaf_id,
            context_tokens,
        } => Ok((
            ActionKind::Model,
            action_id.0 as i64,
            Some(turn_id.0 as i64),
            json!({
                "model_context": model_context.transcript_items(),
                "context_leaf_id": context_leaf_id,
                "context_tokens": context_tokens,
            }),
        )),
        SessionAction::RequestTool {
            action_id,
            turn_id,
            tool_call,
        } => Ok((
            ActionKind::Tool,
            action_id.0 as i64,
            Some(turn_id.0 as i64),
            serde_json::to_value(tool_call)?,
        )),
        SessionAction::CancelSessionWork => {
            Ok((ActionKind::Cancel, 0, None, json!({ "scope": "session" })))
        }
        SessionAction::RequestCompaction {
            request_id,
            model_context,
            context_leaf_id,
            context_tokens,
        } => Ok((
            ActionKind::Compaction,
            request_id.0 as i64,
            None,
            json!({
                "model_context": model_context.transcript_items(),
                "context_leaf_id": context_leaf_id,
                "context_tokens": context_tokens,
            }),
        )),
    }
}

async fn persist_outputs_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entries: &[TranscriptStorageNode],
    active_leaf_id: Option<&str>,
    session_events: &[SessionEvent],
    actions: &[SessionAction],
    action_update: Option<ActionUpdate>,
    consumed_input: Option<QueuedInput>,
    accepted_input: Option<AcceptedInput>,
    config: &SessionConfig,
) -> Result<(Vec<EventFrame>, Vec<DispatchAction>)> {
    for entry in entries {
        insert_entry_tx(tx, session_id, entry)
            .await
            .with_context(|| format!("insert transcript entry {}", entry.id))?;
    }
    sqlx::query("update sessions set active_leaf_id=$2::text, updated_at=now() where id=$1")
        .bind(session_id)
        .bind(active_leaf_id)
        .execute(&mut **tx)
        .await
        .context("update session active leaf")?;

    let mut frames = Vec::new();
    if let Some(input) = consumed_input {
        let updated = sqlx::query(
            r#"
                update queued_inputs
                set status='consumed',
                    origin=coalesce(origin, '{}'::jsonb)
                        || jsonb_build_object('consumed_at', now()::text)
                where id=$1
                    and session_id=$2::text
                    and status='consuming'
                    and origin->>'claim_id'=$3
                "#,
        )
        .bind(&input.id)
        .bind(session_id)
        .bind(&input.claim_id)
        .execute(&mut **tx)
        .await
        .context("mark queued input consumed")?
        .rows_affected();
        if updated != 1 {
            return Err(anyhow!("queued input was already consumed: {}", input.id));
        }
        frames.push(
            insert_event_tx(
                tx,
                session_id,
                EventType::InputConsumed,
                json!({
                    "input_id": input.id,
                    "priority": input.priority,
                    "client_input_id": input.client_input_id,
                }),
            )
            .await
            .context("insert input.consumed event")?,
        );
    }

    if let Some(input) = accepted_input {
        let mut input_id = None;
        if let Some(client_input_id) = input.client_input_id.as_deref() {
            let id = format!("input_{}", Uuid::new_v4());
            let inserted = sqlx::query(
                r#"
                    insert into queued_inputs (id, session_id, priority, content, status, client_input_id)
                    values ($1, $2, $3, $4, 'consumed', $5)
                    on conflict (session_id, client_input_id) where client_input_id is not null
                    do nothing
                    returning id
                    "#,
            )
            .bind(&id)
            .bind(session_id)
            .bind(input.priority.as_str())
            .bind(serde_json::to_value(&input.content)?)
            .bind(client_input_id)
            .fetch_optional(&mut **tx)
            .await
            .context("record accepted input")?;
            let Some(row) = inserted else {
                return Err(anyhow!("input already recorded: {client_input_id}"));
            };
            input_id = Some(row.get::<String, _>("id"));
        }

        frames.push(
            insert_event_tx(
                tx,
                session_id,
                EventType::InputAccepted,
                json!({
                    "input_id": input_id,
                    "priority": input.priority,
                    "client_input_id": input.client_input_id,
                    "content": input.content,
                }),
            )
            .await
            .context("insert input.accepted event")?,
        );
    }

    if let Some(mut update) = action_update {
        if let Some(row) = sqlx::query(
            r#"
                select kind, action_id
                from actions
                where session_id=$1 and id=$2::text and attempt_id=$3::text and status in ('pending','running')
                "#,
        )
        .bind(session_id)
        .bind(&update.row_id)
        .bind(&update.attempt_id)
        .fetch_optional(&mut **tx)
        .await
        .context("load action row for completion")?
        {
            let row_kind = row_text::<ActionKind>(&row, "kind")?;
            let row_action_id: i64 = row.get("action_id");
            for event in session_events {
                match event {
                    SessionEvent::ActionCompleted { kind, id }
                        if action_event_matches_row(row_kind, row_action_id, kind, id) =>
                    {
                        update.status = ActionStatus::Completed;
                    }
                    SessionEvent::ActionFailed { kind, id, error }
                        if action_event_matches_row(row_kind, row_action_id, kind, id) =>
                    {
                        update.status = ActionStatus::Error;
                        update.result = json!({ "error": error });
                    }
                    _ => {}
                }
            }
        }
        let updated = sqlx::query(
            r#"
                update actions
                set status=$4, result=$5, updated_at=now()
                where session_id=$1 and id=$2::text and attempt_id=$3::text and status in ('pending','running')
                "#,
        )
        .bind(session_id)
        .bind(&update.row_id)
        .bind(&update.attempt_id)
        .bind(update.status.as_str())
        .bind(&update.result)
        .execute(&mut **tx)
        .await
        .context("update completed action row")?
        .rows_affected();
        if updated != 1 {
            return Err(anyhow!(
                "action attempt is no longer running: {}",
                update.row_id
            ));
        }
    }

    let mut action_rows = HashMap::<ActionKey, String>::new();
    let mut dispatch = Vec::new();
    for action in actions {
        let (kind, action_id, turn_id, payload) = action_payload(action)?;
        if matches!(action, SessionAction::CancelSessionWork) {
            sqlx::query(
                r#"
                update actions
                set status='interrupted',
                    result='{"reason":"session interrupted"}'::jsonb,
                    updated_at=now()
                where session_id=$1 and status in ('pending','running')
                "#,
            )
            .bind(session_id)
            .execute(&mut **tx)
            .await
            .context("mark session work interrupted")?;
            continue;
        }

        let row_id = format!("action_{}", Uuid::new_v4());
        let attempt_id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            insert into actions (id, session_id, turn_id, action_id, attempt_id, kind, status, payload)
            values ($1::text, $2::text, $3::bigint, $4, $5::text, $6::text, 'running', $7)
            "#,
        )
        .bind(&row_id)
        .bind(session_id)
        .bind(turn_id)
        .bind(action_id)
        .bind(&attempt_id)
        .bind(kind.as_str())
        .bind(&payload)
        .execute(&mut **tx)
        .await
        .context("insert action row")?;
        action_rows.insert(ActionKey::new(kind, action_id), row_id.clone());
        dispatch.push(DispatchAction {
            row_id,
            attempt_id,
            action: action.clone(),
            config: config.clone(),
        });
    }

    for event in session_events {
        frames.extend(
            insert_session_event_tx(tx, session_id, event, &action_rows)
                .await
                .with_context(|| format!("insert session event {event:?}"))?,
        );
    }
    Ok((frames, dispatch))
}

async fn insert_session_event_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    event: &SessionEvent,
    action_rows: &HashMap<ActionKey, String>,
) -> Result<Vec<EventFrame>> {
    match event {
        SessionEvent::TranscriptItemAppended { entry_id, item } => {
            insert_transcript_item_events_tx(tx, session_id, entry_id, item).await
        }
        SessionEvent::ActionRequested { action } => {
            let (kind, action_id, _, payload) = action_payload(action)?;
            let row_id = action_rows.get(&ActionKey::new(kind, action_id)).cloned();
            let mut frames = vec![insert_event_tx(
                tx,
                session_id,
                EventType::ActionRequested,
                json!({ "kind": kind, "action_id": action_id, "action_row_id": row_id, "payload": payload }),
            )
            .await?];
            let event_name = match action {
                SessionAction::RequestModel { .. } => Some(EventType::ModelRequested),
                SessionAction::RequestTool { .. } => Some(EventType::ToolRequested),
                SessionAction::RequestCompaction { .. } => Some(EventType::CompactionRequested),
                SessionAction::CancelSessionWork => Some(EventType::SessionWorkCancelled),
            };
            if let Some(event_name) = event_name {
                frames.push(
                    insert_event_tx(
                        tx,
                        session_id,
                        event_name,
                        json!({ "action_row_id": row_id, "action_id": action_id }),
                    )
                    .await?,
                );
            }
            Ok(frames)
        }
        SessionEvent::ActionCompleted { kind, id } => {
            let event_name = match kind {
                SessionActionKind::Model => EventType::ModelCompleted,
                SessionActionKind::Tool => EventType::ToolCompleted,
                SessionActionKind::Compaction => EventType::CompactionCompleted,
            };
            Ok(vec![
                insert_event_tx(tx, session_id, event_name, json!({ "action_id": id })).await?,
            ])
        }
        SessionEvent::ActionFailed { kind, id, error } => {
            let event_name = match kind {
                SessionActionKind::Model => EventType::ModelError,
                SessionActionKind::Tool => EventType::ToolError,
                SessionActionKind::Compaction => EventType::CompactionError,
            };
            Ok(vec![
                insert_event_tx(
                    tx,
                    session_id,
                    event_name,
                    json!({ "action_id": id, "error": error }),
                )
                .await?,
            ])
        }
        SessionEvent::HistoryCompacted => Ok(vec![
            insert_event_tx(tx, session_id, EventType::HistoryCompacted, json!({})).await?,
        ]),
        SessionEvent::HistoryRewound => Ok(vec![
            insert_event_tx(tx, session_id, EventType::HistoryRewound, json!({})).await?,
        ]),
    }
}

async fn insert_transcript_item_events_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry_id: &str,
    item: &TranscriptItem,
) -> Result<Vec<EventFrame>> {
    let mut frames = vec![
        insert_event_tx(
            tx,
            session_id,
            EventType::TranscriptAppended,
            json!({ "entry_id": entry_id, "item": item }),
        )
        .await?,
    ];
    match item {
        TranscriptItem::TurnStarted { turn_id } => {
            frames.push(
                insert_event_tx(
                    tx,
                    session_id,
                    EventType::TurnStarted,
                    json!({ "turn_id": turn_id.0, "entry_id": entry_id }),
                )
                .await?,
            );
        }
        TranscriptItem::TurnFinished { turn_id, outcome } => {
            frames.push(
                insert_event_tx(
                    tx,
                    session_id,
                    EventType::TurnFinished,
                    json!({ "turn_id": turn_id.0, "outcome": outcome, "entry_id": entry_id }),
                )
                .await?,
            );
        }
        TranscriptItem::AssistantMessage(message) => {
            frames.push(
                insert_event_tx(
                    tx,
                    session_id,
                    EventType::AssistantMessage,
                    json!({ "entry_id": entry_id, "assistant": message }),
                )
                .await?,
            );
        }
        _ => {}
    }
    Ok(frames)
}

async fn insert_entry_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry: &TranscriptStorageNode,
) -> Result<()> {
    let stored = StoredTranscriptEntry {
        id: entry.id.clone(),
        parent_id: entry.parent_id.clone(),
        timestamp_ms: entry.timestamp_ms,
        item: entry.item.clone(),
    };
    insert_stored_entry_tx(tx, session_id, &stored).await
}

async fn insert_stored_entry_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    entry: &StoredTranscriptEntry,
) -> Result<()> {
    let turn_id = entry.item.turn_id().map(|turn_id| turn_id.0 as i64);
    sqlx::query(
        r#"
        insert into transcript_entries (session_id, id, parent_id, timestamp_ms, item, turn_id)
        values ($1::text, $2::text, $3::text, $4, $5, $6::bigint)
        on conflict (session_id, id) do nothing
        "#,
    )
    .bind(session_id)
    .bind(&entry.id)
    .bind(&entry.parent_id)
    .bind(entry.timestamp_ms as i64)
    .bind(serde_json::to_value(&entry.item)?)
    .bind(turn_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_event_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    event_type: EventType,
    payload: Value,
) -> Result<EventFrame> {
    let row = sqlx::query(
        "insert into events (session_id, type, payload) values ($1::text, $2::text, $3) returning id, session_id, type, payload",
    )
    .bind(session_id)
    .bind(event_type.as_str())
    .bind(payload)
    .fetch_one(&mut **tx)
    .await?;
    row_to_event(row)
}

async fn insert_event_pool(
    pool: &PgPool,
    session_id: &str,
    event_type: EventType,
    payload: Value,
) -> Result<EventFrame> {
    let row = sqlx::query(
        "insert into events (session_id, type, payload) values ($1::text, $2::text, $3) returning id, session_id, type, payload",
    )
    .bind(session_id)
    .bind(event_type.as_str())
    .bind(payload)
    .fetch_one(pool)
    .await?;
    row_to_event(row)
}

fn row_to_event(row: PgRow) -> Result<EventFrame> {
    Ok(EventFrame {
        event_id: row.get("id"),
        session_id: row.get("session_id"),
        event: row_text::<EventType>(&row, "type")?,
        data: row.get("payload"),
    })
}

fn row_text<T>(row: &PgRow, column: &'static str) -> Result<T>
where
    T: FromStr<Err = String>,
{
    parse_text(row.get(column))
}

fn parse_text<T>(value: String) -> Result<T>
where
    T: FromStr<Err = String>,
{
    value.parse().map_err(anyhow::Error::msg)
}

fn row_to_stored_entry(row: &PgRow) -> Result<StoredTranscriptEntry> {
    Ok(StoredTranscriptEntry {
        id: row.get("id"),
        parent_id: row.get("parent_id"),
        timestamp_ms: row.get::<i64, _>("timestamp_ms") as u64,
        item: serde_json::from_value(row.get("item"))?,
    })
}
