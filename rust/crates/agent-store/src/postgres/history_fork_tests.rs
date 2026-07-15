use std::sync::atomic::{AtomicU64, Ordering};

use agent_session::TranscriptStorageNode;
use agent_vocab::{
    AssistantItem, AssistantMessage, CompactionSummary, ProviderConfig, ProviderKind,
    ProviderReplayItem, ReasoningEffort, TranscriptItem, TurnId, TurnOutcome, UserMessage,
};
use serde_json::json;
use uuid::Uuid;

use crate::{
    CreateForkRequest, DelegationKind, HistoryChanged, HistoryTarget, HistoryTargetNotTurnBoundary,
    OutputBatch, PostgresAgentStore, RunningDelegationConflict, SessionConfig,
    SourceMutationConflict, SwitchActiveLeafRequest,
};

static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(40_000);

struct TestDb {
    store: PostgresAgentStore,
    admin_url: String,
    name: String,
}

#[tokio::test]
async fn create_fork_copies_full_forest_and_replay_without_mutating_source() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(project_id, "fork copy test", &[], json!({}))
        .await
        .expect("project creates");
    let source_session_id = "fork-source";
    let mut child_config = create_session(store, project_id, source_session_id, false).await;
    let entries = vec![
        entry(
            "start",
            None,
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
        ),
        entry(
            "user",
            Some("start"),
            TranscriptItem::UserMessage(UserMessage::text("hello")),
        ),
        assistant_message_with_replay("assistant", Some("user"), "answer"),
        entry(
            "first-finish",
            Some("assistant"),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ),
        entry(
            "sibling-start",
            Some("first-finish"),
            TranscriptItem::TurnStarted { turn_id: TurnId(2) },
        ),
        entry(
            "sibling-user",
            Some("sibling-start"),
            TranscriptItem::UserMessage(UserMessage::text("alternate")),
        ),
        entry(
            "sibling-finish",
            Some("sibling-user"),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ),
        compaction_summary("compaction", source_session_id, "first-finish"),
    ];
    store
        .persist_outputs(
            source_session_id,
            OutputBatch::new(&entries, Some("sibling-finish"), &[], &[]),
        )
        .await
        .expect("source forest persists");
    let source_before = store
        .load_stored_session(source_session_id)
        .await
        .expect("source loads");
    let revision = store
        .session_snapshot(source_session_id)
        .await
        .expect("source snapshot loads")
        .transcript_revision;
    child_config.outer_cwd = "/tmp/fork-child".to_string();
    child_config.metadata = json!({
        "fork": {
            "source_session_id": source_session_id,
            "source_leaf_id": "compaction",
        }
    });
    let target_branch_ids = vec![
        "start".to_string(),
        "user".to_string(),
        "assistant".to_string(),
        "first-finish".to_string(),
        "compaction".to_string(),
    ];

    let result = store
        .create_fork(CreateForkRequest {
            source_session_id,
            child_session_id: "fork-child",
            config: &child_config,
            target: HistoryTarget {
                leaf_id: Some("compaction"),
                expected_active_leaf_id: Some(Some("sibling-finish")),
                expected_transcript_revision: Some(revision),
                expected_active_branch_entry_ids: Some(&target_branch_ids),
            },
        })
        .await
        .expect("fork creates");

    let source_after = store
        .load_stored_session(source_session_id)
        .await
        .expect("source reloads");
    let child = store
        .load_stored_session("fork-child")
        .await
        .expect("child loads");
    assert_eq!(source_after, source_before);
    assert_eq!(child.active_leaf_id.as_deref(), Some("compaction"));
    assert_eq!(child.entries, source_before.entries);
    assert_eq!(result.active_leaf_id, child.active_leaf_id);
    assert_eq!(result.source_leaf_id, child.active_leaf_id);
    assert_eq!(
        result.events[0].data["provider"],
        serde_json::to_value(&child_config.provider).expect("provider serializes")
    );
    assert_eq!(
        child.entries[2].provider_replay,
        source_before.entries[2].provider_replay
    );
    assert!(child
        .entries
        .iter()
        .any(|entry| entry.id == "sibling-finish"));

    db.cleanup().await;
}

