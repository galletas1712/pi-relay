use std::sync::atomic::{AtomicU64, Ordering};

use agent_session::TranscriptStorageNode;
use agent_vocab::{
    AssistantMessage, DaemonToolObservation, ProviderConfig, ProviderKind, ReasoningEffort,
    ToolCallId, TranscriptItem, TurnId, TurnOutcome, UserMessage,
};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    DelegationKind, DelegationStatus, InputPriority, OutputBatch, QueuedInputStatus, SessionConfig,
    SubagentType,
};

use super::*;

static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(70_000);

struct TestDb {
    store: PostgresAgentStore,
    admin_url: String,
    name: String,
}

async fn create_delegation_subagent_with_task_and_leaf(
    db: &TestDb,
    session_id: &str,
    project_id: Uuid,
    parent_session_id: &str,
    subagent_type: SubagentType,
    role_name: &str,
    task: Option<&str>,
    delegation_id: &str,
    active_leaf: Option<TranscriptItem>,
) {
    let leaf = active_leaf.as_ref().map(|_| format!("{session_id}_leaf"));
    let entries = active_leaf
        .map(|item| {
            vec![TranscriptStorageNode {
                id: leaf.clone().expect("leaf id"),
                parent_id: None,
                timestamp_ms: 1,
                item,
                provider_replay: Vec::new(),
            }]
        })
        .unwrap_or_default();
    db.store
        .start_session_outputs_with_parent(
            session_id,
            &session_config_with_task(project_id, Some(role_name), task),
            &entries,
            leaf.as_deref(),
            &[],
            &[],
            crate::InputPriority::FollowUp,
            &UserMessage::text("go"),
            None,
            Some(parent_session_id),
            Some(subagent_type),
            Some(delegation_id),
        )
        .await
        .expect("create delegation subagent");
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
        "pi_relay_delegations_test_{}_{}",
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

fn session_config(project_id: Uuid, role_name: Option<&str>) -> SessionConfig {
    session_config_with_task(project_id, role_name, None)
}

fn session_config_with_task(
    project_id: Uuid,
    role_name: Option<&str>,
    task: Option<&str>,
) -> SessionConfig {
    let mut metadata = json!({ "created_by": "test" });
    if let Some(role_name) = role_name {
        metadata["role_name"] = json!(role_name);
    }
    if let Some(task) = task {
        metadata["task"] = json!(task);
    }
    SessionConfig {
        project_id: Some(project_id),
        outer_cwd: "/tmp/pi-relay-test".to_string(),
        workspaces: Vec::new(),
        system_prompt: String::new(),
        provider: ProviderConfig {
            kind: ProviderKind::OpenAi,
            model: "gpt-5".to_string(),
            reasoning_effort: ReasoningEffort::Medium,
            max_tokens: None,
            prompt_cache: None,
        },
        metadata,
    }
}

async fn create_session(db: &TestDb, session_id: &str, project_id: Uuid) {
    db.store
        .start_session_outputs(
            session_id,
            &session_config(project_id, None),
            &[],
            None,
            &[],
            &[],
            crate::InputPriority::FollowUp,
            &UserMessage::text("go"),
            None,
        )
        .await
        .expect("create session");
}

async fn create_delegation_subagent(
    db: &TestDb,
    session_id: &str,
    project_id: Uuid,
    parent_session_id: &str,
    subagent_type: SubagentType,
    role_name: &str,
    delegation_id: &str,
) {
    db.store
        .start_session_outputs_with_parent(
            session_id,
            &session_config(project_id, Some(role_name)),
            &[],
            None,
            &[],
            &[],
            crate::InputPriority::FollowUp,
            &UserMessage::text("go"),
            None,
            Some(parent_session_id),
            Some(subagent_type),
            Some(delegation_id),
        )
        .await
        .expect("create delegation subagent");
}

async fn create_delegation_subagent_with_leaf(
    db: &TestDb,
    session_id: &str,
    project_id: Uuid,
    parent_session_id: &str,
    subagent_type: SubagentType,
    role_name: &str,
    delegation_id: &str,
    active_leaf: TranscriptItem,
) {
    let leaf = format!("{session_id}_leaf");
    db.store
        .start_session_outputs_with_parent(
            session_id,
            &session_config(project_id, Some(role_name)),
            &[TranscriptStorageNode {
                id: leaf.clone(),
                parent_id: None,
                timestamp_ms: 1,
                item: active_leaf,
                provider_replay: Vec::new(),
            }],
            Some(&leaf),
            &[],
            &[],
            crate::InputPriority::FollowUp,
            &UserMessage::text("go"),
            None,
            Some(parent_session_id),
            Some(subagent_type),
            Some(delegation_id),
        )
        .await
        .expect("create delegation subagent");
}

#[tokio::test]
async fn create_delegation_persists_kind_status_and_attempt() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;

    let delegation = db
        .store
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            Some("implement_review_test"),
            Some("review fan-out"),
            3,
        )
        .await
        .expect("create delegation");
    assert_eq!(delegation.expected_subagents, 3);
    assert_eq!(delegation.kind, DelegationKind::ReadonlyFanout);
    assert_eq!(delegation.status, DelegationStatus::Running);
    assert!(!delegation.attempt_id.is_empty());

    let loaded = db
        .store
        .get_delegation(&delegation.id)
        .await
        .expect("get delegation")
        .expect("delegation exists");
    assert_eq!(loaded.parent_session_id, "parent");
    assert_eq!(loaded.workflow.as_deref(), Some("implement_review_test"));
    assert_eq!(loaded.label.as_deref(), Some("review fan-out"));
    assert_eq!(loaded.status, DelegationStatus::Running);

    db.cleanup().await;
}

