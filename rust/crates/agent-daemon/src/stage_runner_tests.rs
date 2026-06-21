//! Deterministic stage barrier / handoff / steer tests against a real Postgres.
//!
//! These drive the barrier directly (the live lifecycle hook and the boot
//! sweep both funnel through `complete_stage_if_ready`), with subagents placed
//! into terminal/running states by writing their durable transcripts directly,
//! so the tests are fully deterministic and need no provider.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use agent_session::TranscriptStorageNode;
use agent_store::{
    EventType, InputPriority, PostgresAgentStore, SessionConfig, StageKind, StageStatus,
    SubagentType,
};
use agent_tools::ToolRegistry;
use agent_vocab::{
    AssistantItem, AssistantMessage, ProviderConfig, ProviderKind, ReasoningEffort, TranscriptItem,
    TurnId, TurnOutcome, UserMessage,
};
use serde_json::json;
use tokio::sync::{broadcast, Mutex};
use uuid::Uuid;

/// A unique temp directory removed on drop, so tests need no `tempfile` dep.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pi-relay-barrier-{}-{}-{}",
            label,
            std::process::id(),
            TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

use crate::provider_runtime::{ProviderConnectionRegistry, SessionTitleScheduler};
use crate::runtime::SessionDriver;
use crate::state::AppState;
use crate::workspaces::WorkspaceManager;

use super::{complete_stage_if_ready, sweep_running_stages_on_boot};

static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(90_000);

struct TestEnv {
    state: AppState,
    admin_url: String,
    name: String,
    _state_dir: TempDir,
    cwd: TempDir,
}