fn assistant_message_with_replay(
    id: &str,
    parent_id: Option<&str>,
    text: &str,
) -> TranscriptStorageNode {
    TranscriptStorageNode {
        id: id.to_string(),
        parent_id: parent_id.map(str::to_string),
        timestamp_ms: 1,
        item: TranscriptItem::AssistantMessage(AssistantMessage {
            items: vec![AssistantItem::Text(text.to_string())],
        }),
        provider_replay: vec![ProviderReplayItem::new(
            ProviderKind::OpenAi,
            &json!({ "type": "message", "large": "raw" }),
        )
        .expect("provider replay serializes")],
    }
}

fn compaction_summary(id: &str, session_id: &str, source_leaf_id: &str) -> TranscriptStorageNode {
    entry(
        id,
        None,
        TranscriptItem::CompactionSummary(CompactionSummary::new(
            session_id,
            source_leaf_id,
            "summary",
            None,
            TurnId(0),
        )),
    )
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
        "pi_relay_history_fork_test_{}_{}",
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
        mcp_manifest: None,
    }
}

async fn create_session(
    store: &PostgresAgentStore,
    project_id: Uuid,
    session_id: &str,
    with_history: bool,
) -> SessionConfig {
    let config = session_config(project_id);
    store
        .create_session(session_id, &config)
        .await
        .expect("session creates");
    if with_history {
        store
            .persist_outputs(
                session_id,
                OutputBatch::new(
                    &[
                        entry(
                            "start",
                            None,
                            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                        ),
                        entry(
                            "user",
                            Some("start"),
                            TranscriptItem::UserMessage(UserMessage::text("hello")),
                        ),
                        entry(
                            "finish",
                            Some("user"),
                            TranscriptItem::TurnFinished {
                                turn_id: TurnId(1),
                                outcome: TurnOutcome::Graceful,
                            },
                        ),
                    ],
                    Some("finish"),
                    &[],
                    &[],
                ),
            )
            .await
            .expect("history persists");
    }
    config
}

fn entry(id: &str, parent_id: Option<&str>, item: TranscriptItem) -> TranscriptStorageNode {
    TranscriptStorageNode {
        id: id.to_string(),
        parent_id: parent_id.map(str::to_string),
        timestamp_ms: 1,
        item,
        provider_replay: Vec::new(),
    }
}

async fn switch(
    store: &PostgresAgentStore,
    session_id: &str,
    target: HistoryTarget<'_>,
) -> anyhow::Result<()> {
    store
        .switch_active_leaf(SwitchActiveLeafRequest {
            session_id,
            target,
            return_active_branch: false,
            missing_body_ids: None,
        })
        .await
        .map(|_| ())
}

async fn fork(
    store: &PostgresAgentStore,
    source_session_id: &str,
    child_session_id: &str,
    config: &SessionConfig,
    target: HistoryTarget<'_>,
) -> anyhow::Result<()> {
    store
        .create_fork(CreateForkRequest {
            source_session_id,
            child_session_id,
            config,
            target,
        })
        .await
        .map(|_| ())
}

