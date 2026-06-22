//! Deterministic delegation barrier / handoff / steer tests against a real Postgres.
//!
//! These drive the barrier directly (the live lifecycle hook and the boot
//! sweep both funnel through `complete_delegation_if_ready`), with subagents placed
//! into terminal/running states by writing their durable transcripts directly,
//! so the tests are fully deterministic and need no provider.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use agent_session::{AgentSession, ModelContext, SessionAction, TranscriptStorageNode};
use agent_store::{
    DelegationKind, DelegationStatus, EventType, InputPriority, PostgresAgentStore,
    QueuedInputStatus, SessionConfig, SubagentType,
};
use agent_tools::ToolRegistry;
use agent_vocab::{
    ActionId, AssistantItem, AssistantMessage, ProviderConfig, ProviderKind, ReasoningEffort,
    ToolCall, ToolCallId, TranscriptItem, TurnId, TurnOutcome, UserMessage,
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
use crate::types::RuntimeSession;
use crate::workspaces::WorkspaceManager;

use super::{
    complete_delegation_if_ready, sweep_running_delegations_on_boot,
    try_claim_and_publish_completed_delegation,
};
use crate::delegation_tools::{
    cancel_core, read_handoff_file_core, run_delegation_tool, status_core, steer_subagent_core,
};

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

/// Create a delegation subagent whose durable transcript carries one assistant turn
/// finished with `outcome`, then settle it terminal (no queued input, no
/// unfinished action) so the all-terminal predicate sees it as done.
// Test fixture: each argument shapes a distinct field of the subagent transcript.
#[allow(clippy::too_many_arguments)]
async fn create_terminal_subagent(
    env: &TestEnv,
    project_id: Uuid,
    parent_id: &str,
    delegation_id: &str,
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
            Some(delegation_id),
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
    delegation_id: &str,
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
            Some(delegation_id),
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

/// A full subagent with an active, unfinished model action. This keeps the
/// session genuinely busy so a steer-priority input should be queued rather
/// than immediately consumed into a new turn.
async fn create_busy_full_subagent(
    env: &TestEnv,
    project_id: Uuid,
    parent_id: &str,
    delegation_id: &str,
    session_id: &str,
) {
    let active_leaf = format!("{session_id}_a");
    let entries = vec![
        TranscriptStorageNode {
            id: format!("{session_id}_u"),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::UserMessage(UserMessage::text("keep working")),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: active_leaf.clone(),
            parent_id: Some(format!("{session_id}_u")),
            timestamp_ms: 2,
            item: TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("working...".to_string())],
            }),
            provider_replay: Vec::new(),
        },
    ];
    let actions = vec![SessionAction::RequestModel {
        action_id: ActionId(1),
        turn_id: TurnId(1),
        model_context: ModelContext::new(),
        context_leaf_id: Some(active_leaf.clone()),
    }];
    env.state
        .repo
        .start_session_outputs_with_parent(
            session_id,
            &session_config(
                env,
                project_id,
                json!({ "created_by": "test", "role_name": "implementer", "harness": true }),
            ),
            &entries,
            Some(&active_leaf),
            &[],
            &actions,
            InputPriority::FollowUp,
            &UserMessage::text("keep working"),
            None,
            Some(parent_id),
            Some(SubagentType::Full),
            Some(delegation_id),
        )
        .await
        .expect("create busy full subagent");
    assert_eq!(
        env.state.repo.activity(session_id).await.expect("activity"),
        agent_store::SessionActivity::Running
    );
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

/// Completion steer texts that reached the parent. An idle parent accepts the
/// steer as its next user-message turn, so the steer lands in the parent's
/// transcript; we collect user messages naming the delegation.
async fn parent_completion_steer_texts(
    env: &TestEnv,
    parent_id: &str,
    delegation_id: &str,
) -> Vec<String> {
    let history = env
        .state
        .repo
        .active_branch(parent_id)
        .await
        .expect("parent active branch");
    history
        .entries
        .iter()
        .filter_map(|entry| match &entry.item {
            TranscriptItem::UserMessage(message) => message.as_text().and_then(|text| {
                (text.contains(delegation_id) && text.contains("completed"))
                    .then(|| text.to_string())
            }),
            _ => None,
        })
        .collect()
}

async fn steers_to_parent(env: &TestEnv, parent_id: &str, delegation_id: &str) -> usize {
    parent_completion_steer_texts(env, parent_id, delegation_id)
        .await
        .len()
}

fn wakeup_snapshot_from_text(text: &str) -> serde_json::Value {
    let start = text
        .find("```json\n")
        .map(|index| index + "```json\n".len())
        .expect("wakeup has json fence");
    let rest = &text[start..];
    let end = rest.find("\n```").expect("wakeup json fence closes");
    serde_json::from_str(&rest[..end]).expect("wakeup snapshot is valid json")
}

async fn parent_completion_snapshot(
    env: &TestEnv,
    parent_id: &str,
    delegation_id: &str,
) -> serde_json::Value {
    let texts = parent_completion_steer_texts(env, parent_id, delegation_id).await;
    assert_eq!(texts.len(), 1, "expected exactly one completion wakeup");
    wakeup_snapshot_from_text(&texts[0])
}