impl TestEnv {
    async fn cleanup(self) {
        self.state.repo.close().await;
        if let Ok(admin) = sqlx::PgPool::connect(&self.admin_url).await {
            let _ = sqlx::query(&format!(r#"drop database if exists "{}""#, self.name))
                .execute(&admin)
                .await;
            admin.close().await;
        }
    }

    fn outer_cwd(&self) -> String {
        self.cwd.path().to_string_lossy().into_owned()
    }
}

async fn test_env() -> Option<TestEnv> {
    let admin_url = std::env::var("PI_RELAY_TEST_DATABASE_URL").ok()?;
    let name = format!(
        "pi_relay_barrier_test_{}_{}",
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

    let state_dir = TempDir::new("state");
    let cwd = TempDir::new("cwd");
    let (events, _rx) = broadcast::channel(1024);
    let state = AppState {
        repo: Arc::new(store),
        active: Arc::new(Mutex::new(HashMap::new())),
        session_driver_locks: Arc::new(Mutex::new(HashMap::new())),
        tasks: Arc::new(StdMutex::new(HashMap::new())),
        events,
        tools: Arc::new(ToolRegistry::with_builtin_tools()),
        provider_connections: ProviderConnectionRegistry::new(),
        session_titles: SessionTitleScheduler::default(),
        workspaces: WorkspaceManager::for_tests(state_dir.path().to_path_buf()),
        prompt_root: cwd.path().to_path_buf(),
    };
    Some(TestEnv {
        state,
        admin_url,
        name,
        _state_dir: state_dir,
        cwd,
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

fn session_config(env: &TestEnv, project_id: Uuid, metadata: serde_json::Value) -> SessionConfig {
    SessionConfig {
        project_id: Some(project_id),
        outer_cwd: env.outer_cwd(),
        workspaces: Vec::new(),
        system_prompt: String::new(),
        provider: ProviderConfig {
            kind: ProviderKind::OpenAi,
            model: "gpt-5.2".to_string(),
            reasoning_effort: ReasoningEffort::Medium,
            max_tokens: None,
            prompt_cache: None,
        },
        metadata,
    }
}

/// A parent session that opts into the harness, so any model dispatch the
/// barrier's steer triggers stops at `pending` instead of calling a provider.
async fn create_parent(env: &TestEnv, project_id: Uuid, parent_id: &str) {
    env.state
        .repo
        .start_session_outputs(
            parent_id,
            &session_config(
                env,
                project_id,
                json!({ "created_by": "test", "harness": true }),
            ),
            &[],
            None,
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("go"),
            None,
        )
        .await
        .expect("create parent");
}

/// Create a stage subagent whose durable transcript carries one assistant turn
/// finished with `outcome`, then settle it terminal (no queued input, no
/// unfinished action) so the all-terminal predicate sees it as done.
// Test fixture: each argument shapes a distinct field of the subagent transcript.
#[allow(clippy::too_many_arguments)]
async fn create_terminal_subagent(
    env: &TestEnv,
    project_id: Uuid,
    parent_id: &str,
    stage_id: &str,
    session_id: &str,
    role: &str,
    subagent_type: SubagentType,
    outcome: TurnOutcome,
    final_message: &str,
) {
    let leaf = format!("{session_id}_finish");
    let entries = vec![
        TranscriptStorageNode {
            id: format!("{session_id}_u"),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::UserMessage(UserMessage::text("do the task")),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: format!("{session_id}_a"),
            parent_id: Some(format!("{session_id}_u")),
            timestamp_ms: 2,
            item: TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text(final_message.to_string())],
            }),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: leaf.clone(),
            parent_id: Some(format!("{session_id}_a")),
            timestamp_ms: 3,
            item: TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome,
            },
            provider_replay: Vec::new(),
        },
    ];
    env.state
        .repo
        .start_session_outputs_with_parent(
            session_id,
            &session_config(
                env,
                project_id,
                json!({ "created_by": "test", "role_name": role }),
            ),
            &entries,
            Some(&leaf),
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("do the task"),
            None,
            Some(parent_id),
            Some(subagent_type),
            Some(stage_id),
        )
        .await
        .expect("create terminal subagent");
    // The accepted input above is recorded as the turn's user message, not a
    // live queued input, and we persisted no action — so the session is idle
    // (terminal). Confirm, so the test's premise is sound.
    assert_eq!(
        env.state.repo.activity(session_id).await.expect("activity"),
        agent_store::SessionActivity::Idle
    );
}

/// A non-terminal (mid-turn) subagent: its active leaf is an assistant message,
/// NOT a turn boundary, so the transcript-boundary terminality (FIX C) keeps it
/// out of the terminal set and the barrier must not fire. A `TurnFinished` node
/// is pre-attached but inactive; `settle_subagent_terminal` switches the active
/// leaf to it to make the subagent terminal without appending. Returns that
/// boundary leaf id.
async fn create_running_subagent(
    env: &TestEnv,
    project_id: Uuid,
    parent_id: &str,
    stage_id: &str,
    session_id: &str,
    role: &str,
    outcome: TurnOutcome,
) -> String {
    let mid_turn = format!("{session_id}_a");
    let boundary = format!("{session_id}_finish");
    let entries = vec![
        TranscriptStorageNode {
            id: format!("{session_id}_u"),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::UserMessage(UserMessage::text("keep working")),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: mid_turn.clone(),
            parent_id: Some(format!("{session_id}_u")),
            timestamp_ms: 2,
            item: TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("working...".to_string())],
            }),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: boundary.clone(),
            parent_id: Some(mid_turn.clone()),
            timestamp_ms: 3,
            item: TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome,
            },
            provider_replay: Vec::new(),
        },
    ];
    env.state
        .repo
        .start_session_outputs_with_parent(
            session_id,
            &session_config(
                env,
                project_id,
                json!({ "created_by": "test", "role_name": role }),
            ),
            &entries,
            Some(&mid_turn),
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("keep working"),
            None,
            Some(parent_id),
            Some(SubagentType::ReadOnly),
            Some(stage_id),
        )
        .await
        .expect("create running subagent");
    // The active leaf is the assistant message — a non-boundary, so the subagent
    // is NON-terminal even though it has no queued input or unfinished action.
    assert!(!env
        .state
        .repo
        .active_leaf_is_turn_boundary(session_id)
        .await
        .expect("boundary check"));
    boundary
}

