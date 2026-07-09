use std::sync::atomic::{AtomicU64, Ordering};

use agent_mcp::{McpSessionManifest, McpSessionSnapshot};
use agent_vocab::{ProviderConfig, ProviderKind, ReasoningEffort};
use pretty_assertions::assert_eq;
use serde_json::json;

use crate::{
    InputPriority, McpSessionManifestBinding, PostgresAgentStore, SessionConfig, SubagentType,
    UserMessage,
};

static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(1);

struct TestDb {
    store: PostgresAgentStore,
    admin_url: String,
    database_url: String,
    name: String,
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

impl TestDb {
    async fn cleanup(self) {
        self.store.close().await;
        let admin = sqlx::PgPool::connect(&self.admin_url)
            .await
            .expect("connect admin database for cleanup");
        sqlx::query(&format!(
            "drop database if exists \"{}\" with (force)",
            self.name
        ))
        .execute(&admin)
        .await
        .expect("drop MCP test database");
        admin.close().await;
    }
}

async fn test_store() -> Option<TestDb> {
    let admin_url = std::env::var("PI_RELAY_TEST_DATABASE_URL").ok()?;
    let name = format!(
        "pi_relay_mcp_test_{}_{}",
        std::process::id(),
        TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let admin = sqlx::PgPool::connect(&admin_url)
        .await
        .expect("connect admin database");
    sqlx::query(&format!("create database \"{name}\""))
        .execute(&admin)
        .await
        .expect("create MCP test database");
    admin.close().await;
    let database_url = database_url_with_name(&admin_url, &name);
    let store = PostgresAgentStore::connect(&database_url)
        .await
        .expect("connect MCP test database");
    store.migrate().await.expect("migrate MCP test database");
    Some(TestDb {
        store,
        admin_url,
        database_url,
        name,
    })
}

fn empty_binding() -> McpSessionManifestBinding {
    let snapshot = McpSessionSnapshot::empty();
    McpSessionManifestBinding {
        manifest_fingerprint: snapshot.manifest_fingerprint().to_string(),
        manifest: serde_json::to_value(snapshot.manifest()).expect("manifest serializes"),
    }
}

fn config(binding: Option<McpSessionManifestBinding>) -> SessionConfig {
    SessionConfig {
        project_id: None,
        outer_cwd: "/tmp".to_string(),
        workspaces: Vec::new(),
        system_prompt: "prompt".to_string(),
        provider: ProviderConfig {
            kind: ProviderKind::OpenAi,
            model: "test-model".to_string(),
            reasoning_effort: ReasoningEffort::Medium,
            max_tokens: None,
            prompt_cache: None,
        },
        metadata: json!({ "created_by": "test" }),
        mcp_manifest: binding,
    }
}

async fn create_session(
    store: &PostgresAgentStore,
    session_id: &str,
    config: &SessionConfig,
    parent_session_id: Option<&str>,
    subagent_type: Option<SubagentType>,
) {
    store
        .start_session_outputs_with_parent(
            session_id,
            config,
            &[],
            None,
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("hello"),
            None,
            parent_session_id,
            subagent_type,
            None,
        )
        .await
        .expect("session creates");
}

#[tokio::test]
async fn session_manifest_persists_atomically_reloads_and_children_reuse_it() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let binding = empty_binding();
    let parent_config = config(Some(binding.clone()));
    create_session(&db.store, "parent", &parent_config, None, None).await;
    let restarted = PostgresAgentStore::connect(&db.database_url)
        .await
        .expect("restart reconnects");
    restarted.migrate().await.expect("restart migrates");
    let loaded = restarted
        .load_session_config("parent")
        .await
        .expect("parent reloads");
    assert_eq!(loaded.mcp_manifest, Some(binding.clone()));
    let manifest: McpSessionManifest =
        serde_json::from_value(loaded.mcp_manifest.expect("binding exists").manifest)
            .expect("manifest parses");
    McpSessionSnapshot::from_persisted(manifest).expect("manifest validates");
    let retained_columns: Vec<(String, String)> = sqlx::query_as(
        r#"
        select table_name, column_name
        from information_schema.columns
        where table_schema='public'
          and (
            (table_name in ('actions', 'queued_inputs') and column_name='provider_config')
            or (table_name='sessions' and column_name='mcp_manifest_fingerprint')
          )
        order by table_name, column_name
        "#,
    )
    .fetch_all(&restarted.pool)
    .await
    .expect("retained route and MCP columns load");
    assert_eq!(
        retained_columns,
        vec![
            ("actions".to_string(), "provider_config".to_string()),
            ("queued_inputs".to_string(), "provider_config".to_string()),
            (
                "sessions".to_string(),
                "mcp_manifest_fingerprint".to_string()
            ),
        ]
    );
    let retained_indexes: Vec<String> = sqlx::query_scalar(
        r#"
        select indexname
        from pg_indexes
        where schemaname='public'
          and indexname in (
            'queued_inputs_non_cancelled_session_idx',
            'queued_inputs_follow_up_order_idx',
            'sessions_mcp_manifest_idx'
          )
        order by indexname
        "#,
    )
    .fetch_all(&restarted.pool)
    .await
    .expect("retained hot-path and MCP indexes load");
    assert_eq!(
        retained_indexes,
        vec![
            "queued_inputs_follow_up_order_idx".to_string(),
            "queued_inputs_non_cancelled_session_idx".to_string(),
            "sessions_mcp_manifest_idx".to_string(),
        ]
    );
    restarted.close().await;

    for (child, subagent_type) in [
        ("full-child", SubagentType::Full),
        ("read-only-child", SubagentType::ReadOnly),
    ] {
        create_session(
            &db.store,
            child,
            &config(Some(binding.clone())),
            Some("parent"),
            Some(subagent_type),
        )
        .await;
        assert_eq!(
            db.store
                .load_session_config(child)
                .await
                .expect("child reloads")
                .mcp_manifest,
            Some(binding.clone())
        );
    }
    let count: i64 = sqlx::query_scalar("select count(*) from mcp_session_manifests")
        .fetch_one(&db.store.pool)
        .await
        .expect("count manifests");
    assert_eq!(count, 1);
    db.cleanup().await;
}

#[tokio::test]
async fn child_manifest_mismatch_rolls_back_session_and_manifest_install() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    create_session(&db.store, "mcp-free-parent", &config(None), None, None).await;
    let error = db
        .store
        .start_session_outputs_with_parent(
            "mismatched-child",
            &config(Some(empty_binding())),
            &[],
            None,
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("hello"),
            None,
            Some("mcp-free-parent"),
            Some(SubagentType::Full),
            None,
        )
        .await
        .expect_err("mismatched child is rejected");
    assert!(error
        .to_string()
        .contains("child MCP manifest must exactly match parent"));
    assert!(!db
        .store
        .session_exists("mismatched-child")
        .await
        .expect("child absence loads"));
    let manifests: i64 = sqlx::query_scalar("select count(*) from mcp_session_manifests")
        .fetch_one(&db.store.pool)
        .await
        .expect("manifest count loads");
    assert_eq!(manifests, 0);
    db.cleanup().await;
}

