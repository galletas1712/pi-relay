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

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn create_fork_copies_full_forest_and_replay_without_mutating_source() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(project_id, "fork copy test", "runtime-test", &[], json!({}))
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
    child_config.workspace_id = "/tmp/fork-child".to_string();
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
                source_entry_id: None,
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

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn history_targets_page_newest_users_with_safe_bounded_previews() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "history targets test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    create_session(store, project_id, "target-source", false).await;
    let huge_text = "x".repeat(50_000);
    let entries = vec![
        entry(
            "start-1",
            None,
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
        ),
        entry(
            "user-root",
            Some("start-1"),
            TranscriptItem::UserMessage(UserMessage::text("oldest")),
        ),
        entry(
            "finish-1",
            Some("user-root"),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ),
        entry(
            "start-2",
            Some("finish-1"),
            TranscriptItem::TurnStarted { turn_id: TurnId(2) },
        ),
        entry(
            "user-ordinary",
            Some("start-2"),
            TranscriptItem::UserMessage(UserMessage::text(&huge_text)),
        ),
        entry(
            "assistant-huge",
            Some("user-ordinary"),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("y".repeat(100_000))],
            }),
        ),
        entry(
            "finish-2",
            Some("assistant-huge"),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ),
        compaction_summary("compaction", "target-source", "finish-2"),
        entry(
            "start-3",
            Some("compaction"),
            TranscriptItem::TurnStarted { turn_id: TurnId(3) },
        ),
        entry(
            "user-after-compaction",
            Some("start-3"),
            TranscriptItem::UserMessage(UserMessage::text("newest")),
        ),
    ];
    store
        .persist_outputs(
            "target-source",
            OutputBatch::new(&entries, Some("user-after-compaction"), &[], &[]),
        )
        .await
        .expect("history persists");

    let newest = store
        .history_targets("target-source", None, Some(2))
        .await
        .expect("newest page loads");
    assert!(newest.has_more);
    assert_eq!(newest.targets.len(), 2);
    assert_eq!(
        newest
            .targets
            .iter()
            .map(|target| (
                target.entry_id.as_str(),
                target.target_leaf_id.as_deref(),
                target.preview.len(),
            ))
            .collect::<Vec<_>>(),
        vec![
            ("user-after-compaction", Some("compaction"), 6),
            ("user-ordinary", Some("finish-1"), 160),
        ]
    );
    assert!(newest
        .targets
        .iter()
        .all(|target| !target.preview.contains('y')));

    let older = store
        .history_targets("target-source", newest.next_before_sequence, Some(2))
        .await
        .expect("older page loads");
    assert!(!older.has_more);
    assert_eq!(
        older
            .targets
            .iter()
            .map(|target| (target.entry_id.as_str(), target.target_leaf_id.as_deref()))
            .collect::<Vec<_>>(),
        vec![("user-root", None)]
    );

    db.cleanup().await;
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn long_history_target_ancestry_remains_valid() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "long history target test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    let config = create_session(store, project_id, "long-source", false).await;
    sqlx::query(
        r#"
        insert into transcript_entries (
            session_id, id, parent_id, timestamp_ms, item, provider_replay, turn_id
        )
        select
            'long-source',
            'deep-' || depth,
            case when depth = 0 then null else 'deep-' || (depth - 1) end,
            depth,
            case
                when depth = 10001 then '{"type":"user_message","content":[{"type":"text","text":"long history"}]}'::jsonb
                else '{"type":"assistant_message","items":[]}'::jsonb
            end,
            '[]'::jsonb,
            null
        from generate_series(0, 10001) as ancestry(depth)
        "#,
    )
    .execute(&store.pool)
    .await
    .expect("deep ancestry inserts");
    sqlx::query("update sessions set active_leaf_id='deep-10001' where id='long-source'")
        .execute(&store.pool)
        .await
        .expect("long active leaf installs");

    let stored = store
        .load_stored_session("long-source")
        .await
        .expect("long active branch loads");
    assert_eq!(stored.entries.len(), 10_002);

    let page = store
        .history_targets("long-source", None, None)
        .await
        .expect("history targets load");
    assert_eq!(page.targets.len(), 1);
    assert_eq!(page.targets[0].entry_id, "deep-10001");
    assert_eq!(page.targets[0].target_leaf_id, None);

    let target = HistoryTarget {
        leaf_id: None,
        source_entry_id: Some("deep-10001"),
        expected_active_leaf_id: None,
        expected_transcript_revision: None,
        expected_active_branch_entry_ids: None,
    };
    switch(store, "long-source", target)
        .await
        .expect("long ancestry switches to root");
    fork(store, "long-source", "long-child", &config, target)
        .await
        .expect("long ancestry forks from root");

    db.cleanup().await;
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn cyclic_history_target_ancestry_is_rejected() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "cyclic history target test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    create_session(store, project_id, "cyclic-source", false).await;
    sqlx::query(
        r#"
        insert into transcript_entries (
            session_id, id, parent_id, timestamp_ms, item, provider_replay, turn_id
        )
        values
            ('cyclic-source', 'cycle-root', null, 1,
             '{"type":"assistant_message","items":[]}'::jsonb, '[]'::jsonb, null),
            ('cyclic-source', 'cycle-user', 'cycle-root', 2,
             '{"type":"user_message","content":[{"type":"text","text":"cycle"}]}'::jsonb, '[]'::jsonb, null)
        "#,
    )
    .execute(&store.pool)
    .await
    .expect("ancestry installs");
    sqlx::query(
        "update transcript_entries set parent_id='cycle-user' \
         where session_id='cyclic-source' and id='cycle-root'",
    )
    .execute(&store.pool)
    .await
    .expect("cycle installs");

    let error = store
        .history_targets("cyclic-source", None, None)
        .await
        .expect_err("cyclic ancestry is rejected");
    assert!(error.to_string().contains("cycle or non-append-only link"));

    db.cleanup().await;
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn switch_and_fork_share_history_target_validation() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "history target test",
            "runtime-test",
            &[],
            json!({}),
        )
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
        source_entry_id: None,
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
    store
        .persist_outputs(
            "boundary-source",
            OutputBatch::new(
                &[
                    entry(
                        "start-2",
                        Some("finish"),
                        TranscriptItem::TurnStarted { turn_id: TurnId(2) },
                    ),
                    entry(
                        "user-2",
                        Some("start-2"),
                        TranscriptItem::UserMessage(UserMessage::text("again")),
                    ),
                    entry(
                        "finish-2",
                        Some("user-2"),
                        TranscriptItem::TurnFinished {
                            turn_id: TurnId(2),
                            outcome: TurnOutcome::Graceful,
                        },
                    ),
                ],
                Some("finish-2"),
                &[],
                &[],
            ),
        )
        .await
        .expect("second turn persists");
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
        source_entry_id: Some("user-2"),
        expected_active_leaf_id: Some(Some("finish-2")),
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
                source_entry_id: None,
                expected_active_leaf_id: None,
                expected_transcript_revision: None,
                expected_active_branch_entry_ids: None,
            },
            "boundary",
        ),
        (
            "missing-boundary",
            HistoryTarget {
                leaf_id: Some("missing"),
                source_entry_id: None,
                expected_active_leaf_id: None,
                expected_transcript_revision: None,
                expected_active_branch_entry_ids: None,
            },
            "boundary",
        ),
        (
            "stale-source-entry",
            HistoryTarget {
                leaf_id: Some("finish"),
                source_entry_id: Some("user"),
                expected_active_leaf_id: None,
                expected_transcript_revision: None,
                expected_active_branch_entry_ids: None,
            },
            "history",
        ),
        (
            "stale-active",
            HistoryTarget {
                leaf_id: Some("finish"),
                source_entry_id: None,
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
                source_entry_id: None,
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
                source_entry_id: None,
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
                source_entry_id: None,
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
        source_entry_id: None,
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
        source_entry_id: None,
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
