use std::sync::atomic::{AtomicU64, Ordering};

use agent_session::TranscriptStorageNode;
use agent_vocab::{
    AssistantMessage, ProviderConfig, ProviderKind, ReasoningEffort, TranscriptItem, TurnId,
    TurnOutcome, UserMessage,
};
use serde_json::json;
use uuid::Uuid;

use crate::{DelegationKind, DelegationStatus, SessionConfig, SubagentType};

use super::*;

static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(70_000);

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
    let metadata = match role_name {
        Some(role_name) => json!({ "created_by": "test", "role_name": role_name }),
        None => json!({ "created_by": "test" }),
    };
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
async fn migration_creates_context_query_indexes() {
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
              'delegations_parent_running_updated_idx',
              'delegations_parent_terminal_updated_idx'
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
            "delegations_parent_running_updated_idx".to_string(),
            "delegations_parent_terminal_updated_idx".to_string(),
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
async fn current_delegations_include_all_running_latest_terminals_and_only_parent() {
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

    let other_parent_running = db
        .store
        .create_delegation("other_parent", DelegationKind::Full, None, Some("other"), 1)
        .await
        .expect("create other parent running");
    let running_old = db
        .store
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            None,
            Some("running-old"),
            2,
        )
        .await
        .expect("create old running delegation");
    sqlx::query("update delegations set updated_at = now() - interval '1 hour' where id=$1")
        .bind(&running_old.id)
        .execute(&db.store.pool)
        .await
        .expect("age old running delegation");
    let running_new = db
        .store
        .create_delegation("parent", DelegationKind::Full, None, Some("running-new"), 1)
        .await
        .expect("create new running delegation");

    let mut terminals = Vec::new();
    for index in 0..5 {
        let label = format!("terminal-{index}");
        let terminal = db
            .store
            .create_delegation("parent", DelegationKind::Full, None, Some(&label), 1)
            .await
            .expect("create terminal delegation");
        db.store
            .set_delegation_status(&terminal.id, DelegationStatus::Done)
            .await
            .expect("finish terminal delegation");
        sqlx::query(
            "update delegations set updated_at = now() - ($2::int * interval '1 second') where id=$1",
        )
        .bind(&terminal.id)
        .bind(10 - index)
        .execute(&db.store.pool)
        .await
        .expect("set terminal recency");
        terminals.push(terminal);
    }
    let other_parent_terminal = db
        .store
        .create_delegation(
            "other_parent",
            DelegationKind::Full,
            None,
            Some("other-done"),
            1,
        )
        .await
        .expect("create other parent terminal");
    db.store
        .set_delegation_status(&other_parent_terminal.id, DelegationStatus::Done)
        .await
        .expect("finish other parent terminal");
    sqlx::query("update delegations set updated_at = now() where id=$1")
        .bind(&other_parent_terminal.id)
        .execute(&db.store.pool)
        .await
        .expect("freshen other parent terminal");

    let current = db
        .store
        .list_parent_current_delegations("parent", 3)
        .await
        .expect("list current delegations");
    let ids = current
        .iter()
        .map(|delegation| delegation.id.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        ids,
        vec![
            running_new.id.as_str(),
            running_old.id.as_str(),
            terminals[4].id.as_str(),
            terminals[3].id.as_str(),
            terminals[2].id.as_str(),
        ]
    );
    assert!(!ids.contains(&terminals[0].id.as_str()));
    assert!(!ids.contains(&terminals[1].id.as_str()));
    assert!(!ids.contains(&other_parent_running.id.as_str()));
    assert!(!ids.contains(&other_parent_terminal.id.as_str()));

    let running_only = db
        .store
        .list_parent_current_delegations("parent", 0)
        .await
        .expect("list running-only current delegations");
    let running_only_ids = running_only
        .iter()
        .map(|delegation| delegation.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        running_only_ids,
        vec![running_new.id.as_str(), running_old.id.as_str()]
    );

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

    // The status CAS no longer enqueues the steer. Publication happens after
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
