use anyhow::Result;
use sqlx::Row;

use crate::{TokenUsageEstimate, TranscriptStorageNode};

use super::PostgresAgentStore;

impl PostgresAgentStore {
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
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_vocab::{
        AssistantItem, AssistantMessage, ProviderConfig, ProviderKind, ReasoningEffort, ToolCallId,
        ToolResultMessage, TranscriptItem, TurnId, TurnOutcome, UserMessage,
    };
    use serde_json::json;
    use uuid::Uuid;

    use crate::{InputPriority, PostgresAgentStore, SessionConfig, TranscriptStorageNode};

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(10_000);

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
            "pi_relay_token_usage_test_{}_{}",
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
            project_id: Some(project_id),
            outer_cwd: "/tmp".to_string(),
            workspaces: Vec::new(),
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
            .create_project(project_id, "token usage test", &[], json!({}))
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
