use std::sync::atomic::{AtomicU64, Ordering};

use agent_session::TranscriptStorageNode;
use agent_vocab::{
    AssistantItem, AssistantMessage, CompactionSummary, ProviderConfig, ProviderKind,
    ProviderReplayItem, ReasoningEffort, TranscriptItem, TurnId, TurnOutcome, UserMessage,
};
use serde_json::json;
use uuid::Uuid;

use crate::{
    CreateContextForkRequest, CreateDelegationRequest, CreateForkRequest, DelegationKind,
    HistoryChanged, HistoryTarget, HistoryTargetNotTurnBoundary, OutputBatch, PostgresAgentStore,
    QueuedInputContent, SessionConfig, SourceMutationConflict, SubagentType,
    SwitchActiveLeafRequest, TranscriptEntryBodyMode,
};

static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(40_000);

struct TestDb {
    store: PostgresAgentStore,
    admin_url: String,
    name: String,
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn context_fork_copies_completed_branch_and_excludes_open_delegation_turn() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "runtime-test",
            "context fork test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    let parent_id = "context-fork-parent";
    let mut child_config = create_session(store, project_id, parent_id, false).await;
    let entries = vec![
        entry(
            "completed-start",
            None,
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
        ),
        entry(
            "completed-user",
            Some("completed-start"),
            TranscriptItem::UserMessage(UserMessage::text("remember this")),
        ),
        assistant_message_with_replay(
            "completed-assistant",
            Some("completed-user"),
            "completed answer",
        ),
        entry(
            "completed-finish",
            Some("completed-assistant"),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ),
        compaction_summary("completed-compaction", parent_id, "completed-finish"),
        entry(
            "open-start",
            Some("completed-compaction"),
            TranscriptItem::TurnStarted { turn_id: TurnId(2) },
        ),
        entry(
            "open-user",
            Some("open-start"),
            TranscriptItem::UserMessage(UserMessage::text("delegate now")),
        ),
        assistant_message_with_replay(
            "open-before-compaction",
            Some("open-user"),
            "working before compaction",
        ),
        entry(
            "mid-turn-compaction",
            None,
            TranscriptItem::CompactionSummary(
                CompactionSummary::new(
                    parent_id,
                    "open-before-compaction",
                    "open turn summary",
                    None,
                    TurnId(2),
                )
                .with_turn_started_at_ms(Some(1)),
            ),
        ),
        entry(
            "open-continuation",
            Some("mid-turn-compaction"),
            TranscriptItem::UserMessage(UserMessage::text("delegate now")),
        ),
        entry(
            "open-assistant",
            Some("open-continuation"),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(agent_vocab::ToolCall {
                    id: agent_vocab::ToolCallId::new("call_delegate"),
                    tool_name: "delegate_writing_task".to_string(),
                    args_json: "{}".to_string(),
                })],
            }),
        ),
    ];
    store
        .persist_outputs(
            parent_id,
            OutputBatch::new(&entries, Some("open-assistant"), &[], &[]),
        )
        .await
        .expect("completed and open turns persist");
    let parent_context = store
        .model_context_for_leaf(parent_id, "open-assistant")
        .await
        .expect("compacted open parent context loads");
    assert!(matches!(
        parent_context.transcript_items(),
        [
            TranscriptItem::CompactionSummary(summary),
            TranscriptItem::UserMessage(_),
            TranscriptItem::AssistantMessage(_),
        ] if summary.turn_started_at_ms == Some(1)
    ));
    let delegation = store
        .create_delegation_idempotent(CreateDelegationRequest {
            parent_session_id: parent_id,
            launch_key: "context-fork-launch",
            launch_shape: r#"{"kind":"full","role":"reviewer","prompt":"review"}"#,
            kind: DelegationKind::Full,
            workflow: None,
            label: None,
            expected_subagents: 1,
        })
        .await
        .expect("active delegation creates");
    child_config.workspace_id = "/tmp/context-fork-child".to_string();
    child_config.system_prompt = "child system prompt".to_string();
    child_config.metadata = json!({
        "subagent": true,
        "role_name": "reviewer",
        "delegation_spawn_index": 0,
    });
    child_config.provider.model = "child-model".to_string();
    let task = UserMessage::text("review inherited context");

    let result = store
        .create_context_fork(CreateContextForkRequest {
            child_session_id: "context-fork-child",
            config: &child_config,
            parent_session_id: parent_id,
            subagent_type: SubagentType::Full,
            delegation_id: Some(&delegation.id),
            task: &task,
        })
        .await
        .expect("context fork commits beside active delegation");

    assert_eq!(
        result.active_leaf_id.as_deref(),
        Some("completed-compaction")
    );
    let child = store
        .load_stored_session("context-fork-child")
        .await
        .expect("child transcript loads");
    assert_eq!(
        child
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "completed-start",
            "completed-user",
            "completed-assistant",
            "completed-finish",
            "completed-compaction",
        ]
    );
    assert_eq!(child.entries[2].provider_replay.len(), 1);
    assert!(matches!(
        child.entries.last().map(|entry| &entry.item),
        Some(TranscriptItem::CompactionSummary(_))
    ));
    let child_context = store
        .model_context_for_leaf("context-fork-child", "completed-compaction")
        .await
        .expect("child model context loads");
    assert!(matches!(
        child_context.transcript_items(),
        [TranscriptItem::CompactionSummary(summary)]
            if summary.turn_started_at_ms.is_none()
    ));
    let persisted_config = store
        .load_session_config("context-fork-child")
        .await
        .expect("child config loads");
    assert_eq!(persisted_config.workspace_id, child_config.workspace_id);
    assert_eq!(persisted_config.system_prompt, child_config.system_prompt);
    assert_eq!(
        serde_json::to_value(&persisted_config.provider).expect("provider serializes"),
        serde_json::to_value(&child_config.provider).expect("provider serializes")
    );
    assert_eq!(persisted_config.metadata, child_config.metadata);
    assert_eq!(
        store
            .session_parent_id("context-fork-child")
            .await
            .expect("parent link loads")
            .as_deref(),
        Some(parent_id)
    );
    assert_eq!(
        store
            .session_subagent_type("context-fork-child")
            .await
            .expect("subagent type loads"),
        Some(SubagentType::Full)
    );
    assert_eq!(
        store
            .session_delegation_id("context-fork-child")
            .await
            .expect("delegation link loads")
            .as_deref(),
        Some(delegation.id.as_str())
    );
    let queue = store
        .queue_state("context-fork-child")
        .await
        .expect("child queue loads");
    assert_eq!(queue.queued_inputs.len(), 1);
    assert_eq!(
        queue.queued_inputs[0].content,
        QueuedInputContent::user_message(task)
    );

    db.cleanup().await;
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn context_fork_without_completed_boundary_starts_empty_and_queues_task() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "runtime-test",
            "empty context fork test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    let parent_id = "context-empty-parent";
    let config = create_session(store, project_id, parent_id, false).await;
    store
        .persist_outputs(
            parent_id,
            OutputBatch::new(
                &[
                    entry(
                        "open-start",
                        None,
                        TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                    ),
                    entry(
                        "open-user",
                        Some("open-start"),
                        TranscriptItem::UserMessage(UserMessage::text("still open")),
                    ),
                ],
                Some("open-user"),
                &[],
                &[],
            ),
        )
        .await
        .expect("open turn persists");
    let task = UserMessage::text("start without inherited history");

    let result = store
        .create_context_fork(CreateContextForkRequest {
            child_session_id: "context-empty-child",
            config: &config,
            parent_session_id: parent_id,
            subagent_type: SubagentType::ReadOnly,
            delegation_id: None,
            task: &task,
        })
        .await
        .expect("empty context fork commits");

    assert_eq!(result.active_leaf_id, None);
    let child = store
        .load_stored_session("context-empty-child")
        .await
        .expect("child transcript loads");
    assert_eq!(child.active_leaf_id, None);
    assert!(child.entries.is_empty());
    let queue = store
        .queue_state("context-empty-child")
        .await
        .expect("child queue loads");
    assert_eq!(queue.queued_inputs.len(), 1);
    assert_eq!(
        queue.queued_inputs[0].content,
        QueuedInputContent::user_message(task)
    );

    db.cleanup().await;
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
    // The source's current active leaf is what the child must duplicate.
    store
        .switch_active_leaf(SwitchActiveLeafRequest {
            session_id: source_session_id,
            target: HistoryTarget {
                leaf_id: Some("compaction"),
                source_entry_id: None,
                expected_active_leaf_id: None,
                expected_transcript_revision: None,
                expected_active_branch_entry_ids: None,
            },
            return_active_branch: false,
            missing_body_ids: None,
        })
        .await
        .expect("source switches to its current leaf");
    let source_before = store
        .load_stored_session(source_session_id)
        .await
        .expect("source loads");
    child_config.workspace_id = "/tmp/fork-child".to_string();
    child_config.metadata = json!({ "fork": { "source_session_id": source_session_id } });

    let result = store
        .create_fork(CreateForkRequest {
            source_session_id,
            child_session_id: "fork-child",
            config: &child_config,
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
    assert_eq!(child.active_leaf_id, source_before.active_leaf_id);
    assert_eq!(child.entries, source_before.entries);
    assert_eq!(result.active_leaf_id, child.active_leaf_id);
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
) -> anyhow::Result<()> {
    store
        .create_fork(CreateForkRequest {
            source_session_id,
            child_session_id,
            config,
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
    create_session(store, project_id, "long-source", false).await;
    sqlx::query(
        r#"
        insert into transcript_entries (
            session_id, id, parent_id, timestamp_ms, item, provider_replay, turn_id
        )
        select
            'long-source',
            'deep-' || depth,
            case when depth = 10001 then null else 'deep-' || (depth + 1) end,
            depth,
            case
                when depth = 0 then '{"type":"user_message","content":[{"type":"text","text":"long history"}]}'::jsonb
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
    sqlx::query("update sessions set active_leaf_id='deep-0' where id='long-source'")
        .execute(&store.pool)
        .await
        .expect("long active leaf installs");

    let active_branch = store
        .active_branch("long-source")
        .await
        .expect("long active branch loads");
    assert_eq!(active_branch.entries.len(), 10_002);
    assert_eq!(active_branch.entries.first().unwrap().id, "deep-10001");
    assert_eq!(active_branch.entries.last().unwrap().id, "deep-0");

    let page = store
        .history_targets("long-source", None, None)
        .await
        .expect("history targets load");
    assert_eq!(page.targets.len(), 1);
    assert_eq!(page.targets[0].entry_id, "deep-0");
    assert_eq!(page.targets[0].target_leaf_id, None);
    let synced = store
        .sync_active_branch(
            "long-source",
            Some("deep-10001"),
            TranscriptEntryBodyMode::Ui,
        )
        .await
        .expect("long active branch syncs");
    assert_eq!(synced.entries.len(), 10_001);
    assert_eq!(synced.entries.first().unwrap().id, "deep-10000");
    assert_eq!(synced.entries.last().unwrap().id, "deep-0");
    store
        .transcript_turns("long-source", None, Some(1))
        .await
        .expect("long turn-card ancestry loads");
    assert!(store
        .latest_model_token_usage_estimate("long-source", "deep-0", "missing-toolset")
        .await
        .expect("long token-usage ancestry loads")
        .is_none());

    let target = HistoryTarget {
        leaf_id: None,
        source_entry_id: Some("deep-0"),
        expected_active_leaf_id: None,
        expected_transcript_revision: None,
        expected_active_branch_entry_ids: None,
    };
    switch(store, "long-source", target)
        .await
        .expect("long ancestry switches to root");

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
             '{"type":"turn_started","turn_id":1}'::jsonb, '[]'::jsonb, 1),
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
    sqlx::query("update sessions set active_leaf_id='cycle-user' where id='cyclic-source'")
        .execute(&store.pool)
        .await
        .expect("cyclic active leaf installs");

    let error = store
        .history_targets("cyclic-source", None, None)
        .await
        .expect_err("cyclic ancestry is rejected");
    assert!(error
        .to_string()
        .contains("transcript ancestry contains a cycle"));
    for error in [
        store
            .active_branch("cyclic-source")
            .await
            .expect_err("cyclic active branch is rejected"),
        store
            .transcript_turns("cyclic-source", None, Some(2))
            .await
            .expect_err("cyclic turn-card ancestry is rejected"),
        store
            .latest_model_token_usage_estimate("cyclic-source", "cycle-user", "missing-toolset")
            .await
            .expect_err("cyclic token-usage ancestry is rejected"),
        store
            .sync_active_branch(
                "cyclic-source",
                Some("missing-base"),
                TranscriptEntryBodyMode::Ui,
            )
            .await
            .expect_err("cyclic branch synchronization is rejected"),
    ] {
        assert!(error
            .to_string()
            .contains("transcript ancestry contains a cycle"));
    }

    db.cleanup().await;
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn switch_validates_history_targets_and_both_operations_require_an_idle_source() {
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

    create_session(store, project_id, "root-source", false).await;
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
    switch(store, "root-source", root_target)
        .await
        .expect("root switch succeeds");

    create_session(store, project_id, "boundary-source", true).await;
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
        match expected_kind {
            "active" => {
                assert!(switch_error
                    .downcast_ref::<crate::ExpectedActiveLeafMismatch>()
                    .is_some());
            }
            "boundary" => {
                assert!(switch_error
                    .downcast_ref::<HistoryTargetNotTurnBoundary>()
                    .is_some());
            }
            "history" => {
                assert!(switch_error.downcast_ref::<HistoryChanged>().is_some());
            }
            other => panic!("unexpected expected kind: {other} for {label}"),
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
    let fork_error = fork(store, "busy-source", "busy-child", &busy_config)
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
        .create_delegation_idempotent(crate::CreateDelegationRequest {
            parent_session_id: "delegation-source",
            launch_key: "test:blocks-source-mutation",
            launch_shape: r#"{"kind":"full","role":"implementer","prompt":"work"}"#,
            kind: DelegationKind::Full,
            workflow: None,
            label: None,
            expected_subagents: 1,
        })
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
    )
    .await
    .expect_err("running delegation blocks fork");
    assert!(switch_error
        .downcast_ref::<SourceMutationConflict>()
        .is_some());
    assert!(fork_error
        .downcast_ref::<SourceMutationConflict>()
        .is_some());

    db.cleanup().await;
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn create_fork_rejects_a_source_whose_active_leaf_is_mid_turn() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "fork boundary test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    let config = create_session(store, project_id, "mid-turn-source", false).await;
    store
        .persist_outputs(
            "mid-turn-source",
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
                ],
                Some("user"),
                &[],
                &[],
            ),
        )
        .await
        .expect("open turn persists");

    let error = fork(store, "mid-turn-source", "mid-turn-child", &config)
        .await
        .expect_err("a mid-turn source cannot be duplicated");

    assert!(error
        .downcast_ref::<HistoryTargetNotTurnBoundary>()
        .is_some());
    assert!(!store
        .session_exists("mid-turn-child")
        .await
        .expect("child existence reads"));

    db.cleanup().await;
}