async fn inspect_delegation_snapshot(env: &TestEnv, delegation_id: &str) -> serde_json::Value {
    status_core(
        &env.state,
        "parent",
        json!({ "delegation_id": delegation_id }),
    )
    .await
    .expect("inspect delegation")
}

fn handoff_root(env: &TestEnv, delegation_id: &str) -> PathBuf {
    env.cwd.path().join(".pi-handoff").join(delegation_id)
}

#[tokio::test]
async fn model_facing_steer_subagent_queues_steer_for_running_full_subagent() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "steer test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("impl"), 1)
        .await
        .expect("create delegation");
    create_busy_full_subagent(&env, project_id, "parent", &delegation.id, "impl_busy").await;

    let tool_result = run_delegation_tool(
        &env.state,
        "parent",
        &ToolCall {
            id: ToolCallId::new("call_steer"),
            tool_name: "steer_subagent".to_string(),
            args_json: json!({
                "subagent_id": "impl_busy",
                "message": "Please also update the docs."
            })
            .to_string(),
        },
    )
    .await;
    assert_eq!(tool_result.status, agent_vocab::ToolResultStatus::Success);
    let output: serde_json::Value =
        serde_json::from_str(&tool_result.output).expect("tool output JSON");
    assert_eq!(output["subagent_id"], "impl_busy");
    assert_eq!(output["queued"], true);
    assert!(output["input_id"].as_str().is_some());

    let queue = env
        .state
        .repo
        .queue_state("impl_busy")
        .await
        .expect("queue state");
    assert_eq!(queue.queued_inputs.len(), 1);
    let queued = &queue.queued_inputs[0];
    assert_eq!(queued.priority, InputPriority::Steer);
    assert_eq!(queued.status, QueuedInputStatus::Queued);
    assert_eq!(
        queued.content.as_text(),
        Some("Please also update the docs.")
    );

    env.cleanup().await;
}

#[tokio::test]
async fn steer_subagent_rejects_read_only_subagents() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "steer test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 1)
        .await
        .expect("create delegation");
    let _boundary = create_running_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "readonly_running",
        "reviewer",
        TurnOutcome::Graceful,
    )
    .await;

    let tool_result = run_delegation_tool(
        &env.state,
        "parent",
        &ToolCall {
            id: ToolCallId::new("call_readonly"),
            tool_name: "steer_subagent".to_string(),
            args_json: json!({
                "subagent_id": "readonly_running",
                "message": "Please check one more file."
            })
            .to_string(),
        },
    )
    .await;
    assert_eq!(tool_result.status, agent_vocab::ToolResultStatus::Error);
    assert!(tool_result
        .output
        .contains("cannot_steer_read_only_subagent"));
    assert!(env
        .state
        .repo
        .queue_state("readonly_running")
        .await
        .expect("queue state")
        .queued_inputs
        .is_empty());

    env.cleanup().await;
}

#[tokio::test]
async fn steer_subagent_rejects_idle_nonterminal_subagent_without_reactivating_it() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "steer test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    let active_leaf = "impl_idle_a";
    let entries = vec![
        TranscriptStorageNode {
            id: "impl_idle_u".to_string(),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::UserMessage(UserMessage::text("keep working")),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: active_leaf.to_string(),
            parent_id: Some("impl_idle_u".to_string()),
            timestamp_ms: 2,
            item: TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("mid turn".to_string())],
            }),
            provider_replay: Vec::new(),
        },
    ];
    env.state
        .repo
        .start_session_outputs_with_parent(
            "impl_idle",
            &session_config(
                &env,
                project_id,
                json!({ "created_by": "test", "role_name": "implementer" }),
            ),
            &entries,
            Some(active_leaf),
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("keep working"),
            None,
            Some("parent"),
            Some(SubagentType::Full),
            Some(&delegation.id),
        )
        .await
        .expect("create idle nonterminal subagent");
    assert!(!env
        .state
        .repo
        .active_leaf_is_turn_boundary("impl_idle")
        .await
        .expect("nonterminal"));
    assert_eq!(
        env.state
            .repo
            .activity("impl_idle")
            .await
            .expect("activity"),
        agent_store::SessionActivity::Idle
    );

    let error = steer_subagent_core(
        &env.state,
        "parent",
        json!({ "subagent_id": "impl_idle", "message": "one more thing" }),
    )
    .await
    .expect_err("idle nonterminal subagent rejected");
    assert_eq!(error.code, "subagent_not_running");
    assert!(env
        .state
        .repo
        .queue_state("impl_idle")
        .await
        .expect("queue")
        .queued_inputs
        .is_empty());

    env.cleanup().await;
}