#[tokio::test]
async fn migration_creates_delegation_ledger_query_indexes() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };

    let index_names: Vec<String> = sqlx::query_scalar(
        r#"
        select indexname
        from pg_indexes
        where schemaname='public'
          and indexname in (
              'sessions_delegation_created_idx',
              'delegations_parent_created_idx',
              'delegations_parent_running_idx',
              'delegations_running_created_idx',
              'delegations_completed_repair_idx'
          )
        order by indexname
        "#,
    )
    .fetch_all(&db.store.pool)
    .await
    .expect("list context indexes");

    assert_eq!(
        index_names,
        vec![
            "delegations_completed_repair_idx".to_string(),
            "delegations_parent_created_idx".to_string(),
            "delegations_parent_running_idx".to_string(),
            "delegations_running_created_idx".to_string(),
            "sessions_delegation_created_idx".to_string(),
        ]
    );

    db.cleanup().await;
}

#[tokio::test]
async fn list_delegation_subagents_for_context_is_bounded_and_ordered() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;

    let delegation = db
        .store
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 12)
        .await
        .expect("create delegation");

    for index in 0..12 {
        create_delegation_subagent(
            &db,
            &format!("child_{index:02}"),
            project_id,
            "parent",
            SubagentType::ReadOnly,
            "reviewer",
            &delegation.id,
        )
        .await;
    }

    let subagents = db
        .store
        .list_delegation_subagents_for_context(&delegation.id, 8)
        .await
        .expect("list bounded context subagents");
    let ids = subagents
        .iter()
        .map(|subagent| subagent.session_id.clone())
        .collect::<Vec<_>>();

    assert_eq!(subagents.len(), 9, "limit + 1 row detects omission");
    assert_eq!(
        ids,
        (0..9)
            .map(|index| format!("child_{index:02}"))
            .collect::<Vec<_>>()
    );
    assert!(!ids.contains(&"child_09".to_string()));

    db.cleanup().await;
}

#[tokio::test]
async fn parent_has_running_delegation_tracks_status() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;

    assert!(!db
        .store
        .parent_has_running_delegation("parent")
        .await
        .expect("no delegation yet"));

    let delegation = db
        .store
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    assert!(db
        .store
        .parent_has_running_delegation("parent")
        .await
        .expect("running delegation detected"));

    db.store
        .set_delegation_status(&delegation.id, DelegationStatus::Cancelled)
        .await
        .expect("cancel delegation");
    assert!(!db
        .store
        .parent_has_running_delegation("parent")
        .await
        .expect("cancelled delegation no longer running"));

    db.cleanup().await;
}

#[tokio::test]
async fn parent_delegations_include_complete_parent_set_across_statuses() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;
    create_session(&db, "other_parent", project_id).await;

    let other_parent = db
        .store
        .create_delegation("other_parent", DelegationKind::Full, None, Some("other"), 1)
        .await
        .expect("create other parent delegation");
    let running = db
        .store
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            None,
            Some("running-old"),
            2,
        )
        .await
        .expect("create running delegation");
    let done = db
        .store
        .create_delegation("parent", DelegationKind::Full, None, Some("done"), 1)
        .await
        .expect("create done delegation");
    db.store
        .set_delegation_status(&done.id, DelegationStatus::Done)
        .await
        .expect("finish done delegation");
    let done_with_failures = db
        .store
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            None,
            Some("done-with-failures"),
            2,
        )
        .await
        .expect("create done_with_failures delegation");
    db.store
        .set_delegation_status(&done_with_failures.id, DelegationStatus::DoneWithFailures)
        .await
        .expect("finish done_with_failures delegation");
    let cancelled = db
        .store
        .create_delegation("parent", DelegationKind::Full, None, Some("cancelled"), 1)
        .await
        .expect("create cancelled delegation");
    db.store
        .set_delegation_status(&cancelled.id, DelegationStatus::Cancelled)
        .await
        .expect("cancel delegation");
    let failed = db
        .store
        .create_delegation("parent", DelegationKind::Full, None, Some("failed"), 1)
        .await
        .expect("create failed delegation");
    db.store
        .set_delegation_status(&failed.id, DelegationStatus::Failed)
        .await
        .expect("fail delegation");

    let parent_delegations = db
        .store
        .list_parent_delegations("parent")
        .await
        .expect("list all parent delegations");
    let ids_and_statuses = parent_delegations
        .iter()
        .map(|delegation| (delegation.id.as_str(), delegation.status))
        .collect::<Vec<_>>();
    assert_eq!(
        ids_and_statuses,
        vec![
            (running.id.as_str(), DelegationStatus::Running),
            (done.id.as_str(), DelegationStatus::Done),
            (
                done_with_failures.id.as_str(),
                DelegationStatus::DoneWithFailures
            ),
            (cancelled.id.as_str(), DelegationStatus::Cancelled),
            (failed.id.as_str(), DelegationStatus::Failed),
        ]
    );
    assert!(!parent_delegations
        .iter()
        .any(|delegation| delegation.id == other_parent.id));

    db.cleanup().await;
}