/// Settle a subagent created by `create_running_subagent` terminal by switching
/// its active leaf to the pre-attached `TurnFinished` boundary node.
async fn settle_subagent_terminal(env: &TestEnv, session_id: &str, boundary_leaf: &str) {
    env.state
        .repo
        .set_active_leaf(session_id, Some(boundary_leaf))
        .await
        .expect("switch to boundary leaf");
    assert!(env
        .state
        .repo
        .active_leaf_is_turn_boundary(session_id)
        .await
        .expect("boundary check"));
}

/// Count completion steers that reached the parent. An idle parent accepts the
/// steer as its next user-message turn, so the steer lands in the parent's
/// transcript; we count user messages naming the stage.
async fn steers_to_parent(env: &TestEnv, parent_id: &str, stage_id: &str) -> usize {
    let history = env
        .state
        .repo
        .active_branch(parent_id)
        .await
        .expect("parent active branch");
    history
        .entries
        .iter()
        .filter(|entry| match &entry.item {
            TranscriptItem::UserMessage(message) => message
                .as_text()
                .is_some_and(|text| text.contains(stage_id) && text.contains("finished")),
            _ => false,
        })
        .count()
}

fn read_index(env: &TestEnv, stage_id: &str) -> serde_json::Value {
    let path = env
        .cwd
        .path()
        .join(".pi-handoff")
        .join(stage_id)
        .join("index.json");
    let raw = std::fs::read_to_string(path).expect("index.json exists");
    serde_json::from_str(&raw).expect("index.json parses")
}

#[tokio::test]
async fn barrier_steers_once_after_all_terminal_with_handoff_for_every_subagent() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "barrier test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let stage = env
        .state
        .repo
        .create_stage(
            "parent",
            StageKind::ReadonlyFanout,
            Some("implement_review_test"),
            Some("review"),
            2,
        )
        .await
        .expect("create stage");

    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "ok_a",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "All good.\n\nsuggested_next: approved",
    )
    .await;
    let boundary_leaf = create_running_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "still_running",
        "reviewer",
        TurnOutcome::Crashed,
    )
    .await;

    // Not all subagents terminal yet (the second is mid-turn) -> barrier must not
    // fire. Recovery of an idle mid-turn subagent leaves it at its non-boundary
    // leaf, so it stays non-terminal.
    complete_stage_if_ready(&env.state, &stage.id)
        .await
        .expect("barrier (partial)");
    assert_eq!(
        env.state
            .repo
            .get_stage(&stage.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        StageStatus::Running
    );
    assert_eq!(steers_to_parent(&env, "parent", &stage.id).await, 0);

    // Settle the second subagent terminal at a Crashed boundary — the barrier
    // classifies a non-graceful TurnFinished as a failure, exactly as a child
    // that died mid-task and was recovered to a boundary would appear.
    settle_subagent_terminal(&env, "still_running", &boundary_leaf).await;

    // Now all terminal -> exactly one steer, done_with_failures, handoff for all.
    complete_stage_if_ready(&env.state, &stage.id)
        .await
        .expect("barrier (complete)");
    assert_eq!(
        env.state
            .repo
            .get_stage(&stage.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        StageStatus::DoneWithFailures
    );
    assert_eq!(steers_to_parent(&env, "parent", &stage.id).await, 1);

    // Re-delivered events must not double-steer (idempotent via the CAS).
    complete_stage_if_ready(&env.state, &stage.id)
        .await
        .expect("barrier (replay)");
    sweep_running_stages_on_boot(&env.state).await;
    assert_eq!(steers_to_parent(&env, "parent", &stage.id).await, 1);

    // Handoff: index.json + per-subagent files for EVERY subagent (incl. failed).
    let index = read_index(&env, &stage.id);
    assert_eq!(index["status"], "done_with_failures");
    assert_eq!(index["kind"], "readonly_fanout");
    assert_eq!(index["workflow"], "implement_review_test");
    let subagents = index["subagents"].as_array().expect("subagents array");
    assert_eq!(subagents.len(), 2);
    for subagent in subagents {
        let id = subagent["id"].as_str().unwrap();
        let base = env.cwd.path().join(".pi-handoff").join(&stage.id).join(id);
        assert!(
            base.join("final_message.md").exists(),
            "final_message for {id}"
        );
        assert!(base.join("transcript.md").exists(), "transcript for {id}");
    }
    let ok = subagents.iter().find(|s| s["id"] == "ok_a").unwrap();
    assert_eq!(ok["status"], "done");
    assert_eq!(ok["suggested_next"], "approved");
    let failed = subagents
        .iter()
        .find(|s| s["id"] == "still_running")
        .unwrap();
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["suggested_next"], serde_json::Value::Null);

    env.cleanup().await;
}

