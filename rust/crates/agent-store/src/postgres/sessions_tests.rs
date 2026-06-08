use std::sync::atomic::{AtomicU64, Ordering};

use agent_session::TranscriptStorageNode;
use agent_vocab::{
    AssistantItem, AssistantMessage, ProviderConfig, ProviderKind, ReasoningEffort, TranscriptItem,
    TurnId, TurnOutcome, UserMessage,
};
use serde_json::json;
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
        "pi_relay_sessions_test_{}_{}",
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
        system_prompt: "test prompt".to_string(),
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
async fn list_sessions_sorts_by_last_user_message_timestamp() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(project_id, "session list test", &[], json!({}))
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

    db.cleanup().await;
}