#[tokio::test]
async fn delegation_progress_is_lightweight_and_counts_terminal_failures() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;

    let delegation = db
        .store
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 4)
        .await
        .expect("create delegation");
    create_delegation_subagent_with_leaf(
        &db,
        "child_done",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &delegation.id,
        TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Graceful,
        },
    )
    .await;
    create_delegation_subagent_with_leaf(
        &db,
        "child_failed",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &delegation.id,
        TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Crashed,
        },
    )
    .await;
    create_delegation_subagent_with_leaf(
        &db,
        "child_running",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &delegation.id,
        TranscriptItem::AssistantMessage(AssistantMessage { items: Vec::new() }),
    )
    .await;

    let progress = db
        .store
        .delegation_progress(&delegation)
        .await
        .expect("progress");
    assert_eq!(
        progress,
        DelegationProgress {
            expected: 4,
            spawned: 3,
            terminal: 2,
            running: 2,
            failed: 1,
        }
    );

    db.store
        .set_delegation_status(&delegation.id, DelegationStatus::DoneWithFailures)
        .await
        .expect("terminal status");
    let terminal_delegation = db
        .store
        .get_delegation(&delegation.id)
        .await
        .expect("load")
        .expect("delegation");
    let terminal_progress = db
        .store
        .delegation_progress(&terminal_delegation)
        .await
        .expect("terminal progress");
    assert_eq!(terminal_progress.running, 0);
    assert_eq!(terminal_progress.failed, 1);

    db.cleanup().await;
}

#[tokio::test]
async fn parent_delegations_newest_is_bounded_and_subagent_overview_is_compact() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations newest list test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;

    let mut delegations = Vec::new();
    for index in 0..5 {
        let delegation = db
            .store
            .create_delegation(
                "parent",
                DelegationKind::ReadonlyFanout,
                None,
                Some(&format!("delegation-{index}")),
                1,
            )
            .await
            .expect("create delegation");
        sqlx::query("update delegations set created_at = now() + ($2::int * interval '1 second') where id=$1")
            .bind(&delegation.id)
            .bind(index)
            .execute(&db.store.pool)
            .await
            .expect("set deterministic ordering");
        delegations.push(delegation);
    }

    let newest = db
        .store
        .list_parent_delegations_newest("parent", 3)
        .await
        .expect("list newest delegations");
    assert_eq!(
        newest
            .iter()
            .map(|delegation| delegation.label.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some("delegation-4"),
            Some("delegation-3"),
            Some("delegation-2")
        ]
    );

    let overview_delegation = &delegations[4];
    create_delegation_subagent_with_task_and_leaf(
        &db,
        "child_done",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        Some("review this"),
        &overview_delegation.id,
        Some(TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Graceful,
        }),
    )
    .await;
    create_delegation_subagent_with_task_and_leaf(
        &db,
        "child_running",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        Some("   "),
        &overview_delegation.id,
        Some(TranscriptItem::AssistantMessage(AssistantMessage {
            items: Vec::new(),
        })),
    )
    .await;

    let overview = db
        .store
        .delegation_subagent_overview(&overview_delegation.id)
        .await
        .expect("overview");
    assert_eq!(overview.len(), 2);
    assert_eq!(overview[0].session_id, "child_done");
    assert_eq!(overview[0].activity, crate::SessionActivity::Idle);
    assert_eq!(overview[0].role.as_deref(), Some("reviewer"));
    assert!(overview[0].has_task);
    assert_eq!(overview[0].terminal_status.as_deref(), Some("done"));
    assert_eq!(overview[1].session_id, "child_running");
    assert!(!overview[1].has_task);
    assert_eq!(overview[1].terminal_status, None);

    db.cleanup().await;
}

