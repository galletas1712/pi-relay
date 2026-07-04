use std::sync::atomic::{AtomicU64, Ordering};

use agent_session::TranscriptStorageNode;
use agent_vocab::{
    AssistantItem, AssistantMessage, ProviderConfig, ProviderKind, ProviderReplayItem,
    ReasoningEffort, TranscriptItem, TurnId, TurnOutcome, UserMessage,
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
        .create_project(project_id, "manual compaction failure", &[], json!({}))
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

    db.cleanup().await;
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
        .create_project(project_id, "compaction replay test", &[], json!({}))
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
                    summary_kind: "provider_native".to_string(),
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

    db.cleanup().await;
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
        .create_project(project_id, "subagent type test", &[], json!({}))
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

    db.cleanup().await;
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
        .create_project(project_id, "delete guard test", &[], json!({}))
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

    db.cleanup().await;
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