#[tokio::test]
async fn switch_and_fork_share_history_target_validation() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(project_id, "history target test", &[], json!({}))
        .await
        .expect("project creates");

    let root_config = create_session(store, project_id, "root-source", false).await;
    let root_revision = store
        .session_snapshot("root-source")
        .await
        .expect("root snapshot loads")
        .transcript_revision;
    let root_target = HistoryTarget {
        leaf_id: None,
        expected_active_leaf_id: Some(None),
        expected_transcript_revision: Some(root_revision),
        expected_active_branch_entry_ids: Some(&[]),
    };
    fork(
        store,
        "root-source",
        "root-child",
        &root_config,
        root_target,
    )
    .await
    .expect("root fork succeeds");
    switch(store, "root-source", root_target)
        .await
        .expect("root switch succeeds");

    let boundary_config = create_session(store, project_id, "boundary-source", true).await;
    let snapshot = store
        .session_snapshot("boundary-source")
        .await
        .expect("snapshot loads");
    let branch_ids = vec![
        "start".to_string(),
        "user".to_string(),
        "finish".to_string(),
    ];
    let boundary_target = HistoryTarget {
        leaf_id: Some("finish"),
        expected_active_leaf_id: Some(Some("finish")),
        expected_transcript_revision: Some(snapshot.transcript_revision),
        expected_active_branch_entry_ids: Some(&branch_ids),
    };
    fork(
        store,
        "boundary-source",
        "boundary-child",
        &boundary_config,
        boundary_target,
    )
    .await
    .expect("boundary fork succeeds");
    switch(store, "boundary-source", boundary_target)
        .await
        .expect("boundary switch succeeds");

    for (label, target, expected_kind) in [
        (
            "mid-turn",
            HistoryTarget {
                leaf_id: Some("user"),
                expected_active_leaf_id: None,
                expected_transcript_revision: None,
                expected_active_branch_entry_ids: None,
            },
            "boundary",
        ),
        (
            "stale-active",
            HistoryTarget {
                leaf_id: Some("finish"),
                expected_active_leaf_id: Some(None),
                expected_transcript_revision: None,
                expected_active_branch_entry_ids: None,
            },
            "active",
        ),
        (
            "stale-revision",
            HistoryTarget {
                leaf_id: Some("finish"),
                expected_active_leaf_id: None,
                expected_transcript_revision: Some(snapshot.transcript_revision + 1),
                expected_active_branch_entry_ids: None,
            },
            "history",
        ),
        (
            "stale-branch",
            HistoryTarget {
                leaf_id: Some("finish"),
                expected_active_leaf_id: None,
                expected_transcript_revision: None,
                expected_active_branch_entry_ids: Some(&["start".to_string()]),
            },
            "history",
        ),
        (
            "explicit-empty-branch",
            HistoryTarget {
                leaf_id: Some("finish"),
                expected_active_leaf_id: None,
                expected_transcript_revision: None,
                expected_active_branch_entry_ids: Some(&[]),
            },
            "history",
        ),
    ] {
        let switch_error = switch(store, "boundary-source", target)
            .await
            .expect_err("switch rejects invalid target");
        let fork_error = fork(
            store,
            "boundary-source",
            &format!("{label}-child"),
            &boundary_config,
            target,
        )
        .await
        .expect_err("fork rejects invalid target");
        match expected_kind {
            "active" => {
                assert!(switch_error
                    .downcast_ref::<crate::ExpectedActiveLeafMismatch>()
                    .is_some());
                assert!(fork_error
                    .downcast_ref::<crate::ExpectedActiveLeafMismatch>()
                    .is_some());
            }
            "boundary" => {
                assert!(switch_error
                    .downcast_ref::<HistoryTargetNotTurnBoundary>()
                    .is_some());
                assert!(fork_error
                    .downcast_ref::<HistoryTargetNotTurnBoundary>()
                    .is_some());
            }
            "history" => {
                assert!(switch_error.downcast_ref::<HistoryChanged>().is_some());
                assert!(fork_error.downcast_ref::<HistoryChanged>().is_some());
            }
            other => panic!("unexpected expected kind: {other}"),
        }
    }

    let busy_config = create_session(store, project_id, "busy-source", true).await;
    store
        .enqueue_user_input(
            "busy-source",
            crate::InputPriority::FollowUp,
            &UserMessage::text("queued"),
            Some("busy-input"),
            Some(Some("finish")),
        )
        .await
        .expect("input queues");
    let busy_target = HistoryTarget {
        leaf_id: Some("finish"),
        expected_active_leaf_id: None,
        expected_transcript_revision: None,
        expected_active_branch_entry_ids: None,
    };
    let switch_error = switch(store, "busy-source", busy_target)
        .await
        .expect_err("active work blocks switch");
    let fork_error = fork(
        store,
        "busy-source",
        "busy-child",
        &busy_config,
        busy_target,
    )
    .await
    .expect_err("active work blocks fork");
    assert!(switch_error
        .downcast_ref::<SourceMutationConflict>()
        .is_some());
    assert!(fork_error
        .downcast_ref::<SourceMutationConflict>()
        .is_some());

    let delegation_config = create_session(store, project_id, "delegation-source", false).await;
    store
        .create_delegation("delegation-source", DelegationKind::Full, None, None, 1)
        .await
        .expect("running delegation creates");
    let delegation_target = HistoryTarget {
        leaf_id: None,
        expected_active_leaf_id: None,
        expected_transcript_revision: None,
        expected_active_branch_entry_ids: None,
    };
    let switch_error = switch(store, "delegation-source", delegation_target)
        .await
        .expect_err("running delegation blocks switch");
    let fork_error = fork(
        store,
        "delegation-source",
        "delegation-child",
        &delegation_config,
        delegation_target,
    )
    .await
    .expect_err("running delegation blocks fork");
    assert!(switch_error
        .downcast_ref::<RunningDelegationConflict>()
        .is_some());
    assert!(fork_error
        .downcast_ref::<RunningDelegationConflict>()
        .is_some());

    db.cleanup().await;
}