#[tokio::test]
async fn out_of_set_suggested_next_is_recorded_verbatim() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "barrier test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let stage = env
        .state
        .repo
        .create_stage("parent", StageKind::Full, None, None, 1)
        .await
        .expect("create stage");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "impl",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "Done.\nsuggested_next: ship_it_immediately",
    )
    .await;

    complete_stage_if_ready(&env.state, &stage.id)
        .await
        .expect("barrier");
    let index = read_index(&env, &stage.id);
    assert_eq!(index["status"], "done");
    assert_eq!(
        index["subagents"][0]["suggested_next"],
        "ship_it_immediately"
    );

    env.cleanup().await;
}

#[tokio::test]
async fn stale_attempt_id_cannot_finish_stage() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "barrier test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let stage = env
        .state
        .repo
        .create_stage("parent", StageKind::Full, None, None, 1)
        .await
        .expect("create stage");

    let key = format!("stage-steer:{}:{}", stage.id, stage.attempt_id);
    // The real attempt_id wins exactly once.
    assert!(env
        .state
        .repo
        .finish_stage(
            &stage.id,
            &stage.attempt_id,
            StageStatus::Done,
            "parent",
            "done",
            &key
        )
        .await
        .expect("finish"));
    // A second call with the same id is a no-op (status no longer running).
    assert!(!env
        .state
        .repo
        .finish_stage(
            &stage.id,
            &stage.attempt_id,
            StageStatus::Done,
            "parent",
            "done",
            &key
        )
        .await
        .expect("finish again"));

    // Re-open it and try a stale attempt id: must not transition.
    env.state
        .repo
        .set_stage_status(&stage.id, StageStatus::Running)
        .await
        .expect("reopen");
    assert!(!env
        .state
        .repo
        .finish_stage(
            &stage.id,
            "stale-attempt-id",
            StageStatus::Done,
            "parent",
            "done",
            &key
        )
        .await
        .expect("stale finish"));
    assert_eq!(
        env.state
            .repo
            .get_stage(&stage.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        StageStatus::Running
    );

    env.cleanup().await;
}

#[tokio::test]
async fn boot_sweep_completes_a_crash_mid_barrier_stage_exactly_once() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "barrier test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let stage = env
        .state
        .repo
        .create_stage("parent", StageKind::Full, None, None, 1)
        .await
        .expect("create stage");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "impl",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "Implemented.",
    )
    .await;

    // The stage is still `running` with all subagents terminal — i.e. a crash
    // mid-barrier. The boot sweep completes it exactly once.
    sweep_running_stages_on_boot(&env.state).await;
    assert_eq!(
        env.state
            .repo
            .get_stage(&stage.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        StageStatus::Done
    );
    assert_eq!(steers_to_parent(&env, "parent", &stage.id).await, 1);

    // A second sweep (another restart) must not double-steer.
    sweep_running_stages_on_boot(&env.state).await;
    assert_eq!(steers_to_parent(&env, "parent", &stage.id).await, 1);

    env.cleanup().await;
}

// --- Phase-2 guard tests (deferred until this harness existed) ---

#[tokio::test]
async fn one_stage_per_parent_is_rejected() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "guard test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    // A running stage already exists for this parent.
    env.state
        .repo
        .create_stage("parent", StageKind::Full, None, None, 1)
        .await
        .expect("create stage");

    let error = crate::stage_tools::start_full_core(
        &env.state,
        "parent",
        json!({ "role": "implementer", "prompt": "do it" }),
    )
    .await
    .expect_err("second stage must be rejected");
    assert_eq!(error.code, "stage_already_running");

    env.cleanup().await;
}