#[tokio::test]
async fn steer_subagent_rejects_terminal_or_cancelled_delegations() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "steer test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;

    let done = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create done delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &done.id,
        "impl_done",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "Done.",
    )
    .await;
    env.state
        .repo
        .set_delegation_status(&done.id, DelegationStatus::Done)
        .await
        .expect("mark done");
    let tool_result = run_delegation_tool(
        &env.state,
        "parent",
        &ToolCall {
            id: ToolCallId::new("call_done"),
            tool_name: "steer_subagent".to_string(),
            args_json: json!({ "subagent_id": "impl_done", "message": "one more thing" })
                .to_string(),
        },
    )
    .await;
    assert_eq!(tool_result.status, agent_vocab::ToolResultStatus::Error);
    assert!(tool_result.output.contains("delegation_not_running"));

    let cancelled = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create cancelled delegation");
    create_busy_full_subagent(&env, project_id, "parent", &cancelled.id, "impl_cancelled").await;
    env.state
        .repo
        .set_delegation_status(&cancelled.id, DelegationStatus::Cancelled)
        .await
        .expect("mark cancelled");
    let tool_result = run_delegation_tool(
        &env.state,
        "parent",
        &ToolCall {
            id: ToolCallId::new("call_cancelled"),
            tool_name: "steer_subagent".to_string(),
            args_json: json!({ "subagent_id": "impl_cancelled", "message": "one more thing" })
                .to_string(),
        },
    )
    .await;
    assert_eq!(tool_result.status, agent_vocab::ToolResultStatus::Error);
    assert!(tool_result.output.contains("delegation_not_running"));

    env.cleanup().await;
}

#[tokio::test]
async fn cancel_delegation_returns_transcript_only_paths() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "cancel test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("impl"), 1)
        .await
        .expect("create delegation");
    create_busy_full_subagent(&env, project_id, "parent", &delegation.id, "impl_to_cancel").await;
    env.state
        .repo
        .enqueue_user_input(
            "impl_to_cancel",
            InputPriority::FollowUp,
            &UserMessage::text("queued work that must not run after cancellation"),
            Some("queued-before-cancel"),
        )
        .await
        .expect("queue follow-up before cancellation");

    let result = cancel_core(
        &env.state,
        "parent",
        json!({ "delegation_id": delegation.id }),
    )
    .await
    .expect("cancel delegation");
    assert_eq!(result["cancelled"], true);
    let transcripts = result["transcripts"].as_array().expect("transcripts array");
    assert_eq!(transcripts.len(), 1);
    assert_eq!(transcripts[0]["subagent_id"], "impl_to_cancel");
    let transcript_path = transcripts[0]["transcript"]
        .as_str()
        .expect("transcript path");
    assert!(
        transcript_path.ends_with(&format!(
            ".pi-handoff/{}/cancelled/impl_to_cancel.transcript.md",
            delegation.id
        )),
        "unexpected transcript path: {transcript_path}"
    );
    let transcript = std::fs::read_to_string(transcript_path).expect("transcript readable");
    assert!(transcript.contains("## User"));
    assert!(transcript.contains("keep working"));
    assert!(transcript.contains("## Assistant"));
    assert!(transcript.contains("working..."));
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    let subagent = snapshot["subagents"].as_array().unwrap()[0].clone();
    assert_eq!(snapshot["status"], "cancelled");
    assert_eq!(subagent["status"], "cancelled");
    assert_eq!(subagent["final_message"], serde_json::Value::Null);
    assert_eq!(subagent["final_message_path"], serde_json::Value::Null);
    assert_eq!(
        subagent["final_message_relative_path"],
        serde_json::Value::Null
    );
    assert_eq!(subagent["final_message_file"], serde_json::Value::Null);
    assert_eq!(subagent["transcript_path"], serde_json::Value::Null);
    assert_eq!(
        subagent["transcript_relative_path"],
        serde_json::Value::Null
    );
    assert_eq!(subagent["transcript_file"], serde_json::Value::Null);
    assert_eq!(subagent["cancellation_transcript_path"], transcript_path);
    assert_eq!(
        subagent["cancellation_transcript_relative_path"],
        format!("cancelled/{}.transcript.md", "impl_to_cancel")
    );
    assert_eq!(snapshot["progress"]["running"], 0);
    assert!(!handoff_root(&env, &delegation.id)
        .join("index.json")
        .exists());
    assert!(!env
        .cwd
        .path()
        .join(".pi-handoff")
        .join(&delegation.id)
        .join("impl_to_cancel")
        .join("final_message.md")
        .exists());
    assert!(!env
        .cwd
        .path()
        .join(".pi-handoff")
        .join(&delegation.id)
        .join("impl_to_cancel")
        .join("transcript.md")
        .exists());
    let normal_read = read_handoff_file_core(
        &env.state,
        "parent",
        json!({
            "delegation_id": delegation.id,
            "subagent_id": "impl_to_cancel",
            "file": "transcript.md",
        }),
    )
    .await
    .expect_err("normal transcript read rejected for cancellation");
    assert_eq!(normal_read.code, "handoff_file_not_found");
    assert!(!env
        .cwd
        .path()
        .join(".pi-handoff")
        .join(&delegation.id)
        .join("impl_to_cancel")
        .join("transcript.md")
        .exists());
    let cancellation_read = read_handoff_file_core(
        &env.state,
        "parent",
        json!({
            "delegation_id": delegation.id,
            "file": "cancelled/impl_to_cancel.transcript.md",
        }),
    )
    .await
    .expect("cancelled transcript readable");
    assert_eq!(cancellation_read["subagent_id"], "impl_to_cancel");
    assert_eq!(
        cancellation_read["file"],
        "cancelled/impl_to_cancel.transcript.md"
    );
    assert!(cancellation_read["content"]
        .as_str()
        .expect("cancelled transcript content")
        .contains("working..."));
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Cancelled
    );
    let queue = env
        .state
        .repo
        .queue_state("impl_to_cancel")
        .await
        .expect("queue state after cancellation");
    assert_eq!(queue.queued_inputs.len(), 1);
    assert_eq!(queue.queued_inputs[0].status, QueuedInputStatus::Queued);
    assert_eq!(
        queue.queued_inputs[0].content.as_text(),
        Some("queued work that must not run after cancellation")
    );

    env.cleanup().await;
}