#[tokio::test]
async fn legacy_null_is_explicitly_mcp_free_and_session_delete_releases_reference() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    create_session(&db.store, "legacy", &config(None), None, None).await;
    assert_eq!(
        db.store
            .load_session_config("legacy")
            .await
            .expect("legacy session reloads")
            .mcp_manifest,
        None
    );

    let binding = empty_binding();
    create_session(
        &db.store,
        "selected",
        &config(Some(binding.clone())),
        None,
        None,
    )
    .await;
    db.store
        .delete_session("selected")
        .await
        .expect("session deletes");
    let references: i64 =
        sqlx::query_scalar("select count(*) from sessions where mcp_manifest_fingerprint=$1::text")
            .bind(&binding.manifest_fingerprint)
            .fetch_one(&db.store.pool)
            .await
            .expect("count references");
    assert_eq!(references, 0);
    db.cleanup().await;
}

#[tokio::test]
async fn content_address_collision_rolls_back_session_creation() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let binding = empty_binding();
    create_session(
        &db.store,
        "first",
        &config(Some(binding.clone())),
        None,
        None,
    )
    .await;
    let mut conflicting = binding;
    conflicting.manifest["inventory_revision"] = json!("corrupt");
    let error = db
        .store
        .start_session_outputs_with_parent(
            "conflict",
            &config(Some(conflicting)),
            &[],
            None,
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("hello"),
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("collision is rejected");
    assert!(error.to_string().contains("collision"));
    assert!(!db
        .store
        .session_exists("conflict")
        .await
        .expect("query session"));
    db.cleanup().await;
}
