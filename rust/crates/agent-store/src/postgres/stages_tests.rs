use std::sync::atomic::{AtomicU64, Ordering};

use agent_vocab::{ProviderConfig, ProviderKind, ReasoningEffort, UserMessage};
use serde_json::json;
use uuid::Uuid;

use crate::{SessionConfig, StageKind, StageStatus, SubagentType};

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
        "pi_relay_stages_test_{}_{}",
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

async fn create_stage_subagent(
    db: &TestDb,
    session_id: &str,
    project_id: Uuid,
    parent_session_id: &str,
    subagent_type: SubagentType,
    role_name: &str,
    stage_id: &str,
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
            Some(stage_id),
        )
        .await
        .expect("create stage subagent");
}

#[tokio::test]
async fn create_stage_persists_kind_status_and_attempt() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "stages test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;

    let stage = db
        .store
        .create_stage(
            "parent",
            StageKind::ReadonlyFanout,
            Some("implement_review_test"),
            Some("review fan-out"),
            3,
        )
        .await
        .expect("create stage");
    assert_eq!(stage.expected_subagents, 3);
    assert_eq!(stage.kind, StageKind::ReadonlyFanout);
    assert_eq!(stage.status, StageStatus::Running);
    assert!(!stage.attempt_id.is_empty());

    let loaded = db
        .store
        .get_stage(&stage.id)
        .await
        .expect("get stage")
        .expect("stage exists");
    assert_eq!(loaded.parent_session_id, "parent");
    assert_eq!(loaded.workflow.as_deref(), Some("implement_review_test"));
    assert_eq!(loaded.label.as_deref(), Some("review fan-out"));
    assert_eq!(loaded.status, StageStatus::Running);

    db.cleanup().await;
}

#[tokio::test]
async fn parent_has_running_stage_tracks_status() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "stages test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;

    assert!(!db
        .store
        .parent_has_running_stage("parent")
        .await
        .expect("no stage yet"));

    let stage = db
        .store
        .create_stage("parent", StageKind::Full, None, None, 1)
        .await
        .expect("create stage");
    assert!(db
        .store
        .parent_has_running_stage("parent")
        .await
        .expect("running stage detected"));

    db.store
        .set_stage_status(&stage.id, StageStatus::Cancelled)
        .await
        .expect("cancel stage");
    assert!(!db
        .store
        .parent_has_running_stage("parent")
        .await
        .expect("cancelled stage no longer running"));

    db.cleanup().await;
}