#[tokio::test]
async fn cancel_delegation_does_not_clobber_completed_delegation_or_write_artifacts() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "cancel race test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("impl"), 1)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "impl_done_before_cancel",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "Done.",
    )
    .await;

    assert!(env
        .state
        .repo
        .finish_delegation(
            &delegation.id,
            &delegation.attempt_id,
            DelegationStatus::DoneWithFailures,
        )
        .await
        .expect("simulate completion winning first"));
    let result = cancel_core(
        &env.state,
        "parent",
        json!({ "delegation_id": delegation.id }),
    )
    .await
    .expect("cancel after completion");
    assert_eq!(result, json!({ "cancelled": false }));
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::DoneWithFailures
    );
    let handoff_root = handoff_root(&env, &delegation.id);
    assert!(
        !handoff_root.join("cancelled").exists(),
        "cancel-loser must not write transcript-only artifacts"
    );
    assert!(
        !handoff_root.join("index.json").exists(),
        "direct status completion did not publish normal handoff either"
    );

    env.cleanup().await;
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
    let delegation = env
        .state
        .repo
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            Some("implement_review_test"),
            Some("review"),
            2,
        )
        .await
        .expect("create delegation");

    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
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
        &delegation.id,
        "still_running",
        "reviewer",
        TurnOutcome::Crashed,
    )
    .await;

    // Not all subagents terminal yet (the second is mid-turn) -> barrier must not
    // fire. Recovery of an idle mid-turn subagent leaves it at its non-boundary
    // leaf, so it stays non-terminal.
    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("barrier (partial)");
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Running
    );
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 0);

    // Settle the second subagent terminal at a Crashed boundary — the barrier
    // classifies a non-graceful TurnFinished as a failure, exactly as a child
    // that died mid-task and was recovered to a boundary would appear.
    settle_subagent_terminal(&env, "still_running", &boundary_leaf).await;

    // Now all terminal -> exactly one steer, done_with_failures, handoff for all.
    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("barrier (complete)");
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::DoneWithFailures
    );
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 1);

    // Re-delivered events must not double-steer (idempotent via the CAS).
    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("barrier (replay)");
    sweep_running_delegations_on_boot(&env.state).await;
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 1);

    // Handoff: inspect_delegation is the manifest-equivalent snapshot; the
    // handoff dir contains per-subagent files for EVERY subagent (incl. failed)
    // but no delegation-root index.json.
    let root = handoff_root(&env, &delegation.id);
    assert!(!root.join("index.json").exists());
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    let wakeup_texts = parent_completion_steer_texts(&env, "parent", &delegation.id).await;
    assert_eq!(wakeup_texts.len(), 1);
    let wakeup_text = &wakeup_texts[0];
    assert!(
        !wakeup_text.contains("index.json"),
        "completion wakeup must not instruct the parent to read index.json"
    );
    assert!(
        wakeup_text.contains("Snapshot JSON"),
        "completion wakeup should include the structured snapshot JSON"
    );
    assert!(
        wakeup_text.contains("Full transcript contents are not inlined"),
        "completion wakeup should clarify transcripts are artifact paths only"
    );
    assert!(
        !wakeup_text.contains("## User"),
        "completion wakeup must not inline transcript markdown"
    );
    let wakeup_snapshot = wakeup_snapshot_from_text(wakeup_text);
    assert_eq!(wakeup_snapshot, snapshot);
    assert_eq!(snapshot["status"], "done_with_failures");
    assert_eq!(snapshot["kind"], "readonly_fanout");
    assert_eq!(snapshot["workflow"], "implement_review_test");
    assert_eq!(snapshot["label"], "review");
    assert_eq!(snapshot["handoff_dir"], root.to_string_lossy().as_ref());
    assert_eq!(snapshot["progress"]["expected"], 2);
    assert_eq!(snapshot["progress"]["spawned"], 2);
    assert_eq!(snapshot["progress"]["terminal"], 2);
    assert_eq!(snapshot["progress"]["running"], 0);
    assert_eq!(snapshot["progress"]["failed"], 1);
    let subagents = snapshot["subagents"].as_array().expect("subagents array");
    assert_eq!(subagents.len(), 2);
    for subagent in subagents {
        let id = subagent["id"].as_str().unwrap();
        let base = env
            .cwd
            .path()
            .join(".pi-handoff")
            .join(&delegation.id)
            .join(id);
        assert!(
            base.join("final_message.md").exists(),
            "final_message for {id}"
        );
        assert!(base.join("transcript.md").exists(), "transcript for {id}");
        assert_eq!(
            subagent["final_message_path"],
            base.join("final_message.md").to_string_lossy().as_ref()
        );
        assert_eq!(
            subagent["transcript_path"],
            base.join("transcript.md").to_string_lossy().as_ref()
        );
        assert_eq!(
            subagent["final_message_file"],
            format!("{id}/final_message.md")
        );
        assert_eq!(subagent["transcript_file"], format!("{id}/transcript.md"));
    }
    let ok = subagents.iter().find(|s| s["id"] == "ok_a").unwrap();
    assert_eq!(ok["role"], "reviewer");
    assert_eq!(ok["type"], "read_only");
    assert_eq!(ok["subagent_type"], "read_only");
    assert_eq!(ok["status"], "done");
    assert_eq!(ok["final_message"], "All good.\n\nsuggested_next: approved");
    assert_eq!(ok["suggested_next"], "approved");
    let wakeup_ok = wakeup_snapshot["subagents"]
        .as_array()
        .expect("wakeup subagents array")
        .iter()
        .find(|subagent| subagent["id"] == "ok_a")
        .expect("ok_a in wakeup snapshot");
    assert_eq!(
        wakeup_ok["final_message"],
        "All good.\n\nsuggested_next: approved"
    );
    assert_eq!(wakeup_ok["suggested_next"], "approved");
    assert!(wakeup_ok["transcript_path"]
        .as_str()
        .expect("transcript path in wakeup snapshot")
        .ends_with("ok_a/transcript.md"));
    assert_eq!(ok["steerable"], false);
    let failed = subagents
        .iter()
        .find(|s| s["id"] == "still_running")
        .unwrap();
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["suggested_next"], serde_json::Value::Null);

    env.cleanup().await;
}

