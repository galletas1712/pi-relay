use std::sync::atomic::{AtomicU64, Ordering};

use agent_session::TranscriptStorageNode;
use agent_vocab::{
    AssistantItem, AssistantMessage, ProviderConfig, ProviderKind, ProviderReplayItem,
    ReasoningEffort, TranscriptItem, TurnId, TurnOutcome, UserMessage,
};
use anyhow::{anyhow, Context, Result};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::{OutputBatch, SessionConfig};

use super::*;

static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(40_000);

struct TestDb {
    store: PostgresAgentStore,
    admin_url: String,
    name: String,
}

impl TestDb {
    async fn cleanup(self) -> Result<()> {
        self.store.close().await;
        drop_test_database(&self.admin_url, &self.name).await
    }
}

async fn test_store() -> Option<TestDb> {
    let db = test_store_without_schema()
        .await?
        .expect("create isolated test database");
    db.store
        .migrate()
        .await
        .expect("migrate isolated test database");
    Some(db)
}

async fn test_store_without_schema() -> Option<Result<TestDb>> {
    let admin_url = std::env::var("PI_RELAY_TEST_DATABASE_URL").ok()?;
    Some(create_test_store(admin_url).await)
}

async fn create_test_store(admin_url: String) -> Result<TestDb> {
    let name = format!(
        "pi_relay_sessions_test_{}_{}",
        std::process::id(),
        TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let admin = sqlx::PgPool::connect(&admin_url)
        .await
        .context("connect to PI_RELAY_TEST_DATABASE_URL")?;
    sqlx::query(&format!(r#"create database "{name}""#))
        .execute(&admin)
        .await
        .context("create isolated test database")?;
    admin.close().await;
    let database_url = database_url_with_name(&admin_url, &name);
    let store = match PostgresAgentStore::connect(&database_url)
        .await
        .context("connect isolated test database")
    {
        Ok(store) => store,
        Err(error) => {
            return finish_with_cleanup(Err(error), drop_test_database(&admin_url, &name).await);
        }
    };
    Ok(TestDb {
        store,
        admin_url,
        name,
    })
}

async fn drop_test_database(admin_url: &str, name: &str) -> Result<()> {
    let admin = sqlx::PgPool::connect(admin_url)
        .await
        .context("connect to test database admin for cleanup")?;
    sqlx::query(&format!(r#"drop database if exists "{name}""#))
        .execute(&admin)
        .await
        .with_context(|| format!("drop isolated test database {name}"))?;
    admin.close().await;
    Ok(())
}

fn finish_with_cleanup<T>(work: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (work, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(work), Ok(())) => Err(work),
        (Ok(_), Err(cleanup)) => Err(cleanup.context("test work succeeded but cleanup failed")),
        (Err(work), Err(cleanup)) => Err(anyhow!(
            "test work failed: {work:#}; cleanup also failed: {cleanup:#}"
        )),
    }
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
        runtime_id: "runtime-test".to_string(),
        workspace_id: "/tmp".to_string(),
        workspaces: Vec::new(),
        system_prompt: "test prompt".to_string(),
        provider: ProviderConfig {
            kind: ProviderKind::OpenAi,
            model: "test-model".to_string(),
            reasoning_effort: ReasoningEffort::Medium,
            max_tokens: None,
            prompt_cache: None,
        },
        metadata: json!({}),
        mcp_manifest: None,
    }
}

#[derive(Debug, Eq, PartialEq)]
struct IndexCatalogEntry {
    index_name: String,
    table_schema: String,
    table_name: String,
    access_method: String,
    key_columns: Vec<String>,
    included_columns: Vec<String>,
    has_expressions: bool,
    is_unique: bool,
    is_valid: bool,
    is_ready: bool,
    is_live: bool,
    predicate: Option<String>,
}

async fn queued_input_index_catalog(store: &PostgresAgentStore) -> Result<Vec<IndexCatalogEntry>> {
    let mut connection = store
        .pool
        .acquire()
        .await
        .context("acquire connection for queued input index catalog")?;
    sqlx::query("set quote_all_identifiers = off")
        .execute(&mut *connection)
        .await
        .context("set canonical quote_all_identifiers for index predicates")?;
    sqlx::query("set search_path = pg_catalog, public")
        .execute(&mut *connection)
        .await
        .context("set canonical search_path for index predicates")?;

    let rows = sqlx::query(
        r#"
        select
            index_relation.relname::text as index_name,
            table_namespace.nspname::text as table_schema,
            table_relation.relname::text as table_name,
            access_method.amname::text as access_method,
            array(
                select attribute.attname::text
                from unnest(index_catalog.indkey) with ordinality
                    as key_attribute(attribute_number, position)
                join pg_attribute as attribute
                  on attribute.attrelid = index_catalog.indrelid
                 and attribute.attnum = key_attribute.attribute_number
                where key_attribute.position <= index_catalog.indnkeyatts
                order by key_attribute.position
            ) as key_columns,
            array(
                select attribute.attname::text
                from unnest(index_catalog.indkey) with ordinality
                    as included_attribute(attribute_number, position)
                join pg_attribute as attribute
                  on attribute.attrelid = index_catalog.indrelid
                 and attribute.attnum = included_attribute.attribute_number
                where included_attribute.position > index_catalog.indnkeyatts
                order by included_attribute.position
            ) as included_columns,
            index_catalog.indexprs is not null as has_expressions,
            index_catalog.indisunique as is_unique,
            index_catalog.indisvalid as is_valid,
            index_catalog.indisready as is_ready,
            index_catalog.indislive as is_live,
            pg_get_expr(index_catalog.indpred, index_catalog.indrelid, false) as predicate
        from pg_index as index_catalog
        join pg_class as index_relation
          on index_relation.oid = index_catalog.indexrelid
        join pg_class as table_relation
          on table_relation.oid = index_catalog.indrelid
        join pg_namespace as table_namespace
          on table_namespace.oid = table_relation.relnamespace
        join pg_am as access_method
          on access_method.oid = index_relation.relam
        where index_relation.relname in (
              'queued_inputs_active_session_idx',
              'queued_inputs_non_cancelled_session_idx',
              'queued_inputs_follow_up_order_idx'
        )
        order by index_relation.relname
        "#,
    )
    .fetch_all(&mut *connection)
    .await
    .context("load queued input index catalog")?;

    rows.into_iter()
        .map(|row| {
            Ok(IndexCatalogEntry {
                index_name: row.try_get("index_name")?,
                table_schema: row.try_get("table_schema")?,
                table_name: row.try_get("table_name")?,
                access_method: row.try_get("access_method")?,
                key_columns: row.try_get("key_columns")?,
                included_columns: row.try_get("included_columns")?,
                has_expressions: row.try_get("has_expressions")?,
                is_unique: row.try_get("is_unique")?,
                is_valid: row.try_get("is_valid")?,
                is_ready: row.try_get("is_ready")?,
                is_live: row.try_get("is_live")?,
                predicate: row.try_get("predicate")?,
            })
        })
        .collect()
}

async fn queued_input_index_migration_results(
    store: &PostgresAgentStore,
) -> Result<(Vec<IndexCatalogEntry>, Vec<IndexCatalogEntry>)> {
    store
        .migrate()
        .await
        .context("fresh startup schema initialization")?;
    let fresh = queued_input_index_catalog(store).await?;
    store
        .migrate()
        .await
        .context("repeat startup schema initialization")?;
    let repeated = queued_input_index_catalog(store).await?;
    Ok((fresh, repeated))
}

#[tokio::test]
async fn migration_creates_exact_queued_input_indexes_idempotently() -> Result<()> {
    let Some(db) = test_store_without_schema().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return Ok(());
    };
    let db = db?;
    let expected = vec![
        IndexCatalogEntry {
            index_name: "queued_inputs_active_session_idx".to_string(),
            table_schema: "public".to_string(),
            table_name: "queued_inputs".to_string(),
            access_method: "btree".to_string(),
            key_columns: vec!["session_id".to_string()],
            included_columns: Vec::new(),
            has_expressions: false,
            is_unique: false,
            is_valid: true,
            is_ready: true,
            is_live: true,
            predicate: Some(
                "(status = ANY (ARRAY['queued'::text, 'consuming'::text]))".to_string(),
            ),
        },
        IndexCatalogEntry {
            index_name: "queued_inputs_follow_up_order_idx".to_string(),
            table_schema: "public".to_string(),
            table_name: "queued_inputs".to_string(),
            access_method: "btree".to_string(),
            key_columns: vec![
                "session_id".to_string(),
                "follow_up_position".to_string(),
                "created_at".to_string(),
                "id".to_string(),
            ],
            included_columns: Vec::new(),
            has_expressions: false,
            is_unique: false,
            is_valid: true,
            is_ready: true,
            is_live: true,
            predicate: Some(
                "((priority = 'follow_up'::text) AND (status = 'queued'::text))".to_string(),
            ),
        },
        IndexCatalogEntry {
            index_name: "queued_inputs_non_cancelled_session_idx".to_string(),
            table_schema: "public".to_string(),
            table_name: "queued_inputs".to_string(),
            access_method: "btree".to_string(),
            key_columns: vec!["session_id".to_string()],
            included_columns: Vec::new(),
            has_expressions: false,
            is_unique: false,
            is_valid: true,
            is_ready: true,
            is_live: true,
            predicate: Some("(status <> 'cancelled'::text)".to_string()),
        },
    ];

    let work = queued_input_index_migration_results(&db.store).await;
    let cleanup = db.cleanup().await;
    let (fresh, repeated) = finish_with_cleanup(work, cleanup)?;
    assert_eq!(fresh, expected, "fresh schema index catalog");
    assert_eq!(repeated, expected, "repeated schema index catalog");
    Ok(())
}

#[tokio::test]
async fn provider_route_migration_is_nullable_idempotent_and_rollback_compatible() -> Result<()> {
    let db = test_store_without_schema()
        .await
        .expect("PI_RELAY_TEST_DATABASE_URL is required for provider route tests")?;
    let work = async {
        db.store.migrate().await?;
        let project_id = Uuid::new_v4();
        db.store
            .create_project(
                project_id,
                "route migration",
                "runtime-test",
                &[],
                json!({}),
            )
            .await?;
        let config = session_config(project_id);
        db.store.create_session("legacy-route", &config).await?;

        // Simulate a populated database created by the prior release.
        sqlx::raw_sql(
            r#"
            alter table queued_inputs drop column provider_config;
            alter table actions drop column provider_config;
            insert into queued_inputs (
                id, session_id, priority, content, status, client_input_id
            ) values (
                'legacy-input', 'legacy-route', 'follow_up',
                '{"type":"user_message","content":{"content":[{"type":"text","text":"legacy"}]}}',
                'queued', 'legacy-input'
            );
            insert into actions (
                id, session_id, turn_id, action_id, attempt_id, kind, status, payload
            ) values (
                'legacy-action', 'legacy-route', 1, 1, 'legacy-attempt', 'model',
                'pending', '{"context_leaf_id":"legacy-leaf"}'
            );
            "#,
        )
        .execute(&db.store.pool)
        .await?;
        db.store.migrate().await?;
        db.store.migrate().await?;

        let columns: Vec<(String, String)> = sqlx::query_as(
            r#"
            select table_name, is_nullable
            from information_schema.columns
            where table_schema='public'
              and table_name in ('queued_inputs','actions')
              and column_name='provider_config'
            order by table_name
            "#,
        )
        .fetch_all(&db.store.pool)
        .await?;
        assert_eq!(
            columns,
            vec![
                ("actions".to_string(), "YES".to_string()),
                ("queued_inputs".to_string(), "YES".to_string()),
            ]
        );
        let legacy_nulls: (bool, bool) = sqlx::query_as(
            r#"
            select
                (select provider_config is null from queued_inputs where id='legacy-input'),
                (select provider_config is null from actions where id='legacy-action')
            "#,
        )
        .fetch_one(&db.store.pool)
        .await?;
        assert_eq!(legacy_nulls, (true, true));

        // An old daemon can still omit the new columns during rollback.
        sqlx::raw_sql(
            r#"
            insert into queued_inputs (
                id, session_id, priority, content, status, client_input_id
            ) values (
                'rollback-input', 'legacy-route', 'follow_up',
                '{"type":"user_message","content":{"content":[{"type":"text","text":"rollback"}]}}',
                'queued', 'rollback-input'
            );
            insert into actions (
                id, session_id, turn_id, action_id, attempt_id, kind, status, payload
            ) values (
                'rollback-action', 'legacy-route', 2, 2, 'rollback-attempt', 'tool',
                'pending', '{}'
            );
            "#,
        )
        .execute(&db.store.pool)
        .await?;
        db.store.migrate().await?;
        Ok(())
    }
    .await;
    finish_with_cleanup(work, db.cleanup().await)
}

#[test]
fn work_and_cleanup_errors_are_both_reported() {
    let error = finish_with_cleanup::<()>(
        Err(anyhow!("intentional work failure")),
        Err(anyhow!("intentional cleanup failure")),
    )
    .expect_err("combined failures return an error")
    .to_string();
    assert!(error.contains("intentional work failure"));
    assert!(error.contains("intentional cleanup failure"));
}

#[test]
fn compaction_auto_state_transitions_are_deterministic() {
    let metadata = json!({
        "unrelated": { "preserved": true },
        "compaction": {
            "config": { "max_consecutive_failures": 2 },
            "auto_state": {
                "consecutive_failures": 1,
                "last_success_leaf_id": "leaf-1",
                "consecutive_recompactions": 0
            }
        }
    });
    let failed =
        next_auto_compaction_failure_metadata(metadata, 99, "leaf-1", "provider unavailable");
    assert_eq!(
        failed.pointer("/compaction/auto_state/consecutive_failures"),
        Some(&json!(2))
    );
    assert_eq!(
        failed.pointer("/compaction/auto_state/suppressed"),
        Some(&json!(true))
    );
    assert_eq!(
        failed.pointer("/compaction/auto_state/last_failure_leaf_id"),
        Some(&json!("leaf-1"))
    );
    assert_eq!(failed.pointer("/unrelated/preserved"), Some(&json!(true)));

    let first_recompaction = next_compaction_success_metadata(failed, "leaf-1", "leaf-2", false);
    assert_eq!(
        first_recompaction.pointer("/compaction/auto_state/consecutive_recompactions"),
        Some(&json!(1))
    );
    assert_eq!(
        first_recompaction.pointer("/compaction/auto_state/consecutive_failures"),
        Some(&json!(0))
    );
    assert_eq!(
        first_recompaction.pointer("/compaction/auto_state/last_success_leaf_id"),
        Some(&json!("leaf-2"))
    );

    let manual = next_compaction_success_metadata(first_recompaction, "leaf-2", "leaf-3", true);
    assert_eq!(
        manual.pointer("/compaction/auto_state/consecutive_recompactions"),
        Some(&json!(0)),
        "manual boundary compaction starts a fresh automatic chain"
    );
}

#[test]
fn compaction_auto_state_counters_saturate() {
    let failed = next_auto_compaction_failure_metadata(
        json!({
            "compaction": {
                "auto_state": { "consecutive_failures": u64::MAX }
            }
        }),
        usize::MAX,
        "leaf",
        "failure",
    );
    assert_eq!(
        failed.pointer("/compaction/auto_state/consecutive_failures"),
        Some(&json!(u64::MAX))
    );

    let succeeded = next_compaction_success_metadata(
        json!({
            "compaction": {
                "auto_state": {
                    "last_success_leaf_id": "leaf",
                    "consecutive_recompactions": u64::MAX
                }
            }
        }),
        "leaf",
        "new-root",
        false,
    );
    assert_eq!(
        succeeded.pointer("/compaction/auto_state/consecutive_recompactions"),
        Some(&json!(u64::MAX))
    );
}

fn entry(
    id: &str,
    parent_id: Option<&str>,
    timestamp_ms: u64,
    item: TranscriptItem,
) -> TranscriptStorageNode {
    TranscriptStorageNode {
        id: id.to_string(),
        parent_id: parent_id.map(str::to_string),
        timestamp_ms,
        item,
        provider_replay: Vec::new(),
    }
}

fn turn_started(
    id: &str,
    parent_id: Option<&str>,
    timestamp_ms: u64,
    turn_id: u64,
) -> TranscriptStorageNode {
    entry(
        id,
        parent_id,
        timestamp_ms,
        TranscriptItem::TurnStarted {
            turn_id: TurnId(turn_id),
        },
    )
}

fn user_message(
    id: &str,
    parent_id: Option<&str>,
    timestamp_ms: u64,
    text: &str,
) -> TranscriptStorageNode {
    entry(
        id,
        parent_id,
        timestamp_ms,
        TranscriptItem::UserMessage(UserMessage::text(text)),
    )
}

fn assistant_message(
    id: &str,
    parent_id: Option<&str>,
    timestamp_ms: u64,
    text: &str,
) -> TranscriptStorageNode {
    entry(
        id,
        parent_id,
        timestamp_ms,
        TranscriptItem::AssistantMessage(AssistantMessage {
            items: vec![AssistantItem::Text(text.to_string())],
        }),
    )
}

fn turn_finished(
    id: &str,
    parent_id: Option<&str>,
    timestamp_ms: u64,
    turn_id: u64,
) -> TranscriptStorageNode {
    entry(
        id,
        parent_id,
        timestamp_ms,
        TranscriptItem::TurnFinished {
            turn_id: TurnId(turn_id),
            outcome: TurnOutcome::Graceful,
        },
    )
}

async fn create_project_session(store: &PostgresAgentStore, project_id: Uuid, session_id: &str) {
    store
        .create_session(session_id, &session_config(project_id))
        .await
        .expect("session creates");
}

async fn persist_turn(store: &PostgresAgentStore, session_id: &str, timestamp_ms: u64, text: &str) {
    let active_leaf_id = format!("{session_id}_finish");
    let entries = vec![
        turn_started(
            &format!("{session_id}_start"),
            None,
            timestamp_ms.saturating_sub(1),
            1,
        ),
        user_message(
            &format!("{session_id}_user"),
            Some(&format!("{session_id}_start")),
            timestamp_ms,
            text,
        ),
        assistant_message(
            &format!("{session_id}_assistant"),
            Some(&format!("{session_id}_user")),
            timestamp_ms.saturating_add(1),
            "ok",
        ),
        turn_finished(
            &format!("{session_id}_finish"),
            Some(&format!("{session_id}_assistant")),
            timestamp_ms.saturating_add(2),
            1,
        ),
    ];
    store
        .persist_outputs(
            session_id,
            OutputBatch::new(&entries, Some(active_leaf_id.as_str()), &[], &[]),
        )
        .await
        .expect("turn persists");
}

#[tokio::test]
async fn manual_compaction_failure_terminalizes_without_installing_a_checkpoint() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    let session_id = "manual_compaction_failure";
    store
        .create_project(
            project_id,
            "manual compaction failure",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    create_project_session(store, project_id, session_id).await;
    persist_turn(store, session_id, 1_700_000_000_000, "compact me").await;
    let before = store
        .session_snapshot(session_id)
        .await
        .expect("snapshot before compaction");
    let created = store
        .create_compaction_action(session_id, crate::CompactionTrigger::Manual)
        .await
        .expect("manual compaction action creates");

    let events = store
        .fail_compaction_action(
            &created.job,
            &session_config(project_id),
            "typed provider failure".to_string(),
        )
        .await
        .expect("manual compaction failure commits");

    let after = store
        .session_snapshot(session_id)
        .await
        .expect("snapshot after compaction failure");
    assert_eq!(after.active_leaf_id, before.active_leaf_id);
    assert!(
        after
            .pending_actions
            .iter()
            .all(|action| action.action_row_id != created.job.action_row_id),
        "failed manual compaction must not remain unfinished"
    );
    assert_eq!(after.metadata.pointer("/compaction/auto_state"), None);
    let error = events
        .iter()
        .find(|event| event.event == crate::EventType::CompactionError)
        .expect("compaction error event persists");
    assert_eq!(error.data["trigger"], "manual");
    assert_eq!(error.data["error"], "typed provider failure");

    db.cleanup().await.expect("clean up isolated test database");
}

#[tokio::test]
async fn compaction_provider_replay_round_trips_nullable_and_omitted_encrypted_content() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "compaction replay test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");

    for (label, encrypted_content) in [
        ("string", Some(json!("opaque+/= ciphertext"))),
        ("null", Some(serde_json::Value::Null)),
        ("omitted", None),
    ] {
        let session_id = format!("compaction_replay_{label}");
        create_project_session(store, project_id, &session_id).await;
        persist_turn(store, &session_id, 1_700_000_000_000, "compact me").await;
        let created = store
            .create_compaction_action(&session_id, crate::CompactionTrigger::Manual)
            .await
            .expect("manual compaction action creates");
        let mut block = json!({
            "type": "compaction",
            "content": "opaque summary",
            "provider_extension": { "preserve": true }
        });
        if let Some(value) = encrypted_content {
            block
                .as_object_mut()
                .unwrap()
                .insert("encrypted_content".to_string(), value);
        }
        let replay = ProviderReplayItem::new(ProviderKind::Claude, &block)
            .expect("provider replay item creates");
        store
            .complete_compaction_action(
                &created.job,
                crate::CompactionCompletion {
                    summary: String::new(),
                    summary_kind: "generic".to_string(),
                    provider_replay: vec![replay],
                    provider: ProviderKind::Claude,
                    usage: None,
                    continuation_suffix: Vec::new(),
                },
            )
            .await
            .expect("compaction completes");

        let stored = store
            .load_stored_session(&session_id)
            .await
            .expect("compacted session reloads");
        let checkpoint = stored.entries.last().expect("checkpoint persists");
        assert_eq!(checkpoint.provider_replay.len(), 1, "{label}");
        assert_eq!(
            checkpoint.provider_replay[0].raw_value().unwrap(),
            block,
            "{label}"
        );
    }

    db.cleanup().await.expect("clean up isolated test database");
}