#[tokio::test]
async fn list_delegation_subagents_returns_only_its_members() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;

    let delegation = db
        .store
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create delegation");
    let other = db
        .store
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create other delegation");

    create_delegation_subagent(
        &db,
        "child_a",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &delegation.id,
    )
    .await;
    create_delegation_subagent(
        &db,
        "child_b",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &delegation.id,
    )
    .await;
    create_delegation_subagent(
        &db,
        "child_other",
        project_id,
        "parent",
        SubagentType::Full,
        "implementer",
        &other.id,
    )
    .await;

    let subagents = db
        .store
        .list_delegation_subagents(&delegation.id)
        .await
        .expect("list delegation subagents");
    let ids = subagents
        .iter()
        .map(|subagent| subagent.session_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["child_a".to_string(), "child_b".to_string()]);
    assert!(subagents
        .iter()
        .all(|subagent| subagent.subagent_type == Some(SubagentType::ReadOnly)));
    assert_eq!(subagents[0].role.as_deref(), Some("reviewer"));

    let parent_delegations = db
        .store
        .list_parent_delegations("parent")
        .await
        .expect("list parent delegations");
    assert_eq!(parent_delegations.len(), 2);
    assert_eq!(parent_delegations[0].id, delegation.id);
    assert_eq!(parent_delegations[1].id, other.id);

    db.cleanup().await;
}

#[tokio::test]
async fn finish_delegation_cas_is_attempt_fenced_and_idempotent() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;
    let delegation = db
        .store
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    // The real attempt id wins exactly once; a replay is a no-op.
    assert!(db
        .store
        .finish_delegation(
            &delegation.id,
            &delegation.attempt_id,
            DelegationStatus::Done
        )
        .await
        .expect("first finish wins"));
    assert!(!db
        .store
        .finish_delegation(
            &delegation.id,
            &delegation.attempt_id,
            DelegationStatus::Done
        )
        .await
        .expect("replay is a no-op"));

    // The status CAS no longer enqueues the wakeup. Publication happens after
    // the handoff files exist, but the deterministic client_input_id still
    // makes the enqueue idempotent.
    let key = format!(
        "delegation-steer:{}:{}",
        delegation.id, delegation.attempt_id
    );
    assert_eq!(steer_count(&db, "parent", &key).await, 0);
    db.store
        .enqueue_delegation_steer("parent", "done", &key)
        .await
        .expect("enqueue steer");
    db.store
        .enqueue_delegation_steer("parent", "done", &key)
        .await
        .expect("enqueue steer idempotent");
    assert_eq!(steer_count(&db, "parent", &key).await, 1);

    // A stale attempt id cannot re-fire a re-opened delegation.
    db.store
        .set_delegation_status(&delegation.id, DelegationStatus::Running)
        .await
        .expect("reopen");
    assert!(!db
        .store
        .finish_delegation(&delegation.id, "stale", DelegationStatus::Done)
        .await
        .expect("stale attempt rejected"));
    assert_eq!(
        db.store
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Running
    );

    // A missing delegation is a benign no-op (late lifecycle event for a deleted delegation).
    assert!(!db
        .store
        .finish_delegation("delegation_missing", "whatever", DelegationStatus::Done)
        .await
        .expect("missing delegation is benign"));

    db.cleanup().await;
}