#[tokio::test]
async fn inspect_delegation_refreshes_artifacts_from_postgres() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "inspect refresh test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            Some("explore"),
            None,
            2,
        )
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "done_child",
        "explorer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "Found the answer.\n\nsuggested_next: done",
    )
    .await;
    create_running_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "running_child",
        "explorer",
        TurnOutcome::Graceful,
    )
    .await;

    let root = handoff_root(&env, &delegation.id);
    assert!(
        !root.exists(),
        "inspection should be the first artifact writer in this test"
    );
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    assert_eq!(snapshot["status"], "running");
    assert_eq!(snapshot["progress"]["expected"], 2);
    assert_eq!(snapshot["progress"]["spawned"], 2);
    assert_eq!(snapshot["progress"]["terminal"], 1);
    assert_eq!(snapshot["progress"]["running"], 1);
    assert_eq!(snapshot["progress"]["failed"], 0);
    assert!(!root.join("index.json").exists());

    let done = snapshot["subagents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|subagent| subagent["id"] == "done_child")
        .unwrap();
    assert_eq!(done["status"], "done");
    assert_eq!(
        done["final_message"],
        "Found the answer.\n\nsuggested_next: done"
    );
    assert_eq!(done["suggested_next"], "done");
    assert_eq!(done["final_message_path"], serde_json::Value::Null);
    assert!(
        !root.join("done_child").join("final_message.md").exists(),
        "normal final_message artifact waits for delegation completion"
    );
    assert!(
        std::fs::read_to_string(root.join("done_child").join("transcript.md"))
            .expect("terminal transcript artifact")
            .contains("Found the answer.")
    );

    let running = snapshot["subagents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|subagent| subagent["id"] == "running_child")
        .unwrap();
    assert_eq!(running["activity"], "idle");
    assert_eq!(running["status"], "running");
    assert_eq!(running["final_message"], serde_json::Value::Null);
    assert_eq!(running["suggested_next"], serde_json::Value::Null);
    assert_eq!(running["final_message_path"], serde_json::Value::Null);
    assert!(root.join("running_child").join("transcript.md").exists());
    assert!(
        !root.join("running_child").join("final_message.md").exists(),
        "mid-turn child should not get a premature final_message artifact"
    );

    // Mutate the stale artifact on disk; a later inspect must refresh it from
    // the durable Postgres transcript before returning the file path.
    std::fs::write(
        root.join("done_child").join("transcript.md"),
        "stale local artifact",
    )
    .expect("overwrite transcript artifact");
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    assert_eq!(snapshot["progress"]["terminal"], 1);
    let refreshed = std::fs::read_to_string(root.join("done_child").join("transcript.md")).unwrap();
    assert!(refreshed.contains("Found the answer."));
    assert!(!refreshed.contains("stale local artifact"));

    env.cleanup().await;
}

#[tokio::test]
async fn failed_delegation_does_not_publish_normal_handoff_on_inspect_or_read() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "failed inspect test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("impl"), 1)
        .await
        .expect("create delegation");
    create_busy_full_subagent(&env, project_id, "parent", &delegation.id, "impl_failed").await;
    env.state
        .repo
        .set_delegation_status(&delegation.id, DelegationStatus::Failed)
        .await
        .expect("mark failed");

    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    let subagent = snapshot["subagents"].as_array().unwrap()[0].clone();
    assert_eq!(snapshot["status"], "failed");
    assert_eq!(subagent["status"], "failed");
    assert_eq!(subagent["final_message"], serde_json::Value::Null);
    assert_eq!(subagent["final_message_path"], serde_json::Value::Null);
    assert_eq!(subagent["transcript_path"], serde_json::Value::Null);
    let root = handoff_root(&env, &delegation.id);
    assert!(
        !root.join("impl_failed").join("transcript.md").exists(),
        "failed inspection must not create normal transcript artifacts"
    );
    let error = read_handoff_file_core(
        &env.state,
        "parent",
        json!({
            "delegation_id": delegation.id,
            "subagent_id": "impl_failed",
            "file": "transcript.md",
        }),
    )
    .await
    .expect_err("failed delegation normal read rejected");
    assert_eq!(error.code, "handoff_file_not_found");
    assert!(
        !root.join("impl_failed").join("transcript.md").exists(),
        "failed read must not create normal transcript artifacts"
    );

    env.cleanup().await;
}

