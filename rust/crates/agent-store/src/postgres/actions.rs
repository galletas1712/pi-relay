use anyhow::{anyhow, Result};
use serde_json::json;
use sqlx::Row;

use crate::{
    ActionKind, ActionStatus, EventFrame, EventType, PendingDispatchAction, ResumableModelAction,
    StoredAction, TokenUsageEstimate, TranscriptStorageNode,
};

use super::action_records::model_action_context_leaf_id;
use super::events::insert_event_with_activity_tx;
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
        let row = sqlx::query(
            "select kind, action_id, turn_id, attempt_id from actions where session_id=$1 and id=$2::text and status='running'",
        )
            .bind(session_id)
            .bind(action_row_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("action not found or not active: {action_row_id}"))?;
        Ok(StoredAction {
            kind: row_text::<ActionKind>(&row, "kind")?,
            action_id: row.get("action_id"),
            turn_id: row.get("turn_id"),
            attempt_id: row.get("attempt_id"),
        })
    }

    pub async fn find_resumable_model_action(
        &self,
        session_id: &str,
        turn_id: agent_vocab::TurnId,
    ) -> Result<Option<ResumableModelAction>> {
        let statuses = [
            ActionStatus::Error,
            ActionStatus::Interrupted,
            ActionStatus::Stale,
        ]
        .into_iter()
        .map(|status| status.as_str().to_string())
        .collect::<Vec<_>>();
        let row = sqlx::query(
            r#"
            select action_id, turn_id, status, payload, result
            from actions
            where session_id=$1
                and turn_id=$2
                and kind=$3
                and status = any($4::text[])
            order by updated_at desc, created_at desc
            limit 1
            "#,
        )
        .bind(session_id)
        .bind(turn_id.0 as i64)
        .bind(ActionKind::Model.as_str())
        .bind(statuses)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let payload: serde_json::Value = row.get("payload");
        let context_leaf_id = payload
            .get("context_leaf_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("resumable model action has no context_leaf_id"))?
            .to_string();
        Ok(Some(ResumableModelAction {
            action_id: agent_vocab::ActionId(row.get::<i64, _>("action_id") as u64),
            turn_id: agent_vocab::TurnId(row.get::<i64, _>("turn_id") as u64),
            status: row_text::<ActionStatus>(&row, "status")?,
            context_leaf_id,
        }))
    }

    pub async fn latest_model_token_usage_estimate(
        &self,
        session_id: &str,
        leaf_id: &str,
    ) -> Result<Option<TokenUsageEstimate>> {
        let row = sqlx::query(
            r#"
            with recursive path as (
                select id, parent_id, 0 as depth
                from transcript_entries
                where session_id=$1 and id=$2::text
                union all
                select parent.id, parent.parent_id, path.depth + 1
                from transcript_entries parent
                join path on path.parent_id = parent.id
                where parent.session_id=$1
            ), latest_usage as (
                select a.result->'usage' as usage,
                    a.payload->>'context_leaf_id' as context_leaf_id,
                    p.depth
                from actions a
                join path p on p.id = a.payload->>'context_leaf_id'
                where a.session_id=$1
                    and a.kind='model'
                    and a.status in ('completed','error')
                    and a.result->'usage' is not null
                    and (
                        a.result->'usage'->>'total_tokens' is not null
                        or a.result->'usage'->>'input_tokens' is not null
                    )
                order by p.depth asc, a.updated_at desc, a.created_at desc
                limit 1
            )
            select path.id, path.parent_id, path.depth, entry.item, entry.provider_replay,
                latest_usage.usage, latest_usage.context_leaf_id
            from latest_usage
            join path on path.depth < latest_usage.depth
            join transcript_entries entry
                on entry.session_id=$1 and entry.id = path.id
            union all
            select null::text as id, null::text as parent_id, null::integer as depth,
                null::jsonb as item, null::jsonb as provider_replay,
                latest_usage.usage, latest_usage.context_leaf_id
            from latest_usage
            where latest_usage.depth = 0
            order by depth desc nulls last
            "#,
        )
        .bind(session_id)
        .bind(leaf_id)
        .fetch_all(&self.pool)
        .await?;
        let Some(first) = row.first() else {
            return Ok(None);
        };
        let usage: serde_json::Value = first.get("usage");
        let input_tokens = usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize);
        let output_tokens = usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize);
        let base_tokens = usage
            .get("total_tokens")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize)
            .or_else(|| {
                input_tokens
                    .zip(output_tokens)
                    .map(|(input, output)| input.saturating_add(output))
            })
            .or(input_tokens)
            .unwrap_or_default();
        if base_tokens == 0 {
            return Ok(None);
        }
        let suffix_start_leaf_id: String = first.get("context_leaf_id");
        let suffix_entries = row
            .into_iter()
            .filter_map(|row| {
                let id: Option<String> = row.get("id");
                id.map(|id| {
                    let parent_id: Option<String> = row.get("parent_id");
                    let item = serde_json::from_value(row.get("item"))?;
                    let provider_replay = serde_json::from_value(row.get("provider_replay"))?;
                    Ok(TranscriptStorageNode {
                        id,
                        parent_id,
                        timestamp_ms: 0,
                        item,
                        provider_replay,
                    })
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(
            TokenUsageEstimate {
                total_tokens: base_tokens,
                base_tokens,
                estimated_suffix_tokens: 0,
                suffix_start_leaf_id: Some(suffix_start_leaf_id),
                suffix_entries: Vec::new(),
            }
            .with_suffix_entries(suffix_entries),
        ))
    }

    pub async fn action_can_complete(
        &self,
        session_id: &str,
        action_row_id: &str,
        attempt_id: &str,
    ) -> Result<bool> {
        let query = r#"
                select exists(
                    select 1
                    from actions
                    where session_id=$1
                        and id=$2::text
                        and attempt_id=$3::text
                        and status='running'
                )
                "#;
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
        let query = "update actions set status='running', updated_at=now() where session_id=$1 and id=$2::text and attempt_id=$3::text and status='pending'";
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
        let event = insert_event_with_activity_tx(
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

    pub async fn pending_actions_for_dispatch(
        &self,
        session_id: &str,
    ) -> Result<Vec<PendingDispatchAction>> {
        let rows = sqlx::query(
            r#"
            select session_id, id, attempt_id, kind, action_id, turn_id, payload
            from actions
            where session_id=$1 and status='pending'
            order by created_at
            "#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        let mut actions = Vec::new();
        for row in rows {
            match row_text::<ActionKind>(&row, "kind") {
                Ok(ActionKind::Model) => {
                    actions.push(self.pending_model_dispatch_from_row(row).await?)
                }
                Ok(ActionKind::Tool) => actions.push(pending_tool_dispatch_from_row(row)?),
                Ok(ActionKind::Compaction) => {}
                Err(error) => return Err(anyhow!(error)),
            }
        }
        Ok(actions)
    }

    async fn pending_model_dispatch_from_row(
        &self,
        row: sqlx::postgres::PgRow,
    ) -> Result<PendingDispatchAction> {
        let payload: serde_json::Value = row.get("payload");
        let context_leaf_id = model_action_context_leaf_id(&payload)
            .ok_or_else(|| anyhow!("pending model action missing context_leaf_id"))?;
        let model_context = self
            .model_context_for_leaf(row.get("session_id"), &context_leaf_id)
            .await?;
        Ok(PendingDispatchAction {
            row_id: row.get("id"),
            attempt_id: row.get("attempt_id"),
            action: agent_session::SessionAction::RequestModel {
                action_id: agent_vocab::ActionId(row.get::<i64, _>("action_id") as u64),
                turn_id: agent_vocab::TurnId(row.get::<i64, _>("turn_id") as u64),
                model_context,
                context_leaf_id: Some(context_leaf_id),
            },
        })
    }

    pub async fn claim_pending_model_action(
        &self,
        session_id: &str,
        action_row_id: &str,
        attempt_id: &str,
    ) -> Result<bool> {
        let updated = sqlx::query(
            "update actions set status='running', updated_at=now() where session_id=$1 and id=$2::text and attempt_id=$3::text and kind='model' and status='pending'",
        )
        .bind(session_id)
        .bind(action_row_id)
        .bind(attempt_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    pub async fn fail_blocked_or_pending_model_action(
        &self,
        session_id: &str,
        action_row_id: &str,
        attempt_id: &str,
        error: &str,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            r#"
            update actions
            set status=$4::text,
                result=$5,
                updated_at=now()
            where session_id=$1
                and id=$2::text
                and attempt_id=$3::text
                and kind='model'
                and status in ('pending','blocked')
            "#,
        )
        .bind(session_id)
        .bind(action_row_id)
        .bind(attempt_id)
        .bind(ActionStatus::Error.as_str())
        .bind(serde_json::json!({ "error": error }))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let event = insert_event_with_activity_tx(
            &mut tx,
            session_id,
            EventType::ModelError,
            serde_json::json!({
                "action_row_id": action_row_id,
                "error": error,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    pub async fn cancel_unfinished_session_work(
        &self,
        session_id: &str,
        reason: &str,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        let unfinished_actions = action_is_unfinished(None);
        let query = format!(
            r#"
            update actions
            set status=$2::text,
                result=$3,
                updated_at=now()
            where session_id=$1 and {unfinished_actions}
            "#
        );
        let updated = sqlx::query(&query)
            .bind(session_id)
            .bind(ActionStatus::Interrupted.as_str())
            .bind(json!({ "reason": reason }))
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if updated == 0 {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let event = insert_event_with_activity_tx(
            &mut tx,
            session_id,
            EventType::SessionWorkCancelled,
            json!({ "reason": reason, "actions_interrupted": updated }),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }
}

fn pending_tool_dispatch_from_row(row: sqlx::postgres::PgRow) -> Result<PendingDispatchAction> {
    let payload: serde_json::Value = row.get("payload");
    Ok(PendingDispatchAction {
        row_id: row.get("id"),
        attempt_id: row.get("attempt_id"),
        action: agent_session::SessionAction::RequestTool {
            action_id: agent_vocab::ActionId(row.get::<i64, _>("action_id") as u64),
            turn_id: agent_vocab::TurnId(row.get::<i64, _>("turn_id") as u64),
            tool_call: serde_json::from_value(payload)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_session::{ModelContext, SessionAction};
    use agent_vocab::{
        ActionId, AssistantItem, AssistantMessage, ProviderConfig, ProviderKind, ReasoningEffort,
        ToolCall, ToolCallId, ToolResultMessage, TranscriptItem, TurnId, TurnOutcome, UserMessage,
    };
    use serde_json::json;
    use uuid::Uuid;

    use crate::{InputPriority, SessionActivity, SessionConfig};

    use super::*;

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(1);

    struct TestDb {
        store: PostgresAgentStore,
        admin_url: String,
        name: String,
    }

    impl TestDb {
        async fn cleanup(self) {
            self.store.close().await;
            if let Ok(admin) = sqlx::PgPool::connect(&self.admin_url).await {
                let _ = sqlx::query(&format!(r#"drop database if exists "{}""#, self.name))
                    .execute(&admin)
                    .await;
                admin.close().await;
            }
        }
    }

    async fn test_store() -> Option<TestDb> {
        let admin_url = std::env::var("PI_RELAY_TEST_DATABASE_URL").ok()?;
        let name = format!(
            "pi_relay_cancel_test_{}_{}",
            std::process::id(),
            TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = sqlx::PgPool::connect(&admin_url)
            .await
            .expect("connect to PI_RELAY_TEST_DATABASE_URL");
        sqlx::query(&format!(r#"create database "{name}""#))
            .execute(&admin)
            .await
            .expect("create isolated test database");
        admin.close().await;
        let database_url = database_url_with_name(&admin_url, &name);
        let store = PostgresAgentStore::connect(&database_url)
            .await
            .expect("connect isolated test database");
        store
            .migrate()
            .await
            .expect("migrate isolated test database");
        Some(TestDb {
            store,
            admin_url,
            name,
        })
    }

    fn database_url_with_name(base: &str, name: &str) -> String {
        let (prefix, query) = base
            .split_once('?')
            .map(|(prefix, query)| (prefix, format!("?{query}")))
            .unwrap_or((base, String::new()));
        let Some((root, _)) = prefix.rsplit_once('/') else {
            return format!("{base}_{name}");
        };
        format!("{root}/{name}{query}")
    }

    fn session_config(project_id: Uuid) -> SessionConfig {
        SessionConfig {
            project_id,
            starting_cwd: "/tmp".to_string(),
            provider: ProviderConfig {
                kind: ProviderKind::OpenAi,
                model: "test-model".to_string(),
                reasoning_effort: ReasoningEffort::Medium,
                max_tokens: None,
                prompt_cache: None,
            },
            metadata: json!({}),
        }
    }

    async fn create_session(store: &PostgresAgentStore, session_id: &str) -> SessionConfig {
        let project_id = Uuid::new_v4();
        store
            .create_project(project_id, "test", "/tmp", json!({}))
            .await
            .expect("project creates");
        let config = session_config(project_id);
        store
            .start_session_outputs(
                session_id,
                &config,
                &[],
                None,
                &[],
                &[],
                InputPriority::FollowUp,
                &UserMessage::text("seed"),
                None,
            )
            .await
            .expect("session starts");
        config
    }

    #[tokio::test]
    async fn cancel_unfinished_session_work_marks_compaction_idle_without_active_runtime() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "cancel-compaction";
        create_session(store, session_id).await;

        sqlx::query(
            r#"
            insert into actions (id, session_id, turn_id, action_id, attempt_id, kind, status, payload)
            values ('compaction_1', $1, null, 0, 'attempt_1', 'compaction', 'running', '{}')
            "#,
        )
        .bind(session_id)
        .execute(&store.pool)
        .await
        .expect("insert compaction action");

        let events = store
            .cancel_unfinished_session_work(session_id, "session interrupted")
            .await
            .expect("cancel succeeds");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, EventType::SessionWorkCancelled);
        assert_eq!(
            store.activity(session_id).await.unwrap(),
            SessionActivity::Idle
        );
        let status: String =
            sqlx::query_scalar("select status from actions where id='compaction_1'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(status, "interrupted");
        db.cleanup().await;
    }

    #[tokio::test]
    async fn cancel_unfinished_session_work_marks_model_and_tool_interrupted() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "cancel-actions";
        create_session(store, session_id).await;
        let actions = vec![
            SessionAction::RequestModel {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                model_context: ModelContext::default(),
                context_leaf_id: None,
            },
            SessionAction::RequestTool {
                action_id: ActionId(2),
                turn_id: TurnId(1),
                tool_call: ToolCall {
                    id: ToolCallId::from_u64(1),
                    tool_name: "bash".to_string(),
                    args_json: "{}".to_string(),
                },
            },
        ];
        store
            .persist_outputs(
                session_id,
                crate::OutputBatch::new(&[], None, &[], &actions),
            )
            .await
            .expect("actions persist");

        let events = store
            .cancel_unfinished_session_work(session_id, "session interrupted")
            .await
            .expect("cancel succeeds");
        assert_eq!(events.len(), 1);
        let statuses: Vec<String> =
            sqlx::query_scalar("select status from actions where session_id=$1 order by action_id")
                .bind(session_id)
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(statuses, vec!["interrupted", "interrupted"]);
        db.cleanup().await;
    }

    #[tokio::test]
    async fn cancel_unfinished_session_work_is_idempotent_after_first_cancel() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "cancel-idempotent";
        create_session(store, session_id).await;
        sqlx::query(
            r#"
            insert into actions (id, session_id, turn_id, action_id, attempt_id, kind, status, payload)
            values ('compaction_1', $1, null, 0, 'attempt_1', 'compaction', 'running', '{}')
            "#,
        )
        .bind(session_id)
        .execute(&store.pool)
        .await
        .expect("insert compaction action");

        let first = store
            .cancel_unfinished_session_work(session_id, "session interrupted")
            .await
            .expect("first cancel succeeds");
        let second = store
            .cancel_unfinished_session_work(session_id, "session interrupted")
            .await
            .expect("second cancel succeeds");

        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        db.cleanup().await;
    }
    #[tokio::test]
    async fn latest_model_token_usage_estimate_uses_server_total_plus_suffix_after_model_item() {
        let Some(db) = test_store().await else {
            eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let store = &db.store;
        let session_id = "token-usage-estimate-session";
        let project_id = Uuid::new_v4();
        store
            .create_project(project_id, "token usage test", "/tmp", json!({}))
            .await
            .expect("project creates");
        let config = session_config(project_id);

        let entries = vec![
            transcript_node(
                "leaf_turn1_start",
                None,
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            ),
            transcript_node(
                "leaf_turn1_user",
                Some("leaf_turn1_start"),
                TranscriptItem::UserMessage(UserMessage::text("first")),
            ),
            transcript_node(
                "leaf_turn1_assistant",
                Some("leaf_turn1_user"),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("done".to_string())],
                }),
            ),
            transcript_node(
                "leaf_turn1_done",
                Some("leaf_turn1_assistant"),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
            ),
            transcript_node(
                "leaf_turn2_start",
                Some("leaf_turn1_done"),
                TranscriptItem::TurnStarted { turn_id: TurnId(2) },
            ),
            transcript_node(
                "leaf_turn2_user",
                Some("leaf_turn2_start"),
                TranscriptItem::UserMessage(UserMessage::text("second")),
            ),
            transcript_node(
                "leaf_turn2_assistant",
                Some("leaf_turn2_user"),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("tool next".to_string())],
                }),
            ),
            transcript_node(
                "leaf_turn2_tool_result",
                Some("leaf_turn2_assistant"),
                TranscriptItem::ToolResult(ToolResultMessage::success(
                    ToolCallId::from_u64(9),
                    "Bash",
                    "suffix after assistant",
                )),
            ),
        ];
        store
            .start_session_outputs(
                session_id,
                &config,
                &entries,
                Some("leaf_turn2_tool_result"),
                &[],
                &[],
                InputPriority::FollowUp,
                &UserMessage::text("seed"),
                None,
            )
            .await
            .expect("session with transcript starts");
        sqlx::query(
            r#"
            insert into actions (id, session_id, turn_id, action_id, attempt_id, kind, status, payload, result)
            values ('model_usage_1', $1, 1, 1, 'attempt_1', 'model', 'completed',
                '{"context_leaf_id":"leaf_turn1_done"}'::jsonb,
                '{"usage":{"input_tokens":100,"output_tokens":25,"total_tokens":125}}'::jsonb)
            "#,
        )
        .bind(session_id)
        .execute(&store.pool)
        .await
        .expect("usage action inserts");

        let estimate = store
            .latest_model_token_usage_estimate(session_id, "leaf_turn2_tool_result")
            .await
            .expect("estimate loads")
            .expect("usage estimate exists");

        assert_eq!(estimate.base_tokens, 125);
        assert_eq!(
            estimate.suffix_start_leaf_id.as_deref(),
            Some("leaf_turn1_done")
        );
        let suffix_items = estimate
            .suffix_entries
            .iter()
            .map(|entry| &entry.item)
            .collect::<Vec<_>>();
        assert!(matches!(
            suffix_items.as_slice(),
            [
                TranscriptItem::TurnStarted { turn_id: TurnId(2) },
                TranscriptItem::UserMessage(_),
                TranscriptItem::AssistantMessage(_),
                TranscriptItem::ToolResult(_),
            ]
        ));
        db.cleanup().await;
    }

    fn transcript_node(
        id: &str,
        parent_id: Option<&str>,
        item: TranscriptItem,
    ) -> TranscriptStorageNode {
        TranscriptStorageNode {
            id: id.to_string(),
            parent_id: parent_id.map(str::to_string),
            timestamp_ms: 1,
            item,
            provider_replay: Vec::new(),
        }
    }
}