#[tokio::test]
async fn cancel_running_delegation_is_attempt_fenced_and_terminal_safe() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;

    let delegation = db
        .store
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    assert!(!db
        .store
        .cancel_running_delegation(&delegation.id, "stale-attempt-id")
        .await
        .expect("stale cancel loses"));
    assert_eq!(
        db.store
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Running
    );
    assert!(db
        .store
        .cancel_running_delegation(&delegation.id, &delegation.attempt_id)
        .await
        .expect("real cancel wins"));
    assert!(!db
        .store
        .cancel_running_delegation(&delegation.id, &delegation.attempt_id)
        .await
        .expect("cancel replay loses"));
    assert_eq!(
        db.store
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Cancelled
    );

    let done = db
        .store
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create done delegation");
    assert!(db
        .store
        .finish_delegation(&done.id, &done.attempt_id, DelegationStatus::Done)
        .await
        .expect("finish done"));
    assert!(!db
        .store
        .cancel_running_delegation(&done.id, &done.attempt_id)
        .await
        .expect("cancel after done loses"));
    assert_eq!(
        db.store
            .get_delegation(&done.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Done
    );

    let failed = db
        .store
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create done_with_failures delegation");
    assert!(db
        .store
        .finish_delegation(
            &failed.id,
            &failed.attempt_id,
            DelegationStatus::DoneWithFailures,
        )
        .await
        .expect("finish done_with_failures"));
    assert!(!db
        .store
        .cancel_running_delegation(&failed.id, &failed.attempt_id)
        .await
        .expect("cancel after done_with_failures loses"));
    assert_eq!(
        db.store
            .get_delegation(&failed.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::DoneWithFailures
    );

    db.cleanup().await;
}

#[tokio::test]
async fn cancel_running_delegation_atomically_cancels_queued_partial_wakeup() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;
    let delegation = db
        .store
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create delegation");
    let partial_observation = DaemonToolObservation::inspect_delegation(
        ToolCallId::new("call_partial_atomic_cancel"),
        &delegation.id,
        Some("Subagent finished before cancellation".to_string()),
        json!({
            "delegation_id": delegation.id,
            "status": "running",
        }),
    );
    let partial_key = format!(
        "delegation-steer:{}:{}:done_child",
        delegation.id, delegation.attempt_id
    );
    assert!(db
        .store
        .enqueue_partial_delegation_observation_if_running(
            "parent",
            &delegation.id,
            &delegation.attempt_id,
            &partial_observation,
            &partial_key,
        )
        .await
        .expect("enqueue partial"));
    sqlx::query(
        r#"
        update queued_inputs
        set status='consuming',
            origin=coalesce(origin, '{}'::jsonb)
                || jsonb_build_object('claim_id', 'test-claim', 'claimed_at', now()::text)
        where session_id='parent'
          and client_input_id=$1
        "#,
    )
    .bind(&partial_key)
    .execute(&db.store.pool)
    .await
    .expect("simulate consuming partial");

    // Same delegation/attempt but no trailing ':' is the terminal wakeup key,
    // not a partial decision point; it must not be caught by partial cleanup.
    let terminal_key = format!(
        "delegation-steer:{}:{}",
        delegation.id, delegation.attempt_id
    );
    db.store
        .enqueue_delegation_observation("parent", &partial_observation, &terminal_key)
        .await
        .expect("enqueue terminal-shaped observation");
    db.store
        .enqueue_user_input(
            "parent",
            InputPriority::FollowUp,
            &UserMessage::text("unrelated follow-up"),
            Some("unrelated-follow-up"),
            None,
        )
        .await
        .expect("enqueue unrelated follow-up");

    let (cancelled, events) = db
        .store
        .cancel_running_delegation_and_queued_partials(
            "parent",
            &delegation.id,
            &delegation.attempt_id,
            "delegation_cancelled",
        )
        .await
        .expect("atomic cancel");
    assert!(cancelled);
    assert_eq!(events.len(), 1);
    assert_eq!(
        db.store
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Cancelled
    );
    assert_eq!(
        db.store
            .find_client_input("parent", &partial_key)
            .await
            .expect("find partial")
            .expect("partial row")
            .status,
        QueuedInputStatus::Cancelled
    );
    assert_eq!(
        active_partial_wakeup_count(&db, "parent", &delegation).await,
        0,
        "atomic cancel must leave no queued/consuming partial for this attempt"
    );
    assert_eq!(
        db.store
            .find_client_input("parent", &terminal_key)
            .await
            .expect("find terminal key")
            .expect("terminal row")
            .status,
        QueuedInputStatus::Queued,
        "terminal wakeup key must not match the partial prefix"
    );
    assert_eq!(
        db.store
            .find_client_input("parent", "unrelated-follow-up")
            .await
            .expect("find follow-up")
            .expect("follow-up row")
            .status,
        QueuedInputStatus::Queued
    );

    let next = db
        .store
        .take_next_queued_steer_input("parent")
        .await
        .expect("take next steer")
        .expect("only terminal-shaped steer remains");
    assert_eq!(next.client_input_id.as_deref(), Some(terminal_key.as_str()));

    db.cleanup().await;
}

#[tokio::test]
async fn boot_repair_cancels_partial_wakeup_left_after_cancel_crash_gap() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;
    let delegation = db
        .store
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create delegation");
    let partial_observation = DaemonToolObservation::inspect_delegation(
        ToolCallId::new("call_partial_boot_repair"),
        &delegation.id,
        Some("Subagent finished before cancellation".to_string()),
        json!({
            "delegation_id": delegation.id,
            "status": "running",
        }),
    );
    let partial_key = format!(
        "delegation-steer:{}:{}:done_child",
        delegation.id, delegation.attempt_id
    );
    assert!(db
        .store
        .enqueue_partial_delegation_observation_if_running(
            "parent",
            &delegation.id,
            &delegation.attempt_id,
            &partial_observation,
            &partial_key,
        )
        .await
        .expect("enqueue partial"));

    // Simulate the historical crash window: the cancellation status CAS
    // committed, but the follow-up queued-partial cleanup never ran.
    assert!(db
        .store
        .cancel_running_delegation(&delegation.id, &delegation.attempt_id)
        .await
        .expect("simulate old cancel CAS"));
    assert_eq!(
        db.store
            .find_client_input("parent", &partial_key)
            .await
            .expect("find partial before repair")
            .expect("partial row")
            .status,
        QueuedInputStatus::Queued
    );
    assert_eq!(
        db.store
            .sessions_with_active_queued_inputs()
            .await
            .expect("active queued sessions before repair"),
        vec!["parent".to_string()],
        "top-level parent would be resumed and consume the stale partial before repair"
    );

    let events = db
        .store
        .repair_cancelled_delegation_partial_wakeups()
        .await
        .expect("boot repair");
    assert_eq!(events.len(), 1);
    assert_eq!(
        db.store
            .find_client_input("parent", &partial_key)
            .await
            .expect("find partial after repair")
            .expect("partial row")
            .status,
        QueuedInputStatus::Cancelled
    );
    assert_eq!(
        active_partial_wakeup_count(&db, "parent", &delegation).await,
        0,
        "boot repair must leave no queued/consuming partial for this attempt"
    );
    assert!(
        db.store
            .take_next_queued_steer_input("parent")
            .await
            .expect("take next steer after repair")
            .is_none(),
        "stale cancelled partial must not remain consumable"
    );
    assert!(
        db.store
            .sessions_with_active_queued_inputs()
            .await
            .expect("active queued sessions after repair")
            .is_empty(),
        "boot parent queued-input resume should not see the repaired stale partial"
    );
    assert!(db
        .store
        .repair_cancelled_delegation_partial_wakeups()
        .await
        .expect("boot repair is idempotent")
        .is_empty());

    db.cleanup().await;
}