#[tokio::test]
async fn completion_loser_after_cancellation_does_not_write_normal_handoff() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "completion cancel race test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("impl"), 1)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "impl_cancel_wins",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "Done.",
    )
    .await;

    // Simulate the barrier having already loaded this Running delegation and
    // classified its terminal subagent. Cancellation wins before the completion
    // status CAS, so the completion path must return false and publish no
    // normal handoff artifacts.
    assert!(env
        .state
        .repo
        .cancel_running_delegation(&delegation.id, &delegation.attempt_id)
        .await
        .expect("cancellation wins"));
    let won_completion =
        try_claim_and_publish_completed_delegation(&env.state, &delegation, DelegationStatus::Done)
            .await
            .expect("completion loser returns cleanly");
    assert!(!won_completion);
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Cancelled
    );
    let handoff_root = handoff_root(&env, &delegation.id);
    assert!(!handoff_root.join("index.json").exists());
    assert!(!handoff_root.join("impl_cancel_wins").exists());
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 0);

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
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "impl",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "Done.\nsuggested_next: ship_it_immediately",
    )
    .await;

    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("barrier");
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    assert_eq!(snapshot["status"], "done");
    assert_eq!(
        snapshot["subagents"][0]["suggested_next"],
        "ship_it_immediately"
    );

    env.cleanup().await;
}

#[tokio::test]
async fn stale_attempt_id_cannot_finish_delegation() {
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
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");

    // The real attempt_id wins exactly once.
    assert!(env
        .state
        .repo
        .finish_delegation(
            &delegation.id,
            &delegation.attempt_id,
            DelegationStatus::Done
        )
        .await
        .expect("finish"));
    // A second call with the same id is a no-op (status no longer running).
    assert!(!env
        .state
        .repo
        .finish_delegation(
            &delegation.id,
            &delegation.attempt_id,
            DelegationStatus::Done
        )
        .await
        .expect("finish again"));

    // Re-open it and try a stale attempt id: must not transition.
    env.state
        .repo
        .set_delegation_status(&delegation.id, DelegationStatus::Running)
        .await
        .expect("reopen");
    assert!(!env
        .state
        .repo
        .finish_delegation(&delegation.id, "stale-attempt-id", DelegationStatus::Done)
        .await
        .expect("stale finish"));
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Running
    );

    env.cleanup().await;
}

#[tokio::test]
async fn boot_sweep_completes_a_crash_mid_barrier_delegation_exactly_once() {
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
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "impl",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "Implemented.",
    )
    .await;

    // The delegation is still `running` with all subagents terminal — i.e. a crash
    // mid-barrier. The boot sweep completes it exactly once.
    sweep_running_delegations_on_boot(&env.state).await;
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Done
    );
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 1);

    // A second sweep (another restart) must not double-steer.
    sweep_running_delegations_on_boot(&env.state).await;
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 1);

    env.cleanup().await;
}

// --- Phase-2 guard tests (deferred until this harness existed) ---

#[tokio::test]
async fn one_delegation_per_parent_is_rejected() {
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
    // A running delegation already exists for this parent.
    env.state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");

    let error = crate::delegation_tools::start_full_core(
        &env.state,
        "parent",
        json!({ "role": "implementer", "prompt": "do it" }),
    )
    .await
    .expect_err("second delegation must be rejected");
    assert_eq!(error.code, "delegation_already_running");

    env.cleanup().await;
}

#[tokio::test]
async fn subagent_cannot_start_a_nested_delegation() {
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
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "child",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "done",
    )
    .await;

    // The subagent (which has a subagent_type) cannot itself orchestrate a delegation.
    let error = crate::delegation_tools::start_readonly_fanout_core(
        &env.state,
        "child",
        json!({ "tasks": [{ "role": "reviewer", "prompt": "review" }] }),
    )
    .await
    .expect_err("nested delegation must be rejected");
    assert_eq!(error.code, "delegations_not_allowed_for_subagent");

    env.cleanup().await;
}