#[tokio::test]
async fn subagent_cannot_start_a_nested_stage() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "guard test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let stage = env
        .state
        .repo
        .create_stage("parent", StageKind::Full, None, None, 1)
        .await
        .expect("create stage");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "child",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "done",
    )
    .await;

    // The subagent (which has a subagent_type) cannot itself orchestrate a stage.
    let error = crate::stage_tools::start_readonly_fanout_core(
        &env.state,
        "child",
        json!({ "tasks": [{ "role": "reviewer", "prompt": "review" }] }),
    )
    .await
    .expect_err("nested stage must be rejected");
    assert_eq!(error.code, "stages_not_allowed_for_subagent");

    env.cleanup().await;
}

#[tokio::test]
async fn spawn_failure_leaves_no_running_stage() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    // A parent with NO project makes spawn_subagent fail with project_required,
    // exercising the compensation path: the half-started stage is failed so the
    // one-stage-per-parent guard releases rather than stranding the parent.
    env.state
        .repo
        .start_session_outputs(
            "parent",
            &SessionConfig {
                project_id: None,
                ..session_config(&env, Uuid::new_v4(), json!({ "created_by": "test" }))
            },
            &[],
            None,
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("go"),
            None,
        )
        .await
        .expect("create projectless parent");

    let error = crate::stage_tools::start_full_core(
        &env.state,
        "parent",
        json!({ "role": "implementer", "prompt": "do it" }),
    )
    .await
    .expect_err("spawn must fail without a project");
    assert_eq!(error.code, "project_required");

    // The stage row exists but is failed (not running), so the guard releases.
    assert!(!env
        .state
        .repo
        .parent_has_running_stage("parent")
        .await
        .expect("running stage check"));
    let stages = env
        .state
        .repo
        .list_parent_stages("parent")
        .await
        .expect("list stages");
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].status, StageStatus::Failed);

    env.cleanup().await;
}

// --- Phase-3 adversarial-review regression tests ---

/// Count parent-visible `subagent.idle` rows in the parent's durable event log.
async fn parent_idle_rows(env: &TestEnv, parent_id: &str) -> usize {
    env.state
        .repo
        .events_after(parent_id, None)
        .await
        .expect("parent events")
        .into_iter()
        .filter(|event| event.event == EventType::SubagentIdle)
        .count()
}

/// FIX A: a fan-out whose subagent #1 is terminal while #2 has not yet been
/// inserted must NOT complete — the expected-count fence keeps the barrier shut.
#[tokio::test]
async fn partial_spawn_does_not_complete_stage() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "barrier test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    // The fan-out will spawn TWO subagents.
    let stage = env
        .state
        .repo
        .create_stage("parent", StageKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create stage");
    // Only subagent #1 exists so far and is terminal-on-arrival.
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "first",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "fast review done",
    )
    .await;

    // The barrier must not fire while the sibling is still unspawned.
    complete_stage_if_ready(&env.state, &stage.id)
        .await
        .expect("barrier (partial spawn)");
    assert_eq!(
        env.state
            .repo
            .get_stage(&stage.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        StageStatus::Running
    );
    assert_eq!(steers_to_parent(&env, "parent", &stage.id).await, 0);

    // The sibling arrives terminal too; now the full set exists -> one steer.
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "second",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "second review done",
    )
    .await;
    complete_stage_if_ready(&env.state, &stage.id)
        .await
        .expect("barrier (full set)");
    assert_eq!(
        env.state
            .repo
            .get_stage(&stage.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        StageStatus::Done
    );
    assert_eq!(steers_to_parent(&env, "parent", &stage.id).await, 1);

    env.cleanup().await;
}