#[tokio::test]
async fn boot_repair_only_cancels_active_partial_wakeups_for_cancelled_attempts() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;
    create_session(&db, "other_parent", project_id).await;

    let cancelled = db
        .store
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create cancelled delegation");
    let running = db
        .store
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create running delegation");
    let other_cancelled = db
        .store
        .create_delegation(
            "other_parent",
            DelegationKind::ReadonlyFanout,
            None,
            None,
            2,
        )
        .await
        .expect("create other cancelled delegation");
    let observation = DaemonToolObservation::inspect_delegation(
        ToolCallId::new("call_partial_boot_repair_scoped"),
        &cancelled.id,
        Some("Subagent finished before cancellation".to_string()),
        json!({
            "delegation_id": cancelled.id,
            "status": "running",
        }),
    );
    let stale_key = format!(
        "delegation-steer:{}:{}:done_child",
        cancelled.id, cancelled.attempt_id
    );
    assert!(db
        .store
        .enqueue_partial_delegation_observation_if_running(
            "parent",
            &cancelled.id,
            &cancelled.attempt_id,
            &observation,
            &stale_key,
        )
        .await
        .expect("enqueue stale partial"));
    let running_key = format!(
        "delegation-steer:{}:{}:done_child",
        running.id, running.attempt_id
    );
    assert!(db
        .store
        .enqueue_partial_delegation_observation_if_running(
            "parent",
            &running.id,
            &running.attempt_id,
            &observation,
            &running_key,
        )
        .await
        .expect("enqueue running partial"));
    let wrong_parent_key = format!(
        "delegation-steer:{}:{}:done_child",
        other_cancelled.id, other_cancelled.attempt_id
    );
    assert!(db
        .store
        .enqueue_partial_delegation_observation_if_running(
            "other_parent",
            &other_cancelled.id,
            &other_cancelled.attempt_id,
            &observation,
            &wrong_parent_key,
        )
        .await
        .expect("enqueue other parent partial"));
    db.store
        .enqueue_delegation_observation(
            "parent",
            &observation,
            &format!("delegation-steer:{}:{}", cancelled.id, cancelled.attempt_id),
        )
        .await
        .expect("enqueue terminal-shaped cancelled observation");
    db.store
        .enqueue_user_input(
            "parent",
            InputPriority::FollowUp,
            &UserMessage::text("unrelated follow-up"),
            Some("unrelated-follow-up"),
            None,
        )
        .await
        .expect("enqueue unrelated follow-up");

    assert!(db
        .store
        .cancel_running_delegation(&cancelled.id, &cancelled.attempt_id)
        .await
        .expect("cancel stale delegation"));
    assert!(db
        .store
        .cancel_running_delegation(&other_cancelled.id, &other_cancelled.attempt_id)
        .await
        .expect("cancel other delegation"));

    let events = db
        .store
        .repair_cancelled_delegation_partial_wakeups()
        .await
        .expect("boot repair");
    assert_eq!(
        events.len(),
        2,
        "one cancellation event per affected parent session"
    );
    assert_eq!(
        db.store
            .find_client_input("parent", &stale_key)
            .await
            .expect("find stale partial")
            .expect("stale partial row")
            .status,
        QueuedInputStatus::Cancelled
    );
    assert_eq!(
        db.store
            .find_client_input("parent", &running_key)
            .await
            .expect("find running partial")
            .expect("running partial row")
            .status,
        QueuedInputStatus::Queued,
        "running delegation partials must not be cancelled"
    );
    assert_eq!(
        db.store
            .find_client_input("other_parent", &wrong_parent_key)
            .await
            .expect("find other parent partial")
            .expect("other parent partial row")
            .status,
        QueuedInputStatus::Cancelled,
        "matching stale partials on other parents are repaired independently"
    );
    assert_eq!(
        db.store
            .find_client_input(
                "parent",
                &format!("delegation-steer:{}:{}", cancelled.id, cancelled.attempt_id),
            )
            .await
            .expect("find terminal-shaped wakeup")
            .expect("terminal row")
            .status,
        QueuedInputStatus::Queued,
        "terminal wakeup key must not match the partial key shape"
    );
    assert_eq!(
        db.store
            .find_client_input("parent", "unrelated-follow-up")
            .await
            .expect("find follow-up")
            .expect("follow-up row")
            .status,
        QueuedInputStatus::Queued
    );

    db.cleanup().await;
}