#[tokio::test]
async fn spawn_failure_leaves_no_running_delegation() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    // A parent with NO project makes spawn_subagent fail with project_required,
    // exercising the compensation path: the half-started delegation is failed so the
    // one-delegation-per-parent guard releases rather than stranding the parent.
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

    let error = crate::delegation_tools::start_full_core(
        &env.state,
        "parent",
        json!({ "role": "implementer", "prompt": "do it" }),
    )
    .await
    .expect_err("spawn must fail without a project");
    assert_eq!(error.code, "project_required");

    // The delegation row exists but is failed (not running), so the guard releases.
    assert!(!env
        .state
        .repo
        .parent_has_running_delegation("parent")
        .await
        .expect("running delegation check"));
    let delegations = env
        .state
        .repo
        .list_parent_delegations("parent")
        .await
        .expect("list delegations");
    assert_eq!(delegations.len(), 1);
    assert_eq!(delegations[0].status, DelegationStatus::Failed);

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

/// Count durable parent `input.queued` completion-steer events for a delegation.
///
/// This is intentionally based on the persistent event log rather than the
/// queue row or parent transcript: if replay accidentally publishes a second
/// steer with a fresh/non-deterministic client id, the row and transcript checks
/// can miss it after the first steer is already consumed, but the event log keeps
/// both publishes.
async fn durable_parent_completion_steers(
    env: &TestEnv,
    parent_id: &str,
    delegation_id: &str,
) -> usize {
    env.state
        .repo
        .events_after(parent_id, None)
        .await
        .expect("parent events")
        .into_iter()
        .filter(|event| {
            event.event == EventType::InputQueued
                && event
                    .data
                    .get("priority")
                    .and_then(serde_json::Value::as_str)
                    == Some(InputPriority::Steer.as_str())
                && event_payload_text_mentions(&event.data, delegation_id, "completed")
        })
        .count()
}

fn event_payload_text_mentions(
    payload: &serde_json::Value,
    delegation_id: &str,
    other_needle: &str,
) -> bool {
    payload_texts(payload)
        .iter()
        .any(|text| text.contains(delegation_id) && text.contains(other_needle))
}

fn payload_texts(value: &serde_json::Value) -> Vec<&str> {
    match value {
        serde_json::Value::String(text) => vec![text.as_str()],
        serde_json::Value::Array(items) => items.iter().flat_map(payload_texts).collect(),
        serde_json::Value::Object(map) => ["message", "content", "text"]
            .into_iter()
            .filter_map(|key| map.get(key))
            .flat_map(payload_texts)
            .collect(),
        _ => Vec::new(),
    }
}

/// FIX A: a fan-out whose subagent #1 is terminal while #2 has not yet been
/// inserted must NOT complete — the expected-count fence keeps the barrier shut.
#[tokio::test]
async fn partial_spawn_does_not_complete_delegation() {
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
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create delegation");
    // Only subagent #1 exists so far and is terminal-on-arrival.
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "first",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "fast review done",
    )
    .await;

    // The barrier must not fire while the sibling is still unspawned.
    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("barrier (partial spawn)");
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Running
    );
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 0);

    // The sibling arrives terminal too; now the full set exists -> one steer.
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "second",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "second review done",
    )
    .await;
    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("barrier (full set)");
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Done
    );
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 1);

    env.cleanup().await;
}

/// Simulate a crash after the finish_delegation status claim but before handoff
/// files / steer publication. Boot repair must publish the files, enqueue the
/// deterministic steer, and remain idempotent on later restarts.
#[tokio::test]
async fn boot_repair_publishes_handoff_and_steer_after_finish_claim_crash() {
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
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "impl",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "implemented",
    )
    .await;

    // Claim only the terminal status, then "crash" before normal publication.
    let key = format!(
        "delegation-steer:{}:{}",
        delegation.id, delegation.attempt_id
    );
    assert!(env
        .state
        .repo
        .finish_delegation(
            &delegation.id,
            &delegation.attempt_id,
            DelegationStatus::Done
        )
        .await
        .expect("finish wins"));
    assert!(env
        .state
        .repo
        .find_client_input("parent", &key)
        .await
        .expect("find steer")
        .is_none());
    assert!(!handoff_root(&env, &delegation.id)
        .join("index.json")
        .exists());
    assert_eq!(
        durable_parent_completion_steers(&env, "parent", &delegation.id).await,
        0
    );

    sweep_running_delegations_on_boot(&env.state).await;
    assert!(env
        .state
        .repo
        .find_client_input("parent", &key)
        .await
        .expect("find repaired steer")
        .is_some());
    let root = handoff_root(&env, &delegation.id);
    assert!(!root.join("index.json").exists());
    assert!(root.join("impl").join("final_message.md").exists());
    assert!(root.join("impl").join("transcript.md").exists());
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    assert_eq!(snapshot["status"], "done");
    assert_eq!(snapshot["subagents"][0]["final_message"], "implemented");
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 1);
    let wakeup_snapshot = parent_completion_snapshot(&env, "parent", &delegation.id).await;
    assert_eq!(wakeup_snapshot, snapshot);
    assert_eq!(
        wakeup_snapshot["subagents"][0]["final_message"],
        "implemented"
    );
    assert!(wakeup_snapshot["subagents"][0]["transcript_path"]
        .as_str()
        .expect("transcript path in repaired wakeup")
        .ends_with("impl/transcript.md"));
    assert_eq!(
        durable_parent_completion_steers(&env, "parent", &delegation.id).await,
        1,
        "first repair publishes exactly one durable completion steer"
    );

    let repaired_input = env
        .state
        .repo
        .find_client_input("parent", &key)
        .await
        .expect("find repaired steer")
        .expect("repaired steer exists");
    assert!(matches!(
        repaired_input.status,
        QueuedInputStatus::Queued | QueuedInputStatus::Consuming | QueuedInputStatus::Consumed
    ));

    // A second repair sweep must not double-publish or double-drive. The first
    // repair may already have driven the idle parent and consumed the queued
    // input, so assert the deterministic idempotency row rather than requiring
    // the steer to remain in the active queue.
    sweep_running_delegations_on_boot(&env.state).await;
    let repaired_again = env
        .state
        .repo
        .find_client_input("parent", &key)
        .await
        .expect("find repaired steer after replay")
        .expect("repaired steer still exists");
    assert_eq!(
        repaired_again.input_id, repaired_input.input_id,
        "deterministic steer client id reuses the original row"
    );
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 1);
    assert_eq!(
        durable_parent_completion_steers(&env, "parent", &delegation.id).await,
        1,
        "second repair must not publish any duplicate durable completion steer"
    );

    env.cleanup().await;
}