/// FIX B: simulate a crash after the finish_stage commit (the parent is never
/// driven). The steer must be durably queued, and a re-run (boot sweep) must not
/// enqueue a second one.
#[tokio::test]
async fn steer_is_durable_after_finish_and_not_double_enqueued() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "barrier test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let stage = env
        .state
        .repo
        .create_stage("parent", StageKind::Full, None, None, 1)
        .await
        .expect("create stage");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "impl",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "implemented",
    )
    .await;

    // Run the CAS + atomic steer-enqueue directly (the same call the runner makes),
    // WITHOUT driving the parent — i.e. a crash in the old gap. The steer is part
    // of the committed CAS tx, so it is durably queued.
    let key = format!("stage-steer:{}:{}", stage.id, stage.attempt_id);
    assert!(env
        .state
        .repo
        .finish_stage(
            &stage.id,
            &stage.attempt_id,
            StageStatus::Done,
            "parent",
            "stage finished",
            &key
        )
        .await
        .expect("finish wins"));
    let durable = env
        .state
        .repo
        .find_client_input("parent", &key)
        .await
        .expect("find steer");
    assert!(
        durable.is_some(),
        "steer must be durably queued after the CAS commit"
    );

    // Re-open the stage and re-run with the same deterministic key (a replay /
    // boot sweep racing the original winner): the CAS wins again, but the steer
    // insert is a no-op on the unique (session_id, client_input_id) index, so no
    // second steer is queued. The parent's queue still holds exactly one steer.
    env.state
        .repo
        .set_stage_status(&stage.id, StageStatus::Running)
        .await
        .expect("reopen");
    assert!(env
        .state
        .repo
        .finish_stage(
            &stage.id,
            &stage.attempt_id,
            StageStatus::Done,
            "parent",
            "stage finished",
            &key
        )
        .await
        .expect("replay CAS wins again"));
    let queued_steers = env
        .state
        .repo
        .queue_state("parent")
        .await
        .expect("queue state")
        .queued_inputs
        .into_iter()
        .filter(|input| input.priority == InputPriority::Steer)
        .count();
    assert_eq!(
        queued_steers, 1,
        "exactly one durable steer, no double-enqueue"
    );

    env.cleanup().await;
}

/// FIX C: a stage subagent at a NON-boundary leaf (mid-turn) with its action
/// stale-marked (as the boot stale-mark does) and no queued input must NOT cause
/// the boot sweep to complete/steer the stage.
#[tokio::test]
async fn boot_sweep_does_not_complete_mid_turn_subagent() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "barrier test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let stage = env
        .state
        .repo
        .create_stage("parent", StageKind::Full, None, None, 1)
        .await
        .expect("create stage");
    // A single full subagent stuck mid-turn (active leaf is an assistant message,
    // not a boundary). create_running_subagent leaves it at the non-boundary leaf.
    create_running_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "mid_turn",
        "implementer",
        TurnOutcome::Graceful,
    )
    .await;
    // Emulate the boot stale-mark that erases any unfinished action globally, so
    // action/queue status alone would (wrongly) look terminal.
    env.state
        .repo
        .mark_all_unfinished_actions_stale()
        .await
        .expect("stale-mark");

    // The boot sweep must NOT complete this stage: terminality is transcript-based,
    // and a mid-turn leaf is not a boundary.
    sweep_running_stages_on_boot(&env.state).await;
    assert_eq!(
        env.state
            .repo
            .get_stage(&stage.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        StageStatus::Running
    );
    assert_eq!(steers_to_parent(&env, "parent", &stage.id).await, 0);

    env.cleanup().await;
}

/// FIX D: a terminal stage member produces ZERO parent-visible `subagent.idle`
/// rows, yet the single stage steer is still delivered (and the once-gate fired).
/// Driven through the LIVE seam (`handle_subagent_terminal_for_parent_if_needed`), which
/// is FIX F's live-seam coverage for the suppression path.
#[tokio::test]
async fn terminal_stage_member_yields_zero_parent_idle_rows() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "barrier test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let stage = env
        .state
        .repo
        .create_stage("parent", StageKind::Full, None, None, 1)
        .await
        .expect("create stage");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "member",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "done",
    )
    .await;

    // Drive the LIVE idle seam for the stage member.
    let driver = SessionDriver::acquire(&env.state, "member").await;
    driver.handle_subagent_terminal_for_parent_if_needed().await;

    // Zero per-child idle surfaced to the parent...
    assert_eq!(parent_idle_rows(&env, "parent").await, 0);
    // ...yet the stage completed and the single steer was delivered.
    assert_eq!(
        env.state
            .repo
            .get_stage(&stage.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        StageStatus::Done
    );
    assert_eq!(steers_to_parent(&env, "parent", &stage.id).await, 1);

    env.cleanup().await;
}