#[tokio::test]
async fn all_terminal_predicate_and_boot_sweep() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;
    // The delegation expects TWO subagents (FIX A: the expected-count fence).
    let delegation = db
        .store
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create delegation");

    // An empty delegation (no subagents yet) is NOT terminal — a delegation whose spawn
    // races the barrier must not complete prematurely.
    assert!(!db
        .store
        .delegation_subagents_all_terminal(&delegation.id)
        .await
        .expect("empty delegation not terminal"));
    assert!(db
        .store
        .sweep_running_delegations()
        .await
        .expect("sweep")
        .is_empty());

    // One spawned subagent (of the expected two) is at a boundary, but the
    // expected-count fence keeps the delegation non-terminal — this is the partial
    // spawn window the barrier must never complete (FIX A).
    create_delegation_subagent(
        &db,
        "child_a",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &delegation.id,
    )
    .await;
    assert!(!db
        .store
        .delegation_subagents_all_terminal(&delegation.id)
        .await
        .expect("partial spawn (1 of 2) is NOT terminal"));
    assert!(db
        .store
        .sweep_running_delegations()
        .await
        .expect("sweep")
        .is_empty());

    // Both subagents now exist and both are at a boundary (empty transcript /
    // no active leaf) -> all terminal, and the running delegation is sweep-ready.
    create_delegation_subagent(
        &db,
        "child_b",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &delegation.id,
    )
    .await;
    assert!(db
        .store
        .delegation_subagents_all_terminal(&delegation.id)
        .await
        .expect("both spawned and at a boundary -> terminal"));
    let ready = db.store.sweep_running_delegations().await.expect("sweep");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, delegation.id);

    // A non-running (finished) delegation is not swept again.
    db.store
        .finish_delegation(
            &delegation.id,
            &delegation.attempt_id,
            DelegationStatus::Done,
        )
        .await
        .expect("finish");
    assert!(db
        .store
        .sweep_running_delegations()
        .await
        .expect("sweep")
        .is_empty());

    db.cleanup().await;
}

#[tokio::test]
async fn queued_input_on_boundary_subagent_blocks_delegation_terminality() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(
            project_id,
            "delegations queued terminality test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;
    let delegation = db
        .store
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create delegation");
    create_delegation_subagent(
        &db,
        "child_a",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &delegation.id,
    )
    .await;
    create_delegation_subagent(
        &db,
        "child_b",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &delegation.id,
    )
    .await;
    assert!(db
        .store
        .delegation_subagents_all_terminal(&delegation.id)
        .await
        .expect("both idle boundary subagents are terminal"));

    db.store
        .enqueue_user_input(
            "child_a",
            crate::InputPriority::Steer,
            &UserMessage::text("accepted queued steer must run before fan-out completes"),
            Some("queued-steer-before-barrier"),
            None,
        )
        .await
        .expect("enqueue steer");
    assert!(!db
        .store
        .delegation_subagents_all_terminal(&delegation.id)
        .await
        .expect("queued steer blocks terminality"));
    assert!(db
        .store
        .sweep_running_delegations()
        .await
        .expect("sweep")
        .is_empty());
    let progress = db
        .store
        .delegation_progress(&delegation)
        .await
        .expect("progress with queued steer");
    assert_eq!(progress.terminal, 1);
    assert_eq!(progress.running, 1);

    let consumed = db
        .store
        .take_next_queued_input("child_a")
        .await
        .expect("take queued steer")
        .expect("queued steer exists");
    db.store
        .persist_outputs(
            "child_a",
            OutputBatch::new(&[], None, &[], &[]).with_consumed_input(Some(consumed)),
        )
        .await
        .expect("mark steer consumed");
    assert!(db
        .store
        .delegation_subagents_all_terminal(&delegation.id)
        .await
        .expect("terminal after steer consumed"));
    let ready = db.store.sweep_running_delegations().await.expect("sweep");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, delegation.id);

    db.cleanup().await;
}

#[tokio::test]
async fn enqueue_delegation_observation_event_uses_minimal_payload_and_queue_projection() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;
    let observation = DaemonToolObservation::inspect_delegation(
        ToolCallId::new("call_delegation_1_attempt_1"),
        "delegation_1",
        Some("Delegation delegation_1 completed with status done".to_string()),
        json!({
            "delegation_id": "delegation_1",
            "status": "done",
            "subagents": [{
                "id": "child",
                "outcome": "approved",
                "final_message_file": "child/final_message.md",
                "transcript_file": "child/transcript.md",
            }],
        }),
    );

    db.store
        .enqueue_delegation_observation("parent", &observation, "typed-wakeup")
        .await
        .expect("enqueue observation");

    let payload: Value = sqlx::query_scalar(
        "select payload from events where session_id=$1 and type='input.queued' order by id desc limit 1",
    )
    .bind("parent")
    .fetch_one(&db.store.pool)
    .await
    .expect("load input queued event");

    assert!(payload.get("content_type").is_none());
    assert!(payload.get("content").is_none());
    assert!(payload.get("editable").is_none());
    assert!(payload.get("summary").is_none());
    assert!(payload.get("tool_name").is_none());
    assert!(payload.get("delegation_id").is_none());
    assert_eq!(payload["status"], "queued");
    assert_eq!(payload["client_input_id"], "typed-wakeup");
    let queued = payload["queued_inputs"]
        .as_array()
        .expect("queued inputs")
        .first()
        .expect("queued input");
    assert_eq!(queued["content_type"], "daemon_tool_observation");
    assert_eq!(queued["content"], json!([]));
    assert_eq!(queued["editable"], false);
    assert!(queued.get("summary").is_none());
    let payload_text = payload.to_string();
    assert!(!payload_text.contains("Delegation delegation_1 completed with status done"));
    assert!(!payload_text.contains("subagents"));

    db.cleanup().await;
}