/// FIX C: a delegation subagent at a NON-boundary leaf (mid-turn) with its action
/// stale-marked (as the boot stale-mark does) and no queued input must NOT cause
/// the boot sweep to complete/steer the delegation.
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
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    // A single full subagent stuck mid-turn (active leaf is an assistant message,
    // not a boundary). create_running_subagent leaves it at the non-boundary leaf.
    create_running_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
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

    // The boot sweep must NOT complete this delegation: terminality is transcript-based,
    // and a mid-turn leaf is not a boundary.
    sweep_running_delegations_on_boot(&env.state).await;
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Running
    );
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 0);

    env.cleanup().await;
}

/// FIX D: a terminal delegation member produces ZERO parent-visible `subagent.idle`
/// rows, yet the single delegation steer is still delivered (and the once-gate fired).
/// Driven through the LIVE seam (`handle_subagent_terminal_for_parent_if_needed`), which
/// is FIX F's live-seam coverage for the suppression path.
#[tokio::test]
async fn terminal_delegation_member_yields_zero_parent_idle_rows() {
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
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "member",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "done",
    )
    .await;

    // Drive the LIVE idle seam for the delegation member.
    let driver = SessionDriver::acquire(&env.state, "member").await;
    driver.handle_subagent_terminal_for_parent_if_needed().await;

    // Zero per-child idle surfaced to the parent...
    assert_eq!(parent_idle_rows(&env, "parent").await, 0);
    // ...yet the delegation completed and the single steer was delivered.
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Done
    );
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 1);

    env.cleanup().await;
}

/// Server-side guard: a `Steer`-priority input targeting a `read_only` subagent
/// is rejected with `cannot_steer_read_only_subagent`, a steer to a running full
/// subagent is accepted, and terminal subagents are not reactivated.
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
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 2)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "ro",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "done",
    )
    .await;
    create_busy_full_subagent(&env, project_id, "parent", &delegation.id, "full_running").await;
    env.state.active.lock().await.insert(
        "full_running".to_string(),
        Arc::new(Mutex::new(RuntimeSession {
            session: AgentSession::new(),
            config: session_config(
                &env,
                project_id,
                json!({ "created_by": "test", "role_name": "implementer", "harness": true }),
            ),
        })),
    );

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

    // Steering a genuinely running full subagent is accepted.
    let accepted = crate::input_user(&env.state, steer("full_running"))
        .await
        .expect("steering a running full subagent is allowed");
    assert_eq!(accepted["accepted"], true);

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

    // A full subagent that is already terminal must not be reactivated by a
    // steer-priority input.
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "full_terminal",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "done",
    )
    .await;
    let rejected = crate::input_user(&env.state, steer("full_terminal"))
        .await
        .expect_err("steering a terminal full subagent must be rejected");
    assert_eq!(rejected.code, "subagent_terminal");

    env.cleanup().await;
}

/// FIX E: a delegation member whose initial dispatch fails produces no parent-visible
/// `subagent.idle`. We exercise the spawn path with a provider error by spawning
/// into a delegation from a parent that will fail dispatch; the dispatch-failed
/// notifier must short-circuit for a child that has a delegation_id.
#[tokio::test]
async fn dispatch_failure_for_delegation_member_emits_no_parent_idle() {
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
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 1)
        .await
        .expect("create delegation");
    // A terminal delegation member already exists with a delegation_id; route a simulated
    // dispatch failure for it through the gate. The gate must suppress the
    // parent-visible idle because the child belongs to a delegation.
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
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

/// FIX F: two sibling delegation members reaching idle through the LIVE seam — one
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
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
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
        &delegation.id,
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
    // delegation.
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
            .get_delegation(&delegation.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DelegationStatus::Done
    );
    assert_eq!(steers_to_parent(&env, "parent", &delegation.id).await, 1);

    env.cleanup().await;
}