/// Server-side guard: a `Steer`-priority input targeting a `read_only` subagent
/// is rejected with `cannot_steer_read_only_subagent`, while a follow-up to the
/// same subagent and a steer to a full subagent are both accepted.
#[tokio::test]
async fn steering_a_read_only_subagent_is_rejected_server_side() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "steer guard test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let stage = env
        .state
        .repo
        .create_stage("parent", StageKind::Full, None, None, 2)
        .await
        .expect("create stage");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "ro",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "done",
    )
    .await;
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "full",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "done",
    )
    .await;

    let steer = |session_id: &str| {
        json!({
            "session_id": session_id,
            "priority": "steer",
            "content": [{ "type": "text", "text": "stop" }],
        })
    };

    // Steering the read-only subagent is rejected by the server guard.
    let rejected = crate::input_user(&env.state, steer("ro"))
        .await
        .expect_err("steering a read_only subagent must be rejected");
    assert_eq!(rejected.code, "cannot_steer_read_only_subagent");

    // Steering the full subagent is accepted (only read_only is guarded).
    crate::input_user(&env.state, steer("full"))
        .await
        .expect("steering a full subagent is allowed");

    // A follow-up to the read-only subagent is unaffected by the steer guard.
    crate::input_user(
        &env.state,
        json!({
            "session_id": "ro",
            "priority": "follow_up",
            "content": [{ "type": "text", "text": "fyi" }],
        }),
    )
    .await
    .expect("a follow-up to a read_only subagent is allowed");

    env.cleanup().await;
}

/// FIX E: a stage member whose initial dispatch fails produces no parent-visible
/// `subagent.idle`. We exercise the spawn path with a provider error by spawning
/// into a stage from a parent that will fail dispatch; the dispatch-failed
/// notifier must short-circuit for a child that has a stage_id.
#[tokio::test]
async fn dispatch_failure_for_stage_member_emits_no_parent_idle() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "barrier test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let stage = env
        .state
        .repo
        .create_stage("parent", StageKind::ReadonlyFanout, None, None, 1)
        .await
        .expect("create stage");
    // A terminal stage member already exists with a stage_id; route a simulated
    // dispatch failure for it through the gate. The gate must suppress the
    // parent-visible idle because the child belongs to a stage.
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "ro_member",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "n/a",
    )
    .await;

    crate::subagents::publish_subagent_parent_dispatch_failed_event_for_test(
        &env.state,
        "parent",
        "ro_member",
        "reviewer",
    )
    .await;

    assert_eq!(parent_idle_rows(&env, "parent").await, 0);

    env.cleanup().await;
}

/// FIX F: two sibling stage members reaching idle through the LIVE seam — one
/// triggering recovery of the other — steer the parent EXACTLY once, and neither
/// surfaces a per-child idle.
#[tokio::test]
async fn two_siblings_steer_parent_exactly_once_via_live_seam() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "barrier test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let stage = env
        .state
        .repo
        .create_stage("parent", StageKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create stage");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "sib_a",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "a done",
    )
    .await;
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &stage.id,
        "sib_b",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "b done",
    )
    .await;

    // Both siblings fire their live idle seam (the order does not matter; the DB
    // CAS single-flights the completion). The recursive recover->barrier->recover
    // cycle terminates because the second barrier short-circuits on a non-running
    // stage.
    SessionDriver::acquire(&env.state, "sib_a")
        .await
        .handle_subagent_terminal_for_parent_if_needed()
        .await;
    SessionDriver::acquire(&env.state, "sib_b")
        .await
        .handle_subagent_terminal_for_parent_if_needed()
        .await;

    assert_eq!(parent_idle_rows(&env, "parent").await, 0);
    assert_eq!(
        env.state
            .repo
            .get_stage(&stage.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        StageStatus::Done
    );
    assert_eq!(steers_to_parent(&env, "parent", &stage.id).await, 1);

    env.cleanup().await;
}