#[tokio::test]
async fn partial_delegation_observation_suppresses_duplicate_active_wakeups_transactionally() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "delegations test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;
    let delegation = db
        .store
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            None,
            Some("partial race"),
            3,
        )
        .await
        .expect("create delegation");
    let observation_a = DaemonToolObservation::inspect_delegation(
        ToolCallId::new("call_partial_a"),
        &delegation.id,
        Some("child a finished".to_string()),
        json!({
            "delegation_id": delegation.id,
            "status": "running",
        }),
    );
    let observation_b = DaemonToolObservation::inspect_delegation(
        ToolCallId::new("call_partial_b"),
        &delegation.id,
        Some("child b finished".to_string()),
        json!({
            "delegation_id": delegation.id,
            "status": "running",
        }),
    );
    let key_a = format!(
        "delegation-steer:{}:{}:child_a",
        delegation.id, delegation.attempt_id
    );
    let key_b = format!(
        "delegation-steer:{}:{}:child_b",
        delegation.id, delegation.attempt_id
    );

    let insert_a = db.store.enqueue_partial_delegation_observation_if_running(
        "parent",
        &delegation.id,
        &delegation.attempt_id,
        &observation_a,
        &key_a,
    );
    let insert_b = db.store.enqueue_partial_delegation_observation_if_running(
        "parent",
        &delegation.id,
        &delegation.attempt_id,
        &observation_b,
        &key_b,
    );
    let (insert_a, insert_b) = tokio::join!(insert_a, insert_b);
    let inserted = [
        insert_a.expect("first insert attempt"),
        insert_b.expect("second insert attempt"),
    ];
    assert_eq!(
        inserted.into_iter().filter(|inserted| *inserted).count(),
        1,
        "concurrent terminal children must create only one active partial wakeup"
    );
    let prefix = format!(
        "delegation-steer:{}:{}:",
        delegation.id, delegation.attempt_id
    );
    let active_count: i64 = sqlx::query_scalar(
        r#"
        select count(*)
        from queued_inputs
        where session_id='parent'
          and priority='steer'
          and status in ('queued', 'consuming')
          and left(client_input_id, char_length($1)) = $1
        "#,
    )
    .bind(&prefix)
    .fetch_one(&db.store.pool)
    .await
    .expect("count active partials");
    assert_eq!(active_count, 1);

    sqlx::query(
        r#"
        update queued_inputs
        set status='consuming'
        where session_id='parent'
          and left(client_input_id, char_length($1)) = $1
        "#,
    )
    .bind(&prefix)
    .execute(&db.store.pool)
    .await
    .expect("mark partial consuming");
    let key_c = format!(
        "delegation-steer:{}:{}:child_c",
        delegation.id, delegation.attempt_id
    );
    let inserted_c = db
        .store
        .enqueue_partial_delegation_observation_if_running(
            "parent",
            &delegation.id,
            &delegation.attempt_id,
            &observation_b,
            &key_c,
        )
        .await
        .expect("third insert attempt");
    assert!(
        !inserted_c,
        "a consuming partial is still an active parent decision point"
    );

    db.cleanup().await;
}

async fn steer_count(db: &TestDb, session_id: &str, client_input_id: &str) -> i64 {
    sqlx::query_scalar(
        "select count(*) from queued_inputs where session_id=$1 and client_input_id=$2 and priority='steer'",
    )
    .bind(session_id)
    .bind(client_input_id)
    .fetch_one(&db.store.pool)
    .await
    .expect("count steers")
}

async fn active_partial_wakeup_count(
    db: &TestDb,
    parent_session_id: &str,
    delegation: &Delegation,
) -> i64 {
    let prefix = format!(
        "delegation-steer:{}:{}:",
        delegation.id, delegation.attempt_id
    );
    sqlx::query_scalar(
        r#"
        select count(*)
        from queued_inputs
        where session_id=$1
          and priority='steer'
          and status in ('queued', 'consuming')
          and left(client_input_id, char_length($2)) = $2
        "#,
    )
    .bind(parent_session_id)
    .bind(&prefix)
    .fetch_one(&db.store.pool)
    .await
    .expect("count active partial wakeups")
}