#[tokio::test]
async fn subagent_type_round_trips_through_start_session_outputs() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "subagent type test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    create_project_session(store, project_id, "session_parent").await;

    for (session_id, subagent_type) in [
        ("session_full_child", Some(crate::SubagentType::Full)),
        ("session_ro_child", Some(crate::SubagentType::ReadOnly)),
    ] {
        store
            .start_session_outputs_with_parent(
                session_id,
                &session_config(project_id),
                &[],
                None,
                &[],
                &[],
                crate::InputPriority::FollowUp,
                &UserMessage::text("go"),
                None,
                Some("session_parent"),
                subagent_type,
                None,
            )
            .await
            .expect("child session starts");
        assert_eq!(
            store
                .session_subagent_type(session_id)
                .await
                .expect("subagent type loads"),
            subagent_type,
        );
    }

    // A top-level session carries no subagent type.
    assert_eq!(
        store
            .session_subagent_type("session_parent")
            .await
            .expect("parent subagent type loads"),
        None,
    );

    db.cleanup().await.expect("clean up isolated test database");
}

#[tokio::test]
async fn delete_session_rejects_active_queued_input_under_session_lock() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "delete guard test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    create_project_session(store, project_id, "delete_guard").await;

    store
        .enqueue_user_input(
            "delete_guard",
            crate::InputPriority::FollowUp,
            &UserMessage::text("accepted before delete"),
            Some("delete-guard-input"),
            None,
        )
        .await
        .expect("queued input enqueues");

    let deleted = store.delete_session("delete_guard").await;
    assert!(deleted
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<crate::SourceMutationConflict>())
        .is_some());
    assert!(store
        .session_exists("delete_guard")
        .await
        .expect("session existence loads"));
    assert_eq!(
        store
            .queue_state("delete_guard")
            .await
            .expect("queue state loads")
            .queued_inputs
            .len(),
        1
    );

    db.cleanup().await.expect("clean up isolated test database");
}

#[tokio::test]
async fn list_sessions_sorts_by_last_user_message_timestamp() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "session list test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    create_project_session(store, project_id, "session_older_created").await;
    create_project_session(store, project_id, "session_newer_created").await;

    persist_turn(store, "session_older_created", 2_000, "newer user").await;
    persist_turn(store, "session_newer_created", 1_000, "older user").await;

    let sessions = store
        .list_sessions(Some(project_id), 10)
        .await
        .expect("sessions list");

    assert_eq!(
        sessions
            .iter()
            .map(|session| {
                (
                    session.session_id.as_str(),
                    session.last_user_message_timestamp_ms,
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("session_older_created", Some(2_000)),
            ("session_newer_created", Some(1_000)),
        ]
    );

    db.cleanup().await.expect("clean up isolated test database");
}