#[tokio::test]
async fn list_stage_subagents_returns_only_its_members() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "stages test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;

    let stage = db
        .store
        .create_stage("parent", StageKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create stage");
    let other = db
        .store
        .create_stage("parent", StageKind::Full, None, None, 1)
        .await
        .expect("create other stage");

    create_stage_subagent(
        &db,
        "child_a",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &stage.id,
    )
    .await;
    create_stage_subagent(
        &db,
        "child_b",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &stage.id,
    )
    .await;
    create_stage_subagent(
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
        .list_stage_subagents(&stage.id)
        .await
        .expect("list stage subagents");
    let ids = subagents
        .iter()
        .map(|subagent| subagent.session_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["child_a".to_string(), "child_b".to_string()]);
    assert!(subagents
        .iter()
        .all(|subagent| subagent.subagent_type == Some(SubagentType::ReadOnly)));
    assert_eq!(subagents[0].role.as_deref(), Some("reviewer"));

    let parent_stages = db
        .store
        .list_parent_stages("parent")
        .await
        .expect("list parent stages");
    assert_eq!(parent_stages.len(), 2);
    assert_eq!(parent_stages[0].id, stage.id);
    assert_eq!(parent_stages[1].id, other.id);

    db.cleanup().await;
}

#[tokio::test]
async fn finish_stage_cas_is_attempt_fenced_and_idempotent() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    db.store
        .create_project(project_id, "stages test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;
    let stage = db
        .store
        .create_stage("parent", StageKind::Full, None, None, 1)
        .await
        .expect("create stage");
    let key = format!("stage-steer:{}:{}", stage.id, stage.attempt_id);

    // The real attempt id wins exactly once; a replay is a no-op.
    assert!(db
        .store
        .finish_stage(
            &stage.id,
            &stage.attempt_id,
            StageStatus::Done,
            "parent",
            "done",
            &key
        )
        .await
        .expect("first finish wins"));
    assert!(!db
        .store
        .finish_stage(
            &stage.id,
            &stage.attempt_id,
            StageStatus::Done,
            "parent",
            "done",
            &key
        )
        .await
        .expect("replay is a no-op"));

    // The steer was enqueued atomically with the winning CAS, exactly once
    // (the deterministic client_input_id makes a replay a no-op).
    assert_eq!(steer_count(&db, "parent", &key).await, 1);

    // A stale attempt id cannot re-fire a re-opened stage.
    db.store
        .set_stage_status(&stage.id, StageStatus::Running)
        .await
        .expect("reopen");
    assert!(!db
        .store
        .finish_stage(
            &stage.id,
            "stale",
            StageStatus::Done,
            "parent",
            "done",
            &key
        )
        .await
        .expect("stale attempt rejected"));
    assert_eq!(
        db.store.get_stage(&stage.id).await.unwrap().unwrap().status,
        StageStatus::Running
    );

    // A missing stage is a benign no-op (late lifecycle event for a deleted stage).
    assert!(!db
        .store
        .finish_stage(
            "stage_missing",
            "whatever",
            StageStatus::Done,
            "parent",
            "done",
            "k"
        )
        .await
        .expect("missing stage is benign"));

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
        .create_project(project_id, "stages test", &[], json!({}))
        .await
        .expect("create project");
    create_session(&db, "parent", project_id).await;
    // The stage expects TWO subagents (FIX A: the expected-count fence).
    let stage = db
        .store
        .create_stage("parent", StageKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create stage");

    // An empty stage (no subagents yet) is NOT terminal — a stage whose spawn
    // races the barrier must not complete prematurely.
    assert!(!db
        .store
        .stage_subagents_all_terminal(&stage.id)
        .await
        .expect("empty stage not terminal"));
    assert!(db
        .store
        .sweep_running_stages()
        .await
        .expect("sweep")
        .is_empty());

    // One spawned subagent (of the expected two) is at a boundary, but the
    // expected-count fence keeps the stage non-terminal — this is the partial
    // spawn window the barrier must never complete (FIX A).
    create_stage_subagent(
        &db,
        "child_a",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &stage.id,
    )
    .await;
    assert!(!db
        .store
        .stage_subagents_all_terminal(&stage.id)
        .await
        .expect("partial spawn (1 of 2) is NOT terminal"));
    assert!(db
        .store
        .sweep_running_stages()
        .await
        .expect("sweep")
        .is_empty());

    // Both subagents now exist and both are at a boundary (empty transcript /
    // no active leaf) -> all terminal, and the running stage is sweep-ready.
    create_stage_subagent(
        &db,
        "child_b",
        project_id,
        "parent",
        SubagentType::ReadOnly,
        "reviewer",
        &stage.id,
    )
    .await;
    assert!(db
        .store
        .stage_subagents_all_terminal(&stage.id)
        .await
        .expect("both spawned and at a boundary -> terminal"));
    let ready = db.store.sweep_running_stages().await.expect("sweep");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, stage.id);

    // A non-running (finished) stage is not swept again.
    let key = format!("stage-steer:{}:{}", stage.id, stage.attempt_id);
    db.store
        .finish_stage(
            &stage.id,
            &stage.attempt_id,
            StageStatus::Done,
            "parent",
            "done",
            &key,
        )
        .await
        .expect("finish");
    assert!(db
        .store
        .sweep_running_stages()
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
