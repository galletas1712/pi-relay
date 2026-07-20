//! Deterministic delegation barrier / handoff / wakeup-observation tests against a real Postgres.
//!
//! These drive the barrier directly (the live lifecycle hook and the boot
//! sweep both funnel through `complete_delegation_if_ready`), with subagents placed
//! into terminal/running states by writing their durable transcripts directly,
//! so the tests are fully deterministic and need no provider.

use crate::provider_runtime::{first_party_toolsets, mcp_snapshot_for_session};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use agent_provider::{ModelResponse, ModelStopDetails, ModelStopReason, ModelTranscriptEntry};
use agent_session::{
    AgentSession, ModelContext, ModelContextEntry, SessionAction, StoredSession,
    TranscriptStorageNode,
};
use agent_store::{
    ActionKind, ActionStatus, ActionUpdate, CompactionCompletion, CompactionScope,
    CompactionTrigger, Delegation, DelegationKind, DelegationStatus, EventType, InputPriority,
    McpSessionManifestBinding, OutputBatch, PostgresAgentStore, QueuedInputContent,
    QueuedInputStatus, SessionConfig, SubagentControlPhase, SubagentType, TranscriptEntryBodyMode,
    TranscriptEntryScope,
};
use agent_tools::ToolRegistry;
use agent_vocab::{
    ActionId, AssistantItem, AssistantMessage, CompactionSummary, DaemonToolObservation,
    ProviderConfig, ProviderKind, ProviderReplayItem, ReasoningEffort, ToolCall, ToolCallId,
    ToolResultMessage, TranscriptItem, TurnId, TurnOutcome, UserMessage,
};
use serde_json::json;
use sqlx::Row;
use tokio::sync::{broadcast, Mutex};
use uuid::Uuid;

/// A unique temp directory removed on drop, so tests need no `tempfile` dep.
struct TempDir {
    path: PathBuf,
}

#[path = "history_fork_rpc_tests.rs"]
mod history_fork_rpc_tests;

#[tokio::test]
async fn ordinary_tool_dispatch_claims_starts_and_completes_exactly_once() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "ordinary tool claim",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let marker = env.cwd.path().join("ordinary-tool-runs");
    let command = format!("printf run >> '{}'", marker.display());
    let session_id = "ordinary_tool_claim_once";
    start_prepared_session(
        &env.state,
        PreparedSessionStart {
            session_id: session_id.to_string(),
            config: session_config(
                &env,
                project_id,
                json!({
                    "created_by": "test",
                    "fault_injection": {
                        "model_result": "tool_once",
                        "tool_command": command,
                    }
                }),
            ),
            priority: InputPriority::FollowUp,
            content: UserMessage::text("run the injected tool"),
            client_input_id: None,
            parent_session_id: None,
            subagent_type: None,
            delegation_id: None,
            dispatch_mode: PreparedSessionDispatchMode::Auto,
        },
    )
    .await
    .expect("session starts");

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if env.state.repo.activity(session_id).await.unwrap()
                == agent_store::SessionActivity::Idle
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("tool reaches durable completion");

    assert_eq!(
        std::fs::read_to_string(&marker).expect("tool side effect exists"),
        "run"
    );
    let events = env
        .state
        .repo
        .events_after(session_id, None)
        .await
        .expect("events load");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event == EventType::ToolStarted)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event == EventType::ToolCompleted)
            .count(),
        1
    );
    let started = events
        .iter()
        .find(|event| event.event == EventType::ToolStarted)
        .expect("tool.started exists");
    let completed = events
        .iter()
        .find(|event| event.event == EventType::ToolCompleted)
        .expect("tool.completed exists");
    assert!(started.event_id < completed.event_id);
    let actions = env
        .state
        .repo
        .session_snapshot(session_id)
        .await
        .expect("session snapshot loads")
        .pending_actions;
    assert!(actions
        .iter()
        .all(|action| action.kind != ActionKind::Tool || action.status == ActionStatus::Completed));

    SessionDriver::acquire(&env.state, session_id)
        .await
        .dispatch_ready_actions()
        .await
        .expect("repeat dispatch scan succeeds");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        std::fs::read_to_string(&marker).expect("tool side effect remains"),
        "run"
    );
    env.cleanup().await;
}

#[tokio::test]
async fn history_switch_and_fork_rpc_reject_running_delegation_identically() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "history delegation guard",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    env.state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create running delegation");

    let switch_error = public_rpc(
        &env.state,
        "history.switch",
        json!({ "session_id": "parent", "leaf_id": null }),
    )
    .await
    .expect_err("running delegation blocks switch");
    let fork_error = public_rpc(
        &env.state,
        "history.fork",
        json!({ "session_id": "parent", "leaf_id": null }),
    )
    .await
    .expect_err("running delegation blocks fork");

    assert_eq!(switch_error.code, "session_busy");
    assert_eq!(fork_error.code, switch_error.code);
    assert_eq!(fork_error.message, switch_error.message);

    env.cleanup().await;
}

#[tokio::test]
async fn proactive_compaction_blocks_pending_selected_mcp_action_without_model_error() {
    let Some(mut env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let mcp_config: agent_mcp::McpConfig = serde_json::from_value(json!({
        "servers": {
            "fixture": {
                "transport": {
                    "type": "stdio",
                    "command": fake_mcp_server(),
                    "env": { "MCP_FIXTURE_MODE": "simple" }
                },
                "allow_all_tools": true,
            }
        }
    }))
    .expect("MCP config parses");
    env.state.mcp = agent_mcp::McpManager::start(mcp_config)
        .await
        .expect("MCP manager starts");
    let inventory = env
        .state
        .mcp
        .inventory(
            ProviderKind::OpenAi,
            &first_party_toolsets(&env.state, agent_prompt::PromptProfile::Parent),
        )
        .await
        .expect("MCP inventory loads");
    let selected_tool = inventory.servers[0].tools[0].raw_name.clone();
    let selected = env
        .state
        .mcp
        .select(
            &agent_mcp::McpSessionSelection {
                inventory_revision: inventory.revision,
                servers: vec![agent_mcp::McpServerSelection {
                    server: inventory.servers[0].server.clone(),
                    tools: vec![selected_tool],
                }],
            },
            &first_party_toolsets(&env.state, agent_prompt::PromptProfile::Parent),
        )
        .await
        .expect("MCP tool selection resolves");

    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "proactive pending compaction",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "proactive_pending_selected_mcp";
    let mut config = session_config(
        &env,
        project_id,
        json!({
            "created_by": "test",
            "fault_injection": {
                "pause_compaction_dispatch_before_provider": true,
            },
            "compaction": {
                "config": {
                    "auto_enabled": true,
                    "auto_limit_tokens": 8_000,
                }
            }
        }),
    );
    config.mcp_manifest = Some(McpSessionManifestBinding {
        manifest_fingerprint: selected.manifest_fingerprint().to_string(),
        manifest: serde_json::to_value(selected.manifest()).expect("MCP manifest serializes"),
    });
    let started = start_prepared_session(
        &env.state,
        PreparedSessionStart {
            session_id: session_id.to_string(),
            config,
            priority: InputPriority::FollowUp,
            content: UserMessage::text("threshold token ".repeat(12_000)),
            client_input_id: None,
            parent_session_id: None,
            subagent_type: None,
            delegation_id: None,
            dispatch_mode: PreparedSessionDispatchMode::Deferred,
        },
    )
    .await
    .expect("session starts with pending model action");
    let model_action = started
        .dispatches
        .first()
        .expect("model dispatch persists")
        .row_id
        .clone();
    assert!(env
        .state
        .repo
        .session_snapshot(session_id)
        .await
        .expect("pending session snapshot loads")
        .pending_actions
        .iter()
        .any(|action| {
            action.action_row_id == model_action && action.status == ActionStatus::Pending
        }));

    SessionDriver::acquire(&env.state, session_id)
        .await
        .dispatch_ready_actions()
        .await
        .expect("proactive gate dispatches compaction");

    let snapshot = env
        .state
        .repo
        .session_snapshot(session_id)
        .await
        .expect("session snapshot loads");
    assert!(snapshot.pending_actions.iter().any(|action| {
        action.action_row_id == model_action
            && action.kind == ActionKind::Model
            && action.status == ActionStatus::Blocked
    }));
    assert!(snapshot.pending_actions.iter().any(|action| {
        action.kind == ActionKind::Compaction && action.status == ActionStatus::Running
    }));
    assert!(env
        .state
        .repo
        .events_after(session_id, None)
        .await
        .expect("events load")
        .iter()
        .all(|event| event.event != EventType::ModelError));
    assert!(env
        .state
        .repo
        .load_session_config(session_id)
        .await
        .expect("session config loads")
        .mcp_manifest
        .is_some());

    env.state.mcp.shutdown().await;
    env.cleanup().await;
}

#[tokio::test]
async fn public_rpc_mcp_selection_is_fenced_frozen_and_inherited() {
    let Some(mut env) = test_env().await else {
        eprintln!(
            "SKIPPED PostgreSQL selected-session RPC test: PI_RELAY_TEST_DATABASE_URL is not set"
        );
        return;
    };
    let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    std::fs::copy(source_root.join("PI.md"), env.cwd.path().join("PI.md"))
        .expect("copy PI template");
    let role_dir = env.cwd.path().join("subagent-roles/implementer");
    std::fs::create_dir_all(&role_dir).expect("create role dir");
    std::fs::copy(
        source_root.join("subagent-roles/implementer/SKILL.md"),
        role_dir.join("SKILL.md"),
    )
    .expect("copy implementer role");
    let marker = env.cwd.path().join("public-rpc-mcp-marker");
    let mcp_config: agent_mcp::McpConfig = serde_json::from_value(json!({
        "servers": {
            "fixture": {
                "transport": {
                    "type": "stdio",
                    "command": fake_mcp_server(),
                    "env": {
                        "MCP_FIXTURE_MODE": "notification_race",
                        "MCP_FIXTURE_MARKER": marker
                    }
                },
                "allow_all_tools": true,
                "call_timeout_ms": 1_000,
            }
        }
    }))
    .expect("MCP config parses");
    env.state.mcp = agent_mcp::McpManager::start(mcp_config)
        .await
        .expect("MCP manager starts");
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "runtime-test", "public RPC MCP", &[], json!({}))
        .await
        .expect("create project");
    let provider = json!({
        "kind": "openai",
        "model": "gpt-5.2",
        "reasoning_effort": "medium",
    });
    let inventory = public_rpc(&env.state, "mcp.inventory", json!({ "provider": "openai" }))
        .await
        .expect("inventory RPC succeeds");
    let revision = inventory["revision"]
        .as_str()
        .expect("inventory revision")
        .to_string();
    assert_eq!(inventory["servers"][0]["server"], "fixture");
    assert_eq!(inventory["servers"][0]["tools"][0]["raw_name"], "echo");

    let stale_error = public_rpc(
        &env.state,
        "session.start",
        json!({
            "session_id": "rpc-stale",
            "project_id": project_id,
            "provider": provider,
            "metadata": { "harness": true },
            "content": [{ "type": "text", "text": "stale" }],
            "mcp": {
                "inventory_revision": "stale",
                "servers": [{ "server": "fixture", "tools": ["echo"] }],
            },
        }),
    )
    .await
    .expect_err("stale selection rejects");
    assert_eq!(stale_error.code, "mcp_inventory_changed");
    assert!(!env
        .state
        .repo
        .session_exists("rpc-stale")
        .await
        .expect("stale session absence loads"));

    public_rpc(
        &env.state,
        "session.start",
        json!({
            "session_id": "rpc-no-mcp",
            "project_id": project_id,
            "provider": provider,
            "metadata": { "harness": true },
            "content": [{ "type": "text", "text": "no MCP" }],
        }),
    )
    .await
    .expect("MCP-free session starts");
    let no_mcp_config = env
        .state
        .repo
        .load_session_config("rpc-no-mcp")
        .await
        .expect("MCP-free config loads");
    assert!(no_mcp_config.mcp_manifest.is_none());
    let no_mcp_prompt = public_rpc(
        &env.state,
        "system.prompt",
        json!({ "session_id": "rpc-no-mcp" }),
    )
    .await
    .expect("MCP-free prompt loads");
    assert!(!no_mcp_prompt["rendered"]
        .as_str()
        .expect("MCP-free rendered prompt")
        .contains("Selected MCP tools"));
    let global_tools = public_rpc(&env.state, "tools.list", json!({ "provider": "openai" }))
        .await
        .expect("global tools list succeeds");
    assert!(global_tools["tools"]
        .as_array()
        .expect("global tools array")
        .iter()
        .all(|tool| tool["kind"] == "local_tool"));

    public_rpc(
        &env.state,
        "session.start",
        json!({
            "session_id": "rpc-selected",
            "project_id": project_id,
            "provider": provider,
            "metadata": { "harness": true },
            "content": [{ "type": "text", "text": "selected MCP" }],
            "mcp": {
                "inventory_revision": revision,
                "servers": [{ "server": "fixture", "tools": ["echo"] }],
            },
        }),
    )
    .await
    .expect("selected session starts");
    let selected_config = env
        .state
        .repo
        .load_session_config("rpc-selected")
        .await
        .expect("selected config loads");
    let selected_binding = selected_config
        .mcp_manifest
        .clone()
        .expect("selected manifest is bound");
    let snapshot = mcp_snapshot_for_session(&selected_config).expect("selected snapshot loads");
    let mut future_default = selected_config.clone();
    future_default.provider.reasoning_effort = ReasoningEffort::High;
    env.state
        .repo
        .configure_session("rpc-selected", &future_default)
        .await
        .expect("change selected session future provider default");
    let dispatches = SessionDriver::acquire(&env.state, "rpc-selected")
        .await
        .dispatch_ready_actions()
        .await
        .expect("selected session action dispatches");
    assert_eq!(dispatches.len(), 1);
    assert_eq!(
        dispatches[0].config.provider.reasoning_effort,
        ReasoningEffort::Medium,
        "dispatch keeps the provider route captured by the initial action"
    );
    assert_eq!(
        dispatches[0].mcp_snapshot.manifest_fingerprint(),
        snapshot.manifest_fingerprint(),
        "dispatch independently carries the frozen session MCP manifest"
    );
    assert!(
        !dispatches[0]
            .mcp_snapshot
            .provider_tools(dispatches[0].config.provider.kind)
            .is_empty(),
        "MCP declarations are shaped for the captured dispatch provider"
    );
    let selected_action_row_id = dispatches[0].row_id.clone();
    let selected_action_attempt_id = dispatches[0].attempt_id.clone();
    let selected_tool = snapshot.manifest().tools[0].clone();
    let prompt = public_rpc(
        &env.state,
        "system.prompt",
        json!({ "session_id": "rpc-selected" }),
    )
    .await
    .expect("selected prompt loads")["rendered"]
        .as_str()
        .expect("rendered prompt")
        .to_string();
    let selected_section = prompt
        .split("### MCP")
        .nth(1)
        .and_then(|section| section.split("\n## ").next())
        .expect("selected MCP section exists");
    assert!(selected_section.contains("fixture"));
    assert!(selected_section.contains(&selected_tool.exposed_name));
    for secret in [
        selected_tool.description.as_str(),
        "inputSchema",
        "input_schema",
        "healthy",
        "contract_fingerprint",
        "manifest_fingerprint",
        &selected_tool.contract_fingerprint,
    ] {
        assert!(
            !selected_section.contains(secret),
            "selected MCP prompt section leaked {secret}"
        );
    }
    let tools_before = public_rpc(
        &env.state,
        "tools.list",
        json!({ "provider": "openai", "session_id": "rpc-selected" }),
    )
    .await
    .expect("selected tools list loads");
    assert!(tools_before["tools"]
        .as_array()
        .expect("selected tools array")
        .iter()
        .any(|tool| {
            tool["kind"] == "mcp_tool"
                && tool["server"] == "fixture"
                && tool["name"] == selected_tool.exposed_name
        }));
    let request_before = build_model_request(
        &env.state,
        &selected_config,
        "rpc-selected",
        None,
        ModelContext::new(),
        &snapshot,
    )
    .await
    .expect("initial selected request builds");

    env.state
        .mcp
        .call(
            &snapshot,
            &selected_tool.exposed_name,
            json!({ "value": "refresh" }),
        )
        .await
        .expect("fixture call triggers list_changed");
    let refreshed_inventory =
        public_rpc(&env.state, "mcp.inventory", json!({ "provider": "openai" }))
            .await
            .expect("inventory refreshes");
    assert_ne!(refreshed_inventory["revision"], revision);
    let tools_after = public_rpc(
        &env.state,
        "tools.list",
        json!({ "provider": "openai", "session_id": "rpc-selected" }),
    )
    .await
    .expect("frozen tools list reloads");
    assert_eq!(tools_after, tools_before);
    let request_after = build_model_request(
        &env.state,
        &selected_config,
        "rpc-selected",
        None,
        ModelContext::new(),
        &snapshot,
    )
    .await
    .expect("later selected request builds");
    assert_eq!(request_after.tools, request_before.tools);

    let full = public_rpc(
        &env.state,
        "delegation.start_full",
        json!({
            "parent_session_id": "rpc-selected",
            "role": "implementer",
            "prompt": "full child",
        }),
    )
    .await
    .expect("full child starts");
    let full_id = full["subagent_session_id"].as_str().expect("full child id");
    assert_eq!(
        env.state
            .repo
            .load_session_config(full_id)
            .await
            .expect("full child config")
            .mcp_manifest,
        Some(selected_binding.clone())
    );
    env.state
        .repo
        .set_delegation_status(
            full["delegation_id"].as_str().expect("full delegation id"),
            DelegationStatus::Failed,
        )
        .await
        .expect("finish full delegation for test");

    let read_only = public_rpc(
        &env.state,
        "delegation.start_readonly_fanout",
        json!({
            "parent_session_id": "rpc-selected",
            "tasks": [{ "role": "implementer", "prompt": "read-only child" }],
        }),
    )
    .await
    .expect("read-only child starts");
    let read_only_id = read_only["subagent_session_ids"][0]
        .as_str()
        .expect("read-only child id");
    assert_eq!(
        env.state
            .repo
            .load_session_config(read_only_id)
            .await
            .expect("read-only child config")
            .mcp_manifest,
        Some(selected_binding)
    );

    assert!(env
        .state
        .repo
        .claim_pending_model_action(
            "rpc-selected",
            &selected_action_row_id,
            &selected_action_attempt_id,
        )
        .await
        .expect("selected model action claims for compaction"));
    let compaction = env
        .state
        .repo
        .block_model_action_for_compaction(
            "rpc-selected",
            &selected_action_row_id,
            &selected_action_attempt_id,
            ActionStatus::Running,
            None,
            CompactionTrigger::Auto {
                reason: "selected MCP recovery test".to_string(),
            },
            None,
            Some(100_000),
        )
        .await
        .expect("selected model action blocks for compaction");
    env.state
        .repo
        .complete_compaction_action(
            &compaction.job,
            successful_compaction("selected MCP recovery summary"),
        )
        .await
        .expect("selected MCP compaction completes");
    env.state.active.lock().await.remove("rpc-selected");
    assert_eq!(
        recover_post_compaction_dispatches_on_boot(&env.state)
            .await
            .expect("selected MCP post-compaction action recovers"),
        1
    );
    let recovered = env
        .state
        .active
        .lock()
        .await
        .get("rpc-selected")
        .cloned()
        .expect("selected MCP runtime is reconstructed");
    let recovered = recovered.lock().await;
    assert_eq!(
        recovered.config.provider.reasoning_effort,
        ReasoningEffort::Medium,
        "compaction recovery keeps the captured provider route"
    );
    assert_eq!(
        mcp_snapshot_for_session(&recovered.config)
            .expect("recovered selected MCP snapshot validates")
            .manifest_fingerprint(),
        snapshot.manifest_fingerprint(),
        "compaction recovery keeps the exact selected MCP manifest"
    );

    env.state.mcp.shutdown().await;
    env.cleanup().await;
}

#[tokio::test]
async fn harness_post_compaction_boot_recovery_keeps_legacy_session_mcp_free() {
    let Some(env) = test_env().await else {
        eprintln!(
            "SKIPPED PostgreSQL harness MCP recovery test: PI_RELAY_TEST_DATABASE_URL is not set"
        );
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "legacy harness MCP recovery",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "legacy_harness_post_compaction";
    let (resumed, _) = commit_post_compaction_dispatch(&env, project_id, session_id).await;
    env.state
        .repo
        .update_session_metadata(
            session_id,
            &json!({ "created_by": "test", "harness": true }),
        )
        .await
        .expect("session switches to harness recovery");
    assert_eq!(
        recover_post_compaction_dispatches_on_boot(&env.state)
            .await
            .expect("harness boot recovery succeeds"),
        1
    );
    let config = env
        .state
        .repo
        .load_session_config(session_id)
        .await
        .expect("legacy session config loads");
    assert!(config.mcp_manifest.is_none());
    assert!(mcp_snapshot_for_session(&config)
        .expect("legacy session resolves empty snapshot")
        .manifest()
        .tools
        .is_empty());
    assert!(env
        .state
        .repo
        .load_harness_model_action(session_id, &resumed.row_id)
        .await
        .expect("recovered harness action loads")
        .post_compaction_dispatch_lease
        .is_some());
    assert!(env
        .state
        .tasks
        .lock()
        .expect("task registry lock")
        .is_empty());

    env.cleanup().await;
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

const BLOCKED_USER_INSTRUCTION: &str =
    "Return the exact requested sentinel facts from this instruction.";

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

use crate::provider_runtime::{
    append_delegation_ledger_to_output, build_model_request, native_compaction_request,
    CompactionOutput, CompactionSummaryKind, ProviderConnectionRegistry, SessionTitleScheduler,
};
use crate::runtime::{
    apply_model_response, recover_post_compaction_dispatches_on_boot, take_tasks, SessionDriver,
};
use crate::runtime_hosts::test_support::{connect_test_runtime, TEST_RUNTIME_ID};
use crate::runtime_hosts::RuntimeRegistry;
use crate::session_start::{
    start_prepared_session, PreparedSessionDispatchMode, PreparedSessionStart,
};
use crate::state::{AppState, RunningTask, TaskRegistrationId};
use crate::types::{DispatchAction, RuntimeSession};

use super::{
    complete_delegation_if_ready, publish_next_partial_after_parent_decision,
    sweep_running_delegations_on_boot, try_claim_and_publish_completed_delegation,
};
use crate::delegation_tools::{
    cancel_core, interrupt_subagent_core, read_handoff_file_core, rpc_list, run_delegation_tool,
    status_core, steer_subagent_core,
};
use crate::{enqueue_session_input, SessionInputRequest};

static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(90_000);

struct TestEnv {
    state: AppState,
    admin_url: String,
    name: String,
    _state_dir: TempDir,
    cwd: TempDir,
}

#[tokio::test]
async fn session_get_missing_returns_stable_not_found() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };

    let error = public_rpc(
        &env.state,
        "session.get",
        json!({ "session_id": "missing-session" }),
    )
    .await
    .expect_err("missing session must fail");

    assert_eq!(error.code, "session_not_found");
    assert_eq!(error.message, "session not found");
    env.cleanup().await;
}

impl TestEnv {
    async fn cleanup(self) {
        for handle in take_tasks(&self.state) {
            handle.abort();
        }
        self.state.repo.close().await;
        if let Ok(admin) = sqlx::PgPool::connect(&self.admin_url).await {
            let _ = sqlx::query(&format!(r#"drop database if exists "{}""#, self.name))
                .execute(&admin)
                .await;
            admin.close().await;
        }
    }

    fn workspace_id(&self) -> String {
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
    let repo = Arc::new(store);
    let runtime_hosts = RuntimeRegistry::new(repo.clone());
    connect_test_runtime(&runtime_hosts, TEST_RUNTIME_ID).await;
    let state = AppState {
        repo,
        active: Arc::new(Mutex::new(HashMap::new())),
        session_driver_locks: Arc::new(Mutex::new(HashMap::new())),
        tasks: Arc::new(StdMutex::new(HashMap::new())),
        auxiliary_tasks: Arc::new(StdMutex::new(Vec::new())),
        task_registration_lock: Arc::new(StdMutex::new(())),
        post_compaction_recovery_scheduled: Arc::new(AtomicBool::new(false)),
        post_compaction_recovery_notify: Arc::new(tokio::sync::Notify::new()),
        post_compaction_recovery_task: Arc::new(StdMutex::new(None)),
        shutting_down: Arc::new(AtomicBool::new(false)),
        events,
        tools: Arc::new(ToolRegistry::with_builtin_tools()),
        mcp: agent_mcp::McpManager::disabled(),
        provider_connections: ProviderConnectionRegistry::new(),
        session_titles: SessionTitleScheduler::disabled(),
        runtime_hosts,
        prompt_root: cwd.path().to_path_buf(),
        config_root: cwd.path().to_path_buf(),
        daemon_config: crate::config::DaemonConfig::default(),
        pause_subagent_control_after_commit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        subagent_control_committed: Arc::new(tokio::sync::Notify::new()),
        fail_subagent_control_reload_after_commit: Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )),
    };
    Some(TestEnv {
        state,
        admin_url,
        name,
        _state_dir: state_dir,
        cwd,
    })
}

async fn public_rpc(
    state: &AppState,
    method: &str,
    params: serde_json::Value,
) -> std::result::Result<serde_json::Value, crate::types::RpcError> {
    crate::dispatch_request(
        state,
        &mut std::collections::BTreeSet::new(),
        &mut std::collections::BTreeMap::new(),
        method.to_string(),
        params,
    )
    .await
}

fn fake_mcp_server() -> PathBuf {
    let target = std::env::current_exe()
        .expect("current test executable")
        .parent()
        .and_then(std::path::Path::parent)
        .expect("Cargo target profile directory")
        .to_path_buf();
    let executable = if cfg!(windows) {
        "fake_mcp_server.exe"
    } else {
        "fake_mcp_server"
    };
    std::fs::read_dir(target.join("build"))
        .expect("Cargo build directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("agent-mcp-")
        })
        .map(|entry| entry.path().join("out").join(executable))
        .find(|path| path.is_file())
        .expect("agent-mcp fake server executable")
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

async fn test_app_state(
    store: PostgresAgentStore,
    _state_dir: &TempDir,
    prompt_root: PathBuf,
) -> AppState {
    let (events, _rx) = broadcast::channel(1024);
    let repo = Arc::new(store);
    let runtime_hosts = RuntimeRegistry::new(repo.clone());
    connect_test_runtime(&runtime_hosts, TEST_RUNTIME_ID).await;
    AppState {
        repo,
        active: Arc::new(Mutex::new(HashMap::new())),
        session_driver_locks: Arc::new(Mutex::new(HashMap::new())),
        tasks: Arc::new(StdMutex::new(HashMap::new())),
        auxiliary_tasks: Arc::new(StdMutex::new(Vec::new())),
        task_registration_lock: Arc::new(StdMutex::new(())),
        post_compaction_recovery_scheduled: Arc::new(AtomicBool::new(false)),
        post_compaction_recovery_notify: Arc::new(tokio::sync::Notify::new()),
        post_compaction_recovery_task: Arc::new(StdMutex::new(None)),
        shutting_down: Arc::new(AtomicBool::new(false)),
        events,
        tools: Arc::new(ToolRegistry::with_builtin_tools()),
        mcp: agent_mcp::McpManager::disabled(),
        provider_connections: ProviderConnectionRegistry::new(),
        session_titles: SessionTitleScheduler::disabled(),
        runtime_hosts,
        config_root: prompt_root.clone(),
        prompt_root,
        daemon_config: crate::config::DaemonConfig::default(),
        pause_subagent_control_after_commit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        subagent_control_committed: Arc::new(tokio::sync::Notify::new()),
        fail_subagent_control_reload_after_commit: Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )),
    }
}

async fn expire_post_compaction_lease(
    database_url: &str,
    session_id: &str,
    action_row_id: &str,
    attempt_id: &str,
) {
    let pool = sqlx::PgPool::connect(database_url)
        .await
        .expect("connect fault-injection pool");
    sqlx::query(
        r#"
        update actions
        set payload=jsonb_set(
            payload,
            '{post_compaction_dispatch,lease,expires_at_ms}',
            '1'::jsonb
        )
        where session_id=$1 and id=$2 and attempt_id=$3
        "#,
    )
    .bind(session_id)
    .bind(action_row_id)
    .bind(attempt_id)
    .execute(&pool)
    .await
    .expect("expire crashed owner lease");
    pool.close().await;
}

async fn install_compaction_metadata_fault(pool: &sqlx::PgPool) {
    sqlx::query(
        r#"
        create function reject_compaction_metadata_update() returns trigger
        language plpgsql as $$
        begin
            raise exception 'injected compaction metadata failure';
        end
        $$
        "#,
    )
    .execute(pool)
    .await
    .expect("create fault function");
    sqlx::query(
        r#"
        create trigger reject_compaction_metadata_update
        before update of metadata on sessions
        for each row execute function reject_compaction_metadata_update()
        "#,
    )
    .execute(pool)
    .await
    .expect("create fault trigger");
}

async fn remove_compaction_metadata_fault(pool: &sqlx::PgPool) {
    sqlx::query("drop trigger reject_compaction_metadata_update on sessions")
        .execute(pool)
        .await
        .expect("drop fault trigger");
    sqlx::query("drop function reject_compaction_metadata_update()")
        .execute(pool)
        .await
        .expect("drop fault function");
}

fn session_config(env: &TestEnv, project_id: Uuid, metadata: serde_json::Value) -> SessionConfig {
    SessionConfig {
        project_id: Some(project_id),
        runtime_id: "runtime-test".to_string(),
        workspace_id: env.workspace_id(),
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
        mcp_manifest: None,
    }
}

fn subagent_test_metadata(role: &str, subagent_type: SubagentType) -> serde_json::Value {
    json!({
        "created_by": "test",
        "subagent": true,
        "prompt_profile": "subagent",
        "subagent_type": subagent_type.as_str(),
        "role_name": role,
    })
}

/// A parent session that opts into the harness, so any model dispatch the
/// barrier's wakeup observation triggers stops at `pending` instead of calling
/// a provider.
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

#[tokio::test]
async fn empty_dispatch_stops_after_the_pending_query() {
    let Some(env) = test_env().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let driver = SessionDriver::acquire(&env.state, "missing-empty-dispatch").await;

    let dispatched = driver
        .dispatch_ready_actions()
        .await
        .expect("missing session has no pending actions");

    assert!(dispatched.is_empty());
    env.cleanup().await;
}

#[tokio::test]
async fn nonempty_dispatch_uses_session_fallback_for_legacy_null_route() {
    let Some(env) = test_env().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    let session_id = "legacy-null-dispatch";
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "legacy null dispatch",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let entries = vec![
        TranscriptStorageNode {
            id: "legacy-turn".to_string(),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: "legacy-user".to_string(),
            parent_id: Some("legacy-turn".to_string()),
            timestamp_ms: 2,
            item: TranscriptItem::UserMessage(UserMessage::text("hello")),
            provider_replay: Vec::new(),
        },
    ];
    let action = SessionAction::RequestModel {
        action_id: ActionId(1),
        turn_id: TurnId(1),
        model_context: ModelContext::new(),
        context_leaf_id: Some("legacy-user".to_string()),
    };
    let mut config = session_config(&env, project_id, json!({ "harness": true }));
    config.provider.reasoning_effort = ReasoningEffort::High;
    let (_, persisted) = env
        .state
        .repo
        .start_session_outputs(
            session_id,
            &config,
            &entries,
            Some("legacy-user"),
            &[],
            &[action],
            InputPriority::FollowUp,
            &UserMessage::text("hello"),
            None,
        )
        .await
        .expect("create pending action");
    let database_url = database_url_with_name(&env.admin_url, &env.name);
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect route mutation pool");
    sqlx::query("update actions set provider_config=null where id=$1")
        .bind(&persisted[0].row_id)
        .execute(&pool)
        .await
        .expect("simulate old daemon action insert");
    pool.close().await;

    let driver = SessionDriver::acquire(&env.state, session_id).await;
    let dispatched = driver
        .dispatch_ready_actions()
        .await
        .expect("legacy pending action dispatches");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(
        dispatched[0].config.provider.reasoning_effort,
        ReasoningEffort::High
    );
    env.cleanup().await;
}

#[tokio::test]
async fn provider_retry_keeps_recovered_route_after_default_changes() {
    let Some(env) = test_env().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    let session_id = "retry-route";
    env.state
        .repo
        .create_project(project_id, "runtime-test", "retry route", &[], json!({}))
        .await
        .expect("create project");
    let entries = vec![
        TranscriptStorageNode {
            id: "retry-turn".to_string(),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: "retry-user".to_string(),
            parent_id: Some("retry-turn".to_string()),
            timestamp_ms: 2,
            item: TranscriptItem::UserMessage(UserMessage::text("retry")),
            provider_replay: Vec::new(),
        },
    ];
    let action = SessionAction::RequestModel {
        action_id: ActionId(1),
        turn_id: TurnId(1),
        model_context: ModelContext::from_transcript_items(
            entries.iter().map(|entry| entry.item.clone()).collect(),
        ),
        context_leaf_id: Some("retry-user".to_string()),
    };
    let mut original = session_config(
        &env,
        project_id,
        json!({
            "harness": true,
            "fault_injection": {
                "force_harness_model_dispatch": true,
                "model_result": "retry_once_then_complete",
                "model_provider_max_attempts": 2
            }
        }),
    );
    original.provider.reasoning_effort = ReasoningEffort::Medium;
    env.state
        .repo
        .start_session_outputs(
            session_id,
            &original,
            &entries,
            Some("retry-user"),
            &[],
            &[action],
            InputPriority::FollowUp,
            &UserMessage::text("retry"),
            None,
        )
        .await
        .expect("create retryable action");
    let mut future_default = original;
    future_default.provider.reasoning_effort = ReasoningEffort::High;
    env.state
        .repo
        .configure_session(session_id, &future_default)
        .await
        .expect("change future default");

    let driver = SessionDriver::acquire(&env.state, session_id).await;
    driver
        .ensure_active_loaded_preserving_open_turn()
        .await
        .expect("load retry runtime");
    let dispatched = driver
        .dispatch_ready_actions()
        .await
        .expect("dispatch recovered action");
    assert_eq!(
        dispatched[0].config.provider.reasoning_effort,
        ReasoningEffort::Medium
    );
    drop(driver);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if crate::provider_runtime::injected_provider_start_count(session_id) == 2
                && !env
                    .state
                    .repo
                    .has_unfinished_actions(session_id)
                    .await
                    .expect("read retry action state")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the same captured dispatch route survives its provider retry");
    assert_eq!(
        crate::provider_runtime::injected_provider_start_efforts(session_id),
        vec![ReasoningEffort::Medium, ReasoningEffort::Medium],
        "both provider attempts receive the recovered route, not the new default"
    );
    env.cleanup().await;
}

#[tokio::test]
async fn true_empty_active_output_pass_opens_no_transaction_or_events() {
    let Some(env) = test_env().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let session_id = "empty-active-output";
    let project_id = Uuid::new_v4();
    let active = Arc::new(Mutex::new(RuntimeSession {
        session: AgentSession::new(),
        config: session_config(&env, project_id, json!({})),
        persisted_active_leaf_id: None,
    }));
    let mut events = env.state.events.subscribe();
    let driver = SessionDriver::acquire(&env.state, session_id).await;
    env.state.repo.close().await;

    let dispatched = driver
        .persist_active_outputs(active, None, None, None, Vec::new())
        .await
        .expect("empty output does not touch the closed pool");

    assert!(dispatched.is_empty());
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    drop(driver);
    env.cleanup().await;
}

fn active_leaf_test_entries(session_id: &str) -> Vec<TranscriptStorageNode> {
    ["a", "b"]
        .into_iter()
        .enumerate()
        .map(|(index, suffix)| TranscriptStorageNode {
            id: format!("{session_id}-{suffix}"),
            parent_id: None,
            timestamp_ms: index as u64 + 1,
            item: TranscriptItem::CompactionSummary(CompactionSummary::new(
                session_id,
                format!("source-{suffix}"),
                suffix,
                None,
                TurnId(index as u64 + 1),
            )),
            provider_replay: Vec::new(),
        })
        .collect()
}

fn recovered_session_at(
    session_id: &str,
    entries: &[TranscriptStorageNode],
    active_leaf_id: Option<&str>,
) -> AgentSession {
    let mut stored = StoredSession::new(session_id);
    stored.active_leaf_id = active_leaf_id.map(str::to_string);
    stored.entries = entries.iter().cloned().map(Into::into).collect();
    AgentSession::from_stored_session(stored).expect("test session recovers")
}

async fn create_active_leaf_test_session(
    env: &TestEnv,
    session_id: &str,
    entries: &[TranscriptStorageNode],
    active_leaf_id: Option<&str>,
) -> SessionConfig {
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "active leaf persistence test",
            &[],
            json!({}),
        )
        .await
        .expect("create active-leaf test project");
    let config = session_config(env, project_id, json!({}));
    env.state
        .repo
        .start_session_outputs(
            session_id,
            &config,
            entries,
            active_leaf_id,
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("setup"),
            None,
        )
        .await
        .expect("create active-leaf test session");
    config
}

#[tokio::test]
async fn persisted_active_leaf_tracks_set_change_and_clear() {
    let Some(env) = test_env().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let session_id = "persisted-active-leaf-transitions";
    let entries = active_leaf_test_entries(session_id);
    let first_leaf = entries[0].id.as_str();
    let second_leaf = entries[1].id.as_str();
    let config = create_active_leaf_test_session(&env, session_id, &entries, None).await;
    let active = Arc::new(Mutex::new(RuntimeSession {
        session: recovered_session_at(session_id, &entries, Some(first_leaf)),
        config,
        persisted_active_leaf_id: None,
    }));
    env.state
        .active
        .lock()
        .await
        .insert(session_id.to_string(), active.clone());
    let driver = SessionDriver::acquire(&env.state, session_id).await;

    for expected in [Some(first_leaf), Some(second_leaf), None] {
        active.lock().await.session = recovered_session_at(session_id, &entries, expected);
        driver
            .persist_active_outputs(active.clone(), None, None, None, Vec::new())
            .await
            .expect("active-leaf transition persists");
        assert_eq!(
            active.lock().await.persisted_active_leaf_id.as_deref(),
            expected
        );
        assert_eq!(
            env.state
                .repo
                .load_stored_session(session_id)
                .await
                .expect("stored session reloads")
                .active_leaf_id
                .as_deref(),
            expected
        );
    }

    drop(driver);
    env.cleanup().await;
}

#[tokio::test]
async fn failed_persistence_does_not_advance_persisted_active_leaf() {
    let Some(env) = test_env().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let session_id = "persisted-active-leaf-failure";
    let entries = active_leaf_test_entries(session_id);
    let first_leaf = entries[0].id.as_str();
    let second_leaf = entries[1].id.as_str();
    let config =
        create_active_leaf_test_session(&env, session_id, &entries, Some(first_leaf)).await;
    let active = Arc::new(Mutex::new(RuntimeSession {
        session: recovered_session_at(session_id, &entries, Some(second_leaf)),
        config,
        persisted_active_leaf_id: Some(first_leaf.to_string()),
    }));
    env.state
        .active
        .lock()
        .await
        .insert(session_id.to_string(), active.clone());
    let driver = SessionDriver::acquire(&env.state, session_id).await;
    env.state.repo.close().await;

    driver
        .persist_active_outputs(active.clone(), None, None, None, Vec::new())
        .await
        .expect_err("closed pool rejects active-leaf change");

    assert_eq!(
        active.lock().await.persisted_active_leaf_id.as_deref(),
        Some(first_leaf)
    );
    assert!(!env.state.active.lock().await.contains_key(session_id));
    drop(driver);
    env.cleanup().await;
}

fn successful_compaction(summary: &str) -> CompactionCompletion {
    CompactionCompletion {
        summary: summary.to_string(),
        summary_kind: "provider_text".to_string(),
        provider_replay: Vec::new(),
        provider: ProviderKind::OpenAi,
        usage: None,
        continuation_suffix: vec![ModelContextEntry {
            item: TranscriptItem::UserMessage(UserMessage::text(BLOCKED_USER_INSTRUCTION)),
            provider_replay: Vec::new(),
        }],
    }
}

async fn commit_post_compaction_dispatch(
    env: &TestEnv,
    project_id: Uuid,
    session_id: &str,
) -> (agent_store::PendingDispatchAction, String) {
    commit_post_compaction_dispatch_with_faults(env, project_id, session_id, json!({})).await
}

async fn commit_post_compaction_dispatch_with_faults(
    env: &TestEnv,
    project_id: Uuid,
    session_id: &str,
    faults: serde_json::Value,
) -> (agent_store::PendingDispatchAction, String) {
    let mut faults = faults.as_object().cloned().unwrap_or_default();
    faults
        .entry("pause_model_dispatch_before_provider".to_string())
        .or_insert(json!(true));
    faults
        .entry("post_compaction_heartbeat_interval_ms".to_string())
        .or_insert(json!(10));
    let compaction = faults.remove("compaction");
    let provider_model = faults
        .remove("provider_model")
        .and_then(|value| value.as_str().map(str::to_string));
    let mut metadata = json!({
        "created_by": "test",
        "fault_injection": faults
    });
    if let Some(compaction) = compaction {
        metadata["compaction"] = compaction;
    }
    let entries = vec![
        TranscriptStorageNode {
            id: format!("{session_id}_turn"),
            parent_id: None,
            timestamp_ms: 1_700_000_000_000,
            item: TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: format!("{session_id}_user"),
            parent_id: Some(format!("{session_id}_turn")),
            timestamp_ms: 1_700_000_000_001,
            item: TranscriptItem::UserMessage(UserMessage::text(BLOCKED_USER_INSTRUCTION)),
            provider_replay: Vec::new(),
        },
    ];
    let source_leaf = entries[1].id.clone();
    let action = SessionAction::RequestModel {
        action_id: ActionId(1),
        turn_id: TurnId(1),
        model_context: ModelContext::from_transcript_items(
            entries.iter().map(|entry| entry.item.clone()).collect(),
        ),
        context_leaf_id: Some(source_leaf.clone()),
    };
    let mut config = session_config(env, project_id, metadata);
    if let Some(provider_model) = provider_model {
        config.provider.model = provider_model;
    }
    let (_, actions) = env
        .state
        .repo
        .start_session_outputs(
            session_id,
            &config,
            &entries,
            Some(&source_leaf),
            &[],
            &[action],
            InputPriority::FollowUp,
            &UserMessage::text(BLOCKED_USER_INSTRUCTION),
            None,
        )
        .await
        .expect("session starts");
    let model = actions.into_iter().next().expect("model action persists");
    assert!(env
        .state
        .repo
        .claim_pending_model_action(session_id, &model.row_id, &model.attempt_id)
        .await
        .expect("initial model claims"));
    let compaction = env
        .state
        .repo
        .block_model_action_for_compaction(
            session_id,
            &model.row_id,
            &model.attempt_id,
            ActionStatus::Running,
            None,
            CompactionTrigger::Auto {
                reason: "provider overflow".to_string(),
            },
            None,
            Some(100_000),
        )
        .await
        .expect("overflow blocks model");
    let route_pool = sqlx::PgPool::connect(&database_url_with_name(&env.admin_url, &env.name))
        .await
        .expect("connect route assertion pool");
    let compaction_route: Option<serde_json::Value> =
        sqlx::query_scalar("select provider_config from actions where id=$1")
            .bind(&compaction.job.action_row_id)
            .fetch_one(&route_pool)
            .await
            .expect("read compaction route");
    assert!(
        compaction_route.is_none(),
        "non-recoverable compaction jobs do not duplicate the blocked model route"
    );
    route_pool.close().await;
    let mut future_default = config.clone();
    future_default.provider.reasoning_effort = ReasoningEffort::High;
    env.state
        .repo
        .configure_session(session_id, &future_default)
        .await
        .expect("change future-work default while model is blocked");
    let completed = env
        .state
        .repo
        .complete_compaction_action(
            &compaction.job,
            successful_compaction("restart-safe summary"),
        )
        .await
        .expect("compaction success transaction commits");
    let resumed = completed
        .resumed_model_action
        .expect("success transaction persists resumed model");
    let mut resumed_config = future_default;
    resumed.route.apply_to(&mut resumed_config);
    assert_eq!(
        resumed_config.provider.reasoning_effort,
        ReasoningEffort::Medium,
        "compaction resume retains the blocked model route"
    );
    let compacted_leaf = completed
        .active_leaf_id
        .expect("success transaction installs compacted leaf");
    (resumed, compacted_leaf)
}

#[tokio::test]
async fn expired_post_compaction_claim_is_reclaimed_after_real_boot_state_recreation() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "post-compaction boot recovery",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "post_compaction_boot_recovery";
    let (resumed, compacted_leaf) =
        commit_post_compaction_dispatch(&env, project_id, session_id).await;
    let ordinary_session_id = "ordinary_pending_boot_action";
    start_prepared_session(
        &env.state,
        PreparedSessionStart {
            session_id: ordinary_session_id.to_string(),
            config: session_config(
                &env,
                project_id,
                json!({ "created_by": "test", "harness": true }),
            ),
            priority: InputPriority::FollowUp,
            content: UserMessage::text("ordinary pending work"),
            client_input_id: None,
            parent_session_id: None,
            subagent_type: None,
            delegation_id: None,
            dispatch_mode: PreparedSessionDispatchMode::Deferred,
        },
    )
    .await
    .expect("ordinary pending session starts");

    let intent = agent_store::PostCompactionDispatchIntent {
        session_id: session_id.to_string(),
        row_id: resumed.row_id.clone(),
        attempt_id: resumed.attempt_id.clone(),
    };
    let crashed_claim = env
        .state
        .repo
        .claim_post_compaction_model_action(&intent, std::time::Duration::from_secs(30))
        .await
        .expect("fault injection commits pending-to-running lease")
        .expect("pending intent claims");
    assert_eq!(crashed_claim.lease.generation, 1);
    assert_eq!(crashed_claim.lease.context_leaf_id, compacted_leaf);
    assert!(
        env.state
            .repo
            .claim_post_compaction_model_action(&intent, std::time::Duration::from_secs(30))
            .await
            .expect("repeat live-lease claim is a clean no-op")
            .is_none(),
        "an unexpired owner cannot be claimed concurrently"
    );

    let database_url = database_url_with_name(&env.admin_url, &env.name);
    expire_post_compaction_lease(
        &database_url,
        session_id,
        &resumed.row_id,
        &resumed.attempt_id,
    )
    .await;

    // This is the injected crash point: the lease commit above is durable, but
    // no spawn/register happened. Discard every volatile runtime/store object
    // and construct the state a new daemon process would own.
    env.state.repo.close().await;
    let restarted_store = PostgresAgentStore::connect(&database_url)
        .await
        .expect("restart opens a new store");
    restarted_store.migrate().await.expect("restart migrates");
    let _restarted_state_dir = TempDir::new("restart-state");
    let (events, _rx) = broadcast::channel(1024);
    let repo = Arc::new(restarted_store);
    let runtime_hosts = RuntimeRegistry::new(repo.clone());
    connect_test_runtime(&runtime_hosts, TEST_RUNTIME_ID).await;
    let restarted_state = AppState {
        repo,
        active: Arc::new(Mutex::new(HashMap::new())),
        session_driver_locks: Arc::new(Mutex::new(HashMap::new())),
        tasks: Arc::new(StdMutex::new(HashMap::new())),
        auxiliary_tasks: Arc::new(StdMutex::new(Vec::new())),
        task_registration_lock: Arc::new(StdMutex::new(())),
        post_compaction_recovery_scheduled: Arc::new(AtomicBool::new(false)),
        post_compaction_recovery_notify: Arc::new(tokio::sync::Notify::new()),
        post_compaction_recovery_task: Arc::new(StdMutex::new(None)),
        shutting_down: Arc::new(AtomicBool::new(false)),
        events,
        tools: Arc::new(ToolRegistry::with_builtin_tools()),
        mcp: agent_mcp::McpManager::disabled(),
        provider_connections: ProviderConnectionRegistry::new(),
        session_titles: SessionTitleScheduler::disabled(),
        runtime_hosts,
        prompt_root: env.cwd.path().to_path_buf(),
        config_root: env.cwd.path().to_path_buf(),
        daemon_config: crate::config::DaemonConfig::default(),
        pause_subagent_control_after_commit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        subagent_control_committed: Arc::new(tokio::sync::Notify::new()),
        fail_subagent_control_reload_after_commit: Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )),
    };

    restarted_state
        .repo
        .mark_all_unfinished_actions_stale()
        .await
        .expect("production boot stale sweep runs");
    let after_sweep = restarted_state
        .repo
        .session_snapshot(session_id)
        .await
        .unwrap();
    let leased = after_sweep
        .pending_actions
        .iter()
        .find(|action| action.action_row_id == resumed.row_id)
        .expect("expired leased intent survives the narrow stale-sweep exception");
    assert_eq!(leased.status, ActionStatus::Running);
    assert_eq!(
        leased
            .payload
            .pointer("/post_compaction_dispatch/lease/generation"),
        Some(&json!(1))
    );
    let ordinary_after_sweep = restarted_state
        .repo
        .find_resumable_model_action(ordinary_session_id, TurnId(1))
        .await
        .expect("ordinary stale action loads")
        .expect("ordinary pending action was terminalized");
    assert_eq!(ordinary_after_sweep.status, ActionStatus::Stale);

    assert_eq!(
        recover_post_compaction_dispatches_on_boot(&restarted_state)
            .await
            .expect("boot recovery succeeds"),
        1,
        "production recovery reclaims and registers the expired exact attempt"
    );
    let active = restarted_state.active.lock().await.get(session_id).cloned();
    assert!(
        active.is_some(),
        "boot recovery restores the live outstanding model runtime"
    );
    assert!(
        restarted_state
            .repo
            .load_harness_model_action(session_id, &resumed.row_id)
            .await
            .expect("reclaimed model action remains observable")
            .post_compaction_dispatch_lease
            .as_ref()
            .is_some_and(|lease| {
                lease.generation == 2
                    && lease.owner_id != crashed_claim.lease.owner_id
                    && lease.context_leaf_id == compacted_leaf
            }),
        "reclaim installs a new fenced generation on the same row/attempt/leaf"
    );
    assert_eq!(
        restarted_state
            .tasks
            .lock()
            .expect("task registry lock")
            .len(),
        1,
        "the non-harness production spawn path registered exactly one runner"
    );
    assert_eq!(
        recover_post_compaction_dispatches_on_boot(&restarted_state)
            .await
            .expect("repeat recovery succeeds"),
        0,
        "an unexpired live lease is not concurrently redispatched"
    );
    assert_eq!(
        restarted_state
            .tasks
            .lock()
            .expect("task registry lock")
            .len(),
        1,
        "repeat recovery does not register a duplicate runner"
    );

    let reclaimed = restarted_state
        .repo
        .load_harness_model_action(session_id, &resumed.row_id)
        .await
        .expect("load reclaimed action");
    let reclaimed_lease = reclaimed
        .post_compaction_dispatch_lease
        .expect("reclaimed action has lease");
    for handle in take_tasks(&restarted_state) {
        handle.abort();
    }
    let driver = SessionDriver::acquire(&restarted_state, session_id).await;
    let active = driver
        .active_session()
        .await
        .expect("recovered runtime remains active");
    let SessionAction::RequestModel {
        action_id, turn_id, ..
    } = crashed_claim.pending.action.clone()
    else {
        unreachable!()
    };
    let recovered_config = restarted_state
        .repo
        .load_session_config(session_id)
        .await
        .expect("load recovered config");
    let terminal_dispatch = DispatchAction {
        row_id: resumed.row_id.clone(),
        attempt_id: resumed.attempt_id.clone(),
        post_compaction_dispatch_lease: Some(reclaimed_lease),
        action: crashed_claim.pending.action,
        mcp_snapshot: mcp_snapshot_for_session(&recovered_config).expect("recovered MCP snapshot"),
        config: recovered_config,
    };
    apply_model_response(
        &restarted_state,
        session_id,
        &driver,
        active,
        &terminal_dispatch,
        action_id,
        turn_id,
        ModelResponse {
            assistant: AssistantMessage {
                items: vec![AssistantItem::Text("recovered completion".to_string())],
            },
            provider_replay: Vec::new(),
            usage: None,
            stop_reason: ModelStopReason::Complete,
            stop_details: None,
        },
        "recovered-test-toolset",
    )
    .await
    .expect("ordinary terminal completion commits");
    let stored = restarted_state
        .repo
        .load_stored_session(session_id)
        .await
        .expect("completed compacted session reloads");
    let active_leaf = stored.active_leaf_id.expect("completed branch has a leaf");
    let context = restarted_state
        .repo
        .model_context_for_leaf(session_id, &active_leaf)
        .await
        .expect("completed compacted branch loads");
    assert!(matches!(
        context.transcript_items(),
        [
            TranscriptItem::CompactionSummary(_),
            TranscriptItem::UserMessage(user),
            TranscriptItem::AssistantMessage(_),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ] if user == &UserMessage::text(BLOCKED_USER_INSTRUCTION)
    ));
    let terminal_pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect terminal assertion pool");
    let terminal = sqlx::query(
        "select status, result, payload from actions where session_id=$1 and id=$2 and attempt_id=$3",
    )
    .bind(session_id)
    .bind(&resumed.row_id)
    .bind(&resumed.attempt_id)
    .fetch_one(&terminal_pool)
    .await
    .expect("terminal action remains observable");
    assert_eq!(terminal.get::<String, _>("status"), "completed");
    assert_eq!(
        terminal
            .get::<serde_json::Value, _>("result")
            .get("stop_reason")
            .and_then(serde_json::Value::as_str),
        Some("complete")
    );
    assert!(
        terminal
            .get::<serde_json::Value, _>("payload")
            .get("post_compaction_dispatch")
            .is_none(),
        "ordinary terminal completion atomically clears the retained recovery intent"
    );
    terminal_pool.close().await;
    restarted_state.repo.close().await;

    env.cleanup().await;
}

#[tokio::test]
async fn overlapping_boot_recovery_claims_one_runner_across_independent_states() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "overlapping boot recovery",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "overlapping_post_compaction_boot_recovery";
    let (resumed, compacted_leaf) =
        commit_post_compaction_dispatch(&env, project_id, session_id).await;
    let intent = agent_store::PostCompactionDispatchIntent {
        session_id: session_id.to_string(),
        row_id: resumed.row_id.clone(),
        attempt_id: resumed.attempt_id.clone(),
    };
    let expired_claim = env
        .state
        .repo
        .claim_post_compaction_model_action(&intent, std::time::Duration::from_secs(30))
        .await
        .expect("generation one claims")
        .expect("generation one exists");
    assert_eq!(expired_claim.lease.generation, 1);

    let database_url = database_url_with_name(&env.admin_url, &env.name);
    expire_post_compaction_lease(
        &database_url,
        session_id,
        &resumed.row_id,
        &resumed.attempt_id,
    )
    .await;
    env.state.repo.close().await;

    let store_a = PostgresAgentStore::connect(&database_url)
        .await
        .expect("first daemon store connects");
    let store_b = PostgresAgentStore::connect(&database_url)
        .await
        .expect("second daemon store connects");
    let state_dir_a = TempDir::new("overlap-a");
    let state_dir_b = TempDir::new("overlap-b");
    let state_a = test_app_state(store_a, &state_dir_a, env.cwd.path().to_path_buf()).await;
    let state_b = test_app_state(store_b, &state_dir_b, env.cwd.path().to_path_buf()).await;
    state_a
        .repo
        .mark_all_unfinished_actions_stale()
        .await
        .expect("production stale sweep preserves marked action");

    let (recovered_a, recovered_b) = tokio::join!(
        recover_post_compaction_dispatches_on_boot(&state_a),
        recover_post_compaction_dispatches_on_boot(&state_b)
    );
    let mut recovered = [
        recovered_a.expect("first recovery succeeds"),
        recovered_b.expect("second recovery succeeds"),
    ];
    recovered.sort_unstable();
    assert_eq!(recovered, [0, 1]);

    let reclaimed = state_a
        .repo
        .load_harness_model_action(session_id, &resumed.row_id)
        .await
        .expect("reclaimed action loads");
    let lease = reclaimed
        .post_compaction_dispatch_lease
        .expect("generation two lease persists");
    assert_eq!(lease.generation, 2);
    assert_ne!(lease.owner_id, expired_claim.lease.owner_id);
    assert_eq!(lease.context_leaf_id, compacted_leaf);
    let registered_a = state_a
        .tasks
        .lock()
        .expect("first task registry lock")
        .len();
    let registered_b = state_b
        .tasks
        .lock()
        .expect("second task registry lock")
        .len();
    assert_eq!(
        registered_a + registered_b,
        1,
        "the lease CAS allows one paused pre-provider runner across both daemons"
    );
    assert_eq!(
        state_a
            .repo
            .load_session_config(session_id)
            .await
            .expect("load pause fault")
            .metadata
            .pointer("/fault_injection/pause_model_dispatch_before_provider"),
        Some(&json!(true)),
        "the only registered runner is stopped before any provider call"
    );
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_eq!(
        state_a
            .tasks
            .lock()
            .expect("first task registry lock")
            .len()
            + state_b
                .tasks
                .lock()
                .expect("second task registry lock")
                .len(),
        1,
        "the pre-provider pause keeps one winner and cannot issue a duplicate provider call"
    );

    for handle in take_tasks(&state_a).into_iter().chain(take_tasks(&state_b)) {
        handle.abort();
    }
    state_a.repo.close().await;
    state_b.repo.close().await;
    env.cleanup().await;
}

#[tokio::test]
async fn lost_lease_runner_exit_rearms_recovery_without_process_restart() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "same-process lease recovery",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "same_process_post_compaction_recovery";
    let (resumed, _) = commit_post_compaction_dispatch(&env, project_id, session_id).await;

    assert_eq!(
        recover_post_compaction_dispatches_on_boot(&env.state)
            .await
            .expect("initial recovery claims generation one"),
        1
    );
    let first_lease = env
        .state
        .repo
        .load_harness_model_action(session_id, &resumed.row_id)
        .await
        .expect("generation one action loads")
        .post_compaction_dispatch_lease
        .expect("generation one lease persists");
    assert_eq!(first_lease.generation, 1);
    let database_url = database_url_with_name(&env.admin_url, &env.name);
    expire_post_compaction_lease(
        &database_url,
        session_id,
        &resumed.row_id,
        &resumed.attempt_id,
    )
    .await;

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let action = env
                .state
                .repo
                .load_harness_model_action(session_id, &resumed.row_id)
                .await
                .expect("action remains observable");
            if action
                .post_compaction_dispatch_lease
                .as_ref()
                .is_some_and(|lease| {
                    lease.generation == 2 && lease.owner_id != first_lease.owner_id
                })
                && env.state.tasks.lock().expect("task registry lock").len() == 1
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("heartbeat loss wakes watchdog and generation two registers");

    assert!(
        env.state.active.lock().await.contains_key(session_id),
        "same-process recovery retains a reconstructed live runtime"
    );
    env.cleanup().await;
}

#[tokio::test]
async fn heartbeat_loss_after_terminal_commit_still_registers_persisted_successor() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "heartbeat terminal handoff",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "heartbeat_terminal_handoff";
    let (resumed, _) = commit_post_compaction_dispatch_with_faults(
        &env,
        project_id,
        session_id,
        json!({
            "pause_model_dispatch_before_provider": false,
            "post_compaction_heartbeat_interval_ms": 5,
            "pause_after_model_transition_ms": 100,
            "pause_tool_dispatch_before_run": true,
            "model_result": "tool"
        }),
    )
    .await;

    assert_eq!(
        recover_post_compaction_dispatches_on_boot(&env.state)
            .await
            .expect("recovery claims model"),
        1
    );

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let snapshot = env.state.repo.session_snapshot(session_id).await.unwrap();
            let completed_source = snapshot.pending_actions.iter().all(|action| {
                action.action_row_id != resumed.row_id || action.status == ActionStatus::Completed
            });
            let tool_registered = env
                .state
                .tasks
                .lock()
                .expect("task registry lock")
                .values()
                .any(|task| task.kind == agent_store::ActionKind::Tool);
            if completed_source && tool_registered {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("terminal commit survives false renewal and registers tool successor");

    let pool = sqlx::PgPool::connect(&database_url_with_name(&env.admin_url, &env.name))
        .await
        .expect("connect assertion pool");
    let marker_count: i64 = sqlx::query_scalar(
        "select count(*)::bigint from actions where session_id=$1 and payload ? 'post_compaction_dispatch'",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("count markers");
    assert_eq!(marker_count, 0, "terminal commit clears the exact marker");
    let tool_effort: String = sqlx::query_scalar(
        r#"
        select provider_config->>'reasoning_effort'
        from actions
        where session_id=$1 and kind='tool'
        order by created_at desc
        limit 1
        "#,
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("read persisted tool continuation route");
    assert_eq!(
        tool_effort, "medium",
        "tool continuation from recovered model retains the blocked generation route"
    );
    assert!(
        crate::runtime::runner_start_count(session_id, "tool") >= 1,
        "successor crosses a tracked start barrier"
    );
    pool.close().await;
    env.cleanup().await;
}

#[tokio::test]
async fn unknown_model_explicit_auto_recovers_overflow_across_heartbeat_loss() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "heartbeat compaction handoff",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "heartbeat_compaction_handoff";
    let (resumed, _) = commit_post_compaction_dispatch_with_faults(
        &env,
        project_id,
        session_id,
        json!({
            "pause_model_dispatch_before_provider": false,
            "post_compaction_heartbeat_interval_ms": 5,
            "pause_after_reactive_compaction_transition_ms": 100,
            "pause_compaction_dispatch_before_provider": true,
            "model_provider_max_attempts": 1,
            "model_result": "overflow",
            "provider_model": "unknown-reactive-only-model",
            "compaction": {
                "config": {
                    "auto_enabled": true
                }
            }
        }),
    )
    .await;

    assert_eq!(
        recover_post_compaction_dispatches_on_boot(&env.state)
            .await
            .expect("recovery claims model"),
        1
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let snapshot = env.state.repo.session_snapshot(session_id).await.unwrap();
            let blocked = snapshot.pending_actions.iter().any(|action| {
                action.action_row_id == resumed.row_id && action.status == ActionStatus::Blocked
            });
            let compaction_registered = env
                .state
                .tasks
                .lock()
                .expect("task registry lock")
                .values()
                .any(|task| task.kind == agent_store::ActionKind::Compaction);
            if blocked && compaction_registered {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("reactive transition survives false renewal and registers compaction");
    let snapshot = env.state.repo.session_snapshot(session_id).await.unwrap();
    assert!(
        snapshot
            .pending_actions
            .iter()
            .any(|action| action.kind == agent_store::ActionKind::Compaction
                && action.status == ActionStatus::Running),
        "durable running compaction is not stranded without its local runner"
    );
    env.cleanup().await;
}

#[tokio::test]
async fn shutdown_rejects_recovery_runner_between_claim_and_register() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "shutdown recovery registration",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "shutdown_recovery_registration";
    let (resumed, _) = commit_post_compaction_dispatch_with_faults(
        &env,
        project_id,
        session_id,
        json!({
            "pause_model_dispatch_before_provider": false,
            "pause_recovery_before_register_ms": 150,
            "model_result": "complete"
        }),
    )
    .await;

    let recovery_state = env.state.clone();
    let recovery =
        tokio::spawn(
            async move { recover_post_compaction_dispatches_on_boot(&recovery_state).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let action = env
                .state
                .repo
                .load_harness_model_action(session_id, &resumed.row_id)
                .await
                .expect("claimed action remains visible");
            if action.post_compaction_dispatch_lease.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("recovery reaches claimed pre-registration pause");

    let handles = take_tasks(&env.state);
    for handle in handles {
        handle.await.ok();
    }
    recovery
        .await
        .expect("recovery task joins")
        .expect("shutdown rejection is recoverable");
    assert!(
        env.state
            .tasks
            .lock()
            .expect("task registry lock")
            .is_empty(),
        "drain leaves no untracked runner"
    );
    assert!(
        env.state
            .auxiliary_tasks
            .lock()
            .expect("auxiliary task registry lock")
            .is_empty(),
        "drain leaves no untracked auxiliary task"
    );
    assert!(
        env.state
            .post_compaction_recovery_task
            .lock()
            .expect("recovery task registry lock")
            .is_none(),
        "drain leaves no untracked watchdog"
    );
    assert_eq!(
        crate::provider_runtime::injected_provider_start_count(session_id),
        0,
        "shutdown rejection occurs before provider I/O"
    );
    let action = env
        .state
        .repo
        .load_harness_model_action(session_id, &resumed.row_id)
        .await
        .expect("claimed durable action remains");
    assert!(
        action.post_compaction_dispatch_lease.is_some(),
        "claimed lease is retained for expiry and next-boot recovery"
    );
    env.cleanup().await;
}

#[tokio::test]
async fn shutdown_rejects_successor_runner_from_existing_task() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "shutdown successor registration",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "shutdown_successor_registration";
    let (_resumed, _) = commit_post_compaction_dispatch_with_faults(
        &env,
        project_id,
        session_id,
        json!({
            "pause_model_dispatch_before_provider": false,
            "pause_after_model_transition_ms": 150,
            "model_result": "tool"
        }),
    )
    .await;
    assert_eq!(
        recover_post_compaction_dispatches_on_boot(&env.state)
            .await
            .expect("recovery starts source runner"),
        1
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let snapshot = env.state.repo.session_snapshot(session_id).await.unwrap();
            if snapshot.pending_actions.iter().any(|action| {
                action.kind == agent_store::ActionKind::Tool
                    && action.status == ActionStatus::Pending
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("source commits successor before local dispatch");

    let handles = take_tasks(&env.state);
    for mut handle in handles {
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut handle)
            .await
            .expect("drained source task finishes")
            .ok();
    }
    assert_eq!(
        crate::runtime::runner_start_count(session_id, "tool"),
        0,
        "successor cannot cross its start barrier after shutdown"
    );
    assert!(
        env.state
            .tasks
            .lock()
            .expect("task registry lock")
            .is_empty(),
        "main drain cannot miss a late successor handle"
    );
    assert!(
        env.state
            .auxiliary_tasks
            .lock()
            .expect("auxiliary task registry lock")
            .is_empty(),
        "main drain cannot miss a late auxiliary handle"
    );
    assert!(
        env.state
            .post_compaction_recovery_task
            .lock()
            .expect("recovery task registry lock")
            .is_none(),
        "main drain cannot miss the watchdog handle"
    );
    let snapshot = env.state.repo.session_snapshot(session_id).await.unwrap();
    assert!(
        snapshot.pending_actions.iter().any(|action| {
            action.kind == agent_store::ActionKind::Tool && action.status == ActionStatus::Pending
        }),
        "shutdown leaves the unclaimed durable successor recoverable"
    );
    env.cleanup().await;
}

#[tokio::test]
async fn watchdog_retries_after_transient_recovery_database_error() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "transient recovery error",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "transient_post_compaction_recovery_error";
    let (resumed, _) = commit_post_compaction_dispatch(&env, project_id, session_id).await;
    let database_url = database_url_with_name(&env.admin_url, &env.name);
    let fault_pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect scheduler fault pool");
    sqlx::query("alter table actions rename to actions_scheduler_fault")
        .execute(&fault_pool)
        .await
        .expect("hide actions table");

    assert!(
        recover_post_compaction_dispatches_on_boot(&env.state)
            .await
            .is_err(),
        "the production boot sweep observes the transient database error"
    );
    assert!(
        env.state
            .post_compaction_recovery_scheduled
            .load(Ordering::Acquire),
        "the watchdog is armed before the fallible initial sweep"
    );
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(
        env.state
            .post_compaction_recovery_task
            .lock()
            .expect("watchdog task lock")
            .as_ref()
            .is_some_and(|task| !task.is_finished()),
        "the watchdog survives failed database inspection/recovery cycles"
    );

    sqlx::query("alter table actions_scheduler_fault rename to actions")
        .execute(&fault_pool)
        .await
        .expect("restore actions table");
    fault_pool.close().await;
    env.state.post_compaction_recovery_notify.notify_one();

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let action = env
                .state
                .repo
                .load_harness_model_action(session_id, &resumed.row_id)
                .await
                .expect("action loads after database recovery");
            if action
                .post_compaction_dispatch_lease
                .as_ref()
                .is_some_and(|lease| lease.generation == 1)
                && env.state.tasks.lock().expect("task registry lock").len() == 1
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watchdog survives the transient error and recovers the marker");

    env.cleanup().await;
}

#[tokio::test]
async fn watchdog_retries_transient_per_intent_claim_failure_without_compensation() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "transient intent load failure",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "transient_intent_load_failure";
    let (resumed, _) = commit_post_compaction_dispatch(&env, project_id, session_id).await;
    let pool = sqlx::PgPool::connect(&database_url_with_name(&env.admin_url, &env.name))
        .await
        .expect("connect fault pool");
    sqlx::query("alter table transcript_entries rename to transcript_entries_intent_fault")
        .execute(&pool)
        .await
        .expect("install load fault");

    assert!(
        recover_post_compaction_dispatches_on_boot(&env.state)
            .await
            .is_err(),
        "per-intent context load error propagates as transient"
    );
    let during_fault = env
        .state
        .repo
        .load_harness_model_action(session_id, &resumed.row_id)
        .await
        .expect("marker remains after transient error");
    assert!(
        during_fault
            .post_compaction_dispatch_context_leaf_id
            .is_some(),
        "transient failure cannot terminally remove the marker"
    );
    sqlx::query("alter table transcript_entries_intent_fault rename to transcript_entries")
        .execute(&pool)
        .await
        .expect("remove load fault");
    pool.close().await;
    env.state.post_compaction_recovery_notify.notify_one();

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let action = env
                .state
                .repo
                .load_harness_model_action(session_id, &resumed.row_id)
                .await
                .expect("action loads after fault removal");
            if action
                .post_compaction_dispatch_lease
                .as_ref()
                .is_some_and(|lease| lease.generation == 1)
                && env.state.tasks.lock().expect("task registry lock").len() == 1
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watchdog autonomously retries the retained marker");
    env.cleanup().await;
}

#[tokio::test]
async fn stale_corruption_compensation_cannot_fail_newer_generation() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "stale corruption fence",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "stale_corruption_fence";
    let (resumed, _) = commit_post_compaction_dispatch(&env, project_id, session_id).await;
    let intent = agent_store::PostCompactionDispatchIntent {
        session_id: session_id.to_string(),
        row_id: resumed.row_id.clone(),
        attempt_id: resumed.attempt_id.clone(),
    };
    let pool = sqlx::PgPool::connect(&database_url_with_name(&env.admin_url, &env.name))
        .await
        .expect("connect assertion pool");
    let first = env
        .state
        .repo
        .claim_post_compaction_model_action(
            &intent,
            agent_store::POST_COMPACTION_DISPATCH_LEASE_DURATION,
        )
        .await
        .expect("generation one claim succeeds")
        .expect("generation one exists");
    assert_eq!(first.lease.generation, 1);
    sqlx::query(
        r#"
        update actions
        set payload=jsonb_set(
                jsonb_set(
                    payload,
                    '{post_compaction_dispatch,kind}',
                    '"corrupt-kind"'::jsonb
                ),
                '{post_compaction_dispatch,lease,expires_at_ms}',
                '1'::jsonb
            )
        where session_id=$1 and id=$2 and attempt_id=$3
        "#,
    )
    .bind(session_id)
    .bind(&resumed.row_id)
    .bind(&resumed.attempt_id)
    .execute(&pool)
    .await
    .expect("install deterministic marker corruption");
    let corrupt = match env
        .state
        .repo
        .claim_post_compaction_model_action(
            &intent,
            agent_store::POST_COMPACTION_DISPATCH_LEASE_DURATION,
        )
        .await
    {
        Err(agent_store::PostCompactionDispatchClaimError::Corrupt(error)) => error,
        other => panic!("expected typed corruption, got {other:?}"),
    };

    sqlx::query(
        r#"
        update actions
        set payload=jsonb_set(
            payload,
            '{post_compaction_dispatch,kind}',
            '"resume_model_v1"'::jsonb
        )
        where session_id=$1 and id=$2 and attempt_id=$3
        "#,
    )
    .bind(session_id)
    .bind(&resumed.row_id)
    .bind(&resumed.attempt_id)
    .execute(&pool)
    .await
    .expect("repair marker for newer claim");
    let newer = env
        .state
        .repo
        .claim_post_compaction_model_action(
            &intent,
            agent_store::POST_COMPACTION_DISPATCH_LEASE_DURATION,
        )
        .await
        .expect("generation two claim succeeds")
        .expect("expired generation one is reclaimed");
    assert_eq!(newer.lease.generation, 2);
    assert_ne!(newer.lease.owner_id, first.lease.owner_id);
    let events = env
        .state
        .repo
        .fail_corrupt_post_compaction_model_action(&intent, corrupt.fence(), corrupt.message())
        .await
        .expect("stale compensation is a clean no-op");
    assert!(events.is_empty());
    let row = sqlx::query(
        "select status, payload from actions where session_id=$1 and id=$2 and attempt_id=$3",
    )
    .bind(session_id)
    .bind(&resumed.row_id)
    .bind(&resumed.attempt_id)
    .fetch_one(&pool)
    .await
    .expect("load newer generation");
    assert_eq!(row.get::<String, _>("status"), "running");
    assert_eq!(
        row.get::<serde_json::Value, _>("payload")
            .pointer("/post_compaction_dispatch/lease/generation"),
        Some(&json!(2)),
        "stale corruption compensation cannot terminally fail newer ownership"
    );
    pool.close().await;
    env.cleanup().await;
}

#[tokio::test]
async fn corrupt_post_compaction_dispatch_is_terminally_observable_on_boot() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "corrupt post-compaction boot recovery",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let session_id = "corrupt_post_compaction_boot_recovery";
    let (resumed, _) = commit_post_compaction_dispatch(&env, project_id, session_id).await;
    let pool = sqlx::PgPool::connect(&database_url_with_name(&env.admin_url, &env.name))
        .await
        .expect("connect assertion pool");
    sqlx::query(
        r#"
        update actions
        set payload=jsonb_set(
            payload,
            '{post_compaction_dispatch,attempt_id}',
            '"corrupt-attempt"'::jsonb
        )
        where session_id=$1 and id=$2 and attempt_id=$3
        "#,
    )
    .bind(session_id)
    .bind(&resumed.row_id)
    .bind(&resumed.attempt_id)
    .execute(&pool)
    .await
    .expect("corrupt committed marker");

    env.state
        .repo
        .mark_all_unfinished_actions_stale()
        .await
        .expect("production boot stale sweep runs");
    assert_eq!(
        recover_post_compaction_dispatches_on_boot(&env.state)
            .await
            .expect("terminal recovery succeeds"),
        0,
        "corrupt intent is terminal recovery, not a dispatch"
    );
    let row = sqlx::query(
        "select status, result from actions where session_id=$1 and id=$2 and attempt_id=$3",
    )
    .bind(session_id)
    .bind(&resumed.row_id)
    .bind(&resumed.attempt_id)
    .fetch_one(&pool)
    .await
    .expect("load terminal action");
    assert_eq!(row.get::<String, _>("status"), "error");
    let result = row.get::<serde_json::Value, _>("result");
    assert!(
        result
            .get("error")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|error| error.contains("marker attempt does not match")),
        "terminal result explains corrupt reconstruction: {result}"
    );
    let error_events: i64 = sqlx::query_scalar(
        r#"
        select count(*)::bigint
        from events
        where session_id=$1
            and type='model.error'
            and payload->>'action_row_id'=$2
        "#,
    )
    .bind(session_id)
    .bind(&resumed.row_id)
    .fetch_one(&pool)
    .await
    .expect("count recovery error events");
    assert_eq!(error_events, 1);
    assert_eq!(
        recover_post_compaction_dispatches_on_boot(&env.state)
            .await
            .expect("repeat terminal recovery succeeds"),
        0,
        "terminal corrupt intent remains idempotent"
    );
    env.cleanup().await;
}

#[tokio::test]
async fn immediate_post_compaction_overflow_never_strands_blocked_model_action() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "post-compaction overflow",
            &[],
            json!({}),
        )
        .await
        .expect("create project");

    for second_compaction_succeeds in [true, false] {
        let session_id = if second_compaction_succeeds {
            "post_compaction_overflow_success"
        } else {
            "post_compaction_overflow_failure"
        };
        let entries = vec![
            TranscriptStorageNode {
                id: format!("{session_id}_turn"),
                parent_id: None,
                timestamp_ms: 1_700_000_000_000,
                item: TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                provider_replay: Vec::new(),
            },
            TranscriptStorageNode {
                id: format!("{session_id}_user"),
                parent_id: Some(format!("{session_id}_turn")),
                timestamp_ms: 1_700_000_000_001,
                item: TranscriptItem::UserMessage(UserMessage::text(BLOCKED_USER_INSTRUCTION)),
                provider_replay: Vec::new(),
            },
        ];
        let source_leaf = entries[1].id.clone();
        let model_context = ModelContext::from_transcript_items(
            entries.iter().map(|entry| entry.item.clone()).collect(),
        );
        let action = SessionAction::RequestModel {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            model_context,
            context_leaf_id: Some(source_leaf.clone()),
        };
        let metadata = if second_compaction_succeeds {
            json!({
                "created_by": "test",
                "fault_injection": {
                    "model_result": "overflow",
                    "model_provider_max_attempts": 1
                },
                "compaction": { "config": {
                    "auto_enabled": true,
                    "auto_limit_tokens": 100_000
                }}
            })
        } else {
            json!({ "created_by": "test" })
        };
        let (_, actions) = env
            .state
            .repo
            .start_session_outputs(
                session_id,
                &session_config(&env, project_id, metadata),
                &entries,
                Some(&source_leaf),
                &[],
                &[action],
                InputPriority::FollowUp,
                &UserMessage::text(BLOCKED_USER_INSTRUCTION),
                None,
            )
            .await
            .expect("session starts");
        let model = actions.into_iter().next().expect("model action persists");
        assert!(env
            .state
            .repo
            .claim_pending_model_action(session_id, &model.row_id, &model.attempt_id)
            .await
            .expect("initial model claims"));

        let first = env
            .state
            .repo
            .block_model_action_for_compaction(
                session_id,
                &model.row_id,
                &model.attempt_id,
                ActionStatus::Running,
                None,
                CompactionTrigger::Auto {
                    reason: "first overflow".to_string(),
                },
                None,
                Some(100_000),
            )
            .await
            .expect("first overflow blocks model");
        let first_result = env
            .state
            .repo
            .complete_compaction_action(&first.job, successful_compaction("first summary"))
            .await
            .expect("first compaction completes");
        let resumed = first_result
            .resumed_model_action
            .expect("first compaction resumes model");
        let first_claim = env
            .state
            .repo
            .claim_post_compaction_model_action(
                &agent_store::PostCompactionDispatchIntent {
                    session_id: session_id.to_string(),
                    row_id: resumed.row_id.clone(),
                    attempt_id: resumed.attempt_id.clone(),
                },
                agent_store::POST_COMPACTION_DISPATCH_LEASE_DURATION,
            )
            .await
            .expect("resumed model claim succeeds")
            .expect("resumed model claims");

        let second = env
            .state
            .repo
            .block_model_action_for_compaction(
                session_id,
                &resumed.row_id,
                &resumed.attempt_id,
                ActionStatus::Running,
                Some(&first_claim.lease),
                CompactionTrigger::Auto {
                    reason: "immediate overflow after first compaction".to_string(),
                },
                None,
                Some(100_000),
            )
            .await
            .expect("second overflow blocks model");
        assert!(
            matches!(second.job.scope, CompactionScope::MidTurn { .. }),
            "a CompactionSummary root must not erase the blocked-action lifecycle"
        );

        if second_compaction_succeeds {
            let fault_pool =
                sqlx::PgPool::connect(&database_url_with_name(&env.admin_url, &env.name))
                    .await
                    .expect("connect fault injector");
            install_compaction_metadata_fault(&fault_pool).await;
            let injected_failure = env
                .state
                .repo
                .complete_compaction_action(
                    &second.job,
                    successful_compaction("rolled back second summary"),
                )
                .await;
            assert!(
                injected_failure.is_err(),
                "metadata fault rolls back the whole completion"
            );
            let rolled_back = env.state.repo.session_snapshot(session_id).await.unwrap();
            assert_eq!(
                rolled_back.active_leaf_id.as_deref(),
                Some(second.job.source_leaf_id.as_str())
            );
            assert!(rolled_back.pending_actions.iter().any(|action| {
                action.action_row_id == resumed.row_id && action.status == ActionStatus::Blocked
            }));
            assert!(rolled_back.pending_actions.iter().any(|action| {
                action.action_row_id == second.job.action_row_id
                    && action.status == ActionStatus::Running
            }));
            assert_eq!(
                rolled_back
                    .metadata
                    .pointer("/compaction/auto_state/consecutive_recompactions"),
                Some(&json!(0))
            );
            remove_compaction_metadata_fault(&fault_pool).await;
            fault_pool.close().await;

            let second_result = env
                .state
                .repo
                .complete_compaction_action(&second.job, successful_compaction("second summary"))
                .await
                .expect("second compaction completes");
            assert!(
                second_result.resumed_model_action.is_some(),
                "successful second compaction must resume the blocked model"
            );
            let second_leaf = second_result
                .active_leaf_id
                .as_deref()
                .expect("second compaction installs an active leaf");
            assert_ne!(
                second_result.new_root_id.as_deref(),
                Some(second_leaf),
                "the retained user suffix makes the installed leaf nonempty"
            );
            let snapshot = env.state.repo.session_snapshot(session_id).await.unwrap();
            assert!(snapshot.pending_actions.iter().any(|action| {
                action.action_row_id == resumed.row_id && action.status == ActionStatus::Pending
            }));
            assert_eq!(
                snapshot
                    .metadata
                    .pointer("/compaction/auto_state/consecutive_recompactions"),
                Some(&json!(1)),
                "the immediate recompaction bound commits with action resumption"
            );
            assert_eq!(
                snapshot
                    .metadata
                    .pointer("/compaction/auto_state/last_success_leaf_id")
                    .and_then(serde_json::Value::as_str),
                Some(second_leaf),
            );

            // Simulate a daemon restart/config reload through a fresh pool.
            let reloaded_store =
                PostgresAgentStore::connect(&database_url_with_name(&env.admin_url, &env.name))
                    .await
                    .expect("reconnect test database");
            let reloaded = reloaded_store
                .load_session_config(session_id)
                .await
                .expect("reload session config after recompaction");
            assert_eq!(
                reloaded
                    .metadata
                    .pointer("/compaction/auto_state/consecutive_recompactions"),
                Some(&json!(1))
            );
            assert_eq!(
                reloaded
                    .metadata
                    .pointer("/compaction/auto_state/last_success_leaf_id")
                    .and_then(serde_json::Value::as_str),
                Some(second_leaf)
            );
            reloaded_store.close().await;

            drop(reloaded);
            assert_eq!(
                recover_post_compaction_dispatches_on_boot(&env.state)
                    .await
                    .expect("third attempt recovers through production boot orchestration"),
                1
            );
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if !env
                        .state
                        .repo
                        .has_unfinished_actions(session_id)
                        .await
                        .expect("third attempt action state loads")
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("third overflow terminalizes without recompacting");
            assert_eq!(
                crate::provider_runtime::injected_provider_start_count(session_id),
                1,
                "the bounded third attempt reaches the model once"
            );
            let assertion_pool =
                sqlx::PgPool::connect(&database_url_with_name(&env.admin_url, &env.name))
                    .await
                    .expect("connect recompaction assertion pool");
            let compaction_count: i64 = sqlx::query_scalar(
                "select count(*)::bigint from actions where session_id=$1 and kind='compaction'",
            )
            .bind(session_id)
            .fetch_one(&assertion_pool)
            .await
            .expect("compaction action count loads");
            assert_eq!(
                compaction_count, 2,
                "the persisted active leaf bound rejects a third compaction"
            );
            assertion_pool.close().await;
        } else {
            let fault_pool =
                sqlx::PgPool::connect(&database_url_with_name(&env.admin_url, &env.name))
                    .await
                    .expect("connect fault injector");
            install_compaction_metadata_fault(&fault_pool).await;
            let injected_failure = env
                .state
                .repo
                .fail_compaction_action(
                    &second.job,
                    &session_config(&env, project_id, json!({ "created_by": "test" })),
                    "rolled back compaction failure".to_string(),
                )
                .await;
            assert!(
                injected_failure.is_err(),
                "metadata fault rolls back the whole failure transition"
            );
            let rolled_back = env.state.repo.session_snapshot(session_id).await.unwrap();
            assert!(rolled_back.pending_actions.iter().any(|action| {
                action.action_row_id == resumed.row_id && action.status == ActionStatus::Blocked
            }));
            assert!(rolled_back.pending_actions.iter().any(|action| {
                action.action_row_id == second.job.action_row_id
                    && action.status == ActionStatus::Running
            }));
            assert_eq!(
                rolled_back
                    .metadata
                    .pointer("/compaction/auto_state/last_failure"),
                Some(&serde_json::Value::Null)
            );
            remove_compaction_metadata_fault(&fault_pool).await;
            fault_pool.close().await;

            env.state
                .repo
                .fail_compaction_action(
                    &second.job,
                    &session_config(&env, project_id, json!({ "created_by": "test" })),
                    "second compaction failed".to_string(),
                )
                .await
                .expect("failure transaction completes");
            let snapshot = env.state.repo.session_snapshot(session_id).await.unwrap();
            assert_eq!(
                snapshot
                    .metadata
                    .pointer("/compaction/auto_state/consecutive_failures"),
                Some(&json!(1))
            );
            assert_eq!(
                snapshot
                    .metadata
                    .pointer("/compaction/auto_state/last_failure_leaf_id"),
                Some(&json!(second.job.source_leaf_id))
            );
            assert!(
                env.state
                    .repo
                    .find_resumable_model_action(session_id, TurnId(1))
                    .await
                    .expect("resumable status loads")
                    .is_some(),
                "failed second compaction must terminally fail the blocked model"
            );
            assert!(
                !env.state
                    .repo
                    .has_unfinished_actions(session_id)
                    .await
                    .unwrap(),
                "no blocked model action may remain stranded"
            );
        }
    }
    env.cleanup().await;
}

#[tokio::test]
async fn unexpected_ordinary_turn_stops_discard_partial_content_and_replay() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "unexpected ordinary stops",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let pool = sqlx::PgPool::connect(&database_url_with_name(&env.admin_url, &env.name))
        .await
        .expect("connect action-result assertion pool");

    for stop_reason in [ModelStopReason::Compaction, ModelStopReason::Refusal] {
        let label = match stop_reason {
            ModelStopReason::Compaction => "compaction",
            ModelStopReason::Refusal => "refusal",
            _ => unreachable!(),
        };
        let session_id = format!("unexpected_ordinary_{label}");
        let config = session_config(
            &env,
            project_id,
            json!({ "created_by": "test", "harness": true }),
        );
        let started = start_prepared_session(
            &env.state,
            PreparedSessionStart {
                session_id: session_id.clone(),
                config,
                priority: InputPriority::FollowUp,
                content: UserMessage::text("ordinary request"),
                client_input_id: None,
                parent_session_id: None,
                subagent_type: None,
                delegation_id: None,
                dispatch_mode: PreparedSessionDispatchMode::Deferred,
            },
        )
        .await
        .expect("live session starts");
        let persisted = started
            .dispatches
            .into_iter()
            .next()
            .expect("live runtime emits a model action");
        assert!(env
            .state
            .repo
            .claim_pending_model_action(&session_id, &persisted.row_id, &persisted.attempt_id,)
            .await
            .expect("model claims"));

        let driver = SessionDriver::acquire(&env.state, &session_id).await;
        let active = driver
            .active_session()
            .await
            .expect("session start retains the live runtime");
        let SessionAction::RequestModel {
            action_id, turn_id, ..
        } = persisted.action.clone()
        else {
            unreachable!()
        };
        let replay = ProviderReplayItem::new(
            ProviderKind::Claude,
            &json!({
                "type": "compaction",
                "content": "partial secret",
                "encrypted_content": "opaque-secret"
            }),
        )
        .expect("valid replay");
        let dispatch = DispatchAction {
            row_id: persisted.row_id,
            attempt_id: persisted.attempt_id,
            post_compaction_dispatch_lease: None,
            action: persisted.action,
            config: session_config(
                &env,
                project_id,
                json!({ "created_by": "test", "harness": true }),
            ),
            mcp_snapshot: agent_mcp::McpSessionSnapshot::empty(),
        };
        apply_model_response(
            &env.state,
            &session_id,
            &driver,
            active,
            &dispatch,
            action_id,
            turn_id,
            ModelResponse {
                assistant: AssistantMessage {
                    items: vec![AssistantItem::Text("partial assistant output".to_string())],
                },
                provider_replay: vec![replay],
                usage: None,
                stop_reason,
                stop_details: (stop_reason == ModelStopReason::Refusal).then(|| ModelStopDetails {
                    category: Some("policy".to_string()),
                    explanation: Some("declined".to_string()),
                }),
            },
            "unexpected-stop-test-toolset",
        )
        .await
        .expect("unexpected stop persists failure");

        let transcript = env
            .state
            .repo
            .transcript_entries_for_scope(
                &session_id,
                TranscriptEntryScope::ActiveBranch,
                TranscriptEntryBodyMode::Full,
            )
            .await
            .expect("transcript loads");
        assert!(
            transcript
                .iter()
                .all(|entry| !matches!(entry.item, TranscriptItem::AssistantMessage(_))),
            "partial {label} assistant output must not persist"
        );
        assert!(
            transcript
                .iter()
                .all(|entry| entry.provider_replay.is_empty()),
            "partial {label} replay must not persist"
        );
        assert!(matches!(
            transcript.last().map(|entry| &entry.item),
            Some(TranscriptItem::TurnFinished {
                outcome: TurnOutcome::Crashed,
                ..
            })
        ));
        let resumable = env
            .state
            .repo
            .find_resumable_model_action(&session_id, turn_id)
            .await
            .expect("terminal model action loads")
            .expect("unexpected stop terminalizes the action");
        assert_eq!(resumable.status, ActionStatus::Error);
        let expected_error = match stop_reason {
            ModelStopReason::Compaction => {
                "unexpected provider compaction stop during an ordinary model turn"
            }
            ModelStopReason::Refusal => "provider refused the request (policy): declined",
            _ => unreachable!(),
        };
        let result: serde_json::Value = sqlx::query_scalar(
            "select result from actions where session_id=$1 and id=$2 and attempt_id=$3",
        )
        .bind(&session_id)
        .bind(&dispatch.row_id)
        .bind(&dispatch.attempt_id)
        .fetch_one(&pool)
        .await
        .expect("load exact terminal action result");
        assert_eq!(
            result
                .get("stop_reason")
                .and_then(serde_json::Value::as_str),
            Some(label)
        );
        assert_eq!(
            result.get("error").and_then(serde_json::Value::as_str),
            Some(expected_error)
        );
        if stop_reason == ModelStopReason::Refusal {
            assert_eq!(
                result
                    .pointer("/stop_details/category")
                    .and_then(serde_json::Value::as_str),
                Some("policy")
            );
            assert_eq!(
                result
                    .pointer("/stop_details/explanation")
                    .and_then(serde_json::Value::as_str),
                Some("declined")
            );
        } else {
            assert_eq!(result.get("stop_details"), Some(&serde_json::Value::Null));
        }
        assert!(
            env.state
                .repo
                .events_after(&session_id, None)
                .await
                .expect("events load")
                .iter()
                .any(|event| {
                    event.event == EventType::ModelError
                        && event
                            .data
                            .get("error")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|error| error.contains(expected_error))
                }),
            "unexpected {label} stop records its terminal provider error"
        );
        assert!(
            !env.state
                .repo
                .has_unfinished_actions(&session_id)
                .await
                .expect("unfinished action check"),
            "unexpected {label} stop must leave no live action"
        );
    }
    pool.close().await;

    env.cleanup().await;
}

#[tokio::test]
async fn follow_up_to_idle_session_is_durably_queued_before_drive() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "durable follow-up test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    env.state
        .repo
        .start_session_outputs(
            "idle_parent",
            &session_config(&env, project_id, json!({ "created_by": "test" })),
            &[
                TranscriptStorageNode {
                    id: "idle_parent_user".to_string(),
                    parent_id: None,
                    timestamp_ms: 1,
                    item: TranscriptItem::UserMessage(UserMessage::text("initial")),
                    provider_replay: Vec::new(),
                },
                TranscriptStorageNode {
                    id: "idle_parent_finish".to_string(),
                    parent_id: Some("idle_parent_user".to_string()),
                    timestamp_ms: 2,
                    item: TranscriptItem::TurnFinished {
                        turn_id: TurnId(1),
                        outcome: TurnOutcome::Graceful,
                    },
                    provider_replay: Vec::new(),
                },
            ],
            Some("idle_parent_finish"),
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("initial"),
            None,
        )
        .await
        .expect("create idle session");

    let response = crate::input_user(
        &env.state,
        json!({
            "session_id": "idle_parent",
            "client_input_id": "client_follow_up_1",
            "content": [{"type": "text", "text": "continue"}],
            "expected_active_leaf_id": "idle_parent_finish",
        }),
    )
    .await
    .expect("follow-up accepted");
    assert_eq!(response["accepted"], true);
    assert_eq!(response["queued"], true);
    assert!(response.get("active_branch_sync").is_none());

    let record = env
        .state
        .repo
        .find_client_input("idle_parent", "client_follow_up_1")
        .await
        .expect("find client input")
        .expect("recorded client input");
    assert!(matches!(
        record.status,
        QueuedInputStatus::Queued | QueuedInputStatus::Consuming | QueuedInputStatus::Consumed
    ));

    env.cleanup().await;
}

#[tokio::test]
async fn exact_child_interrupt_and_combined_control_preserve_parent_and_sibling_scope() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "exact child control test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    create_parent(&env, project_id, "other_parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 3)
        .await
        .expect("create delegation");
    for child in ["child_stop", "child_tool", "child_combined"] {
        create_busy_subagent(
            &env,
            project_id,
            "parent",
            &delegation.id,
            child,
            "reviewer",
            SubagentType::ReadOnly,
        )
        .await;
    }

    let stopped = crate::interrupt_session(&env.state, "child_stop")
        .await
        .expect("user Stop exact child succeeds");
    assert_eq!(stopped["interrupted"], true);
    assert!(!env
        .state
        .repo
        .has_unfinished_actions("child_stop")
        .await
        .expect("stopped child action state"));
    assert!(env
        .state
        .repo
        .has_unfinished_actions("child_tool")
        .await
        .expect("model-controlled sibling remains"));
    assert!(env
        .state
        .repo
        .has_unfinished_actions("child_combined")
        .await
        .expect("combined-control sibling remains"));

    let parent_stop = crate::interrupt_session(&env.state, "parent")
        .await
        .expect("user Stop parent is exact");
    assert_eq!(parent_stop["ignored"], true);
    assert!(env
        .state
        .repo
        .has_unfinished_actions("child_tool")
        .await
        .expect("parent Stop must not cascade to child"));
    assert!(env
        .state
        .repo
        .has_unfinished_actions("child_combined")
        .await
        .expect("parent Stop must not cascade to sibling"));

    let out_of_scope = interrupt_subagent_core(
        &env.state,
        "other_parent",
        json!({ "subagent_id": "child_tool" }),
    )
    .await
    .expect_err("another parent cannot interrupt this child");
    assert_eq!(out_of_scope.code, "subagent_not_found");

    let interrupted =
        interrupt_subagent_core(&env.state, "parent", json!({ "subagent_id": "child_tool" }))
            .await
            .expect("exact child interrupt succeeds");
    assert_eq!(interrupted["subagent_id"], "child_tool");
    assert_eq!(interrupted["interrupted"], true);
    assert!(!env
        .state
        .repo
        .has_unfinished_actions("child_tool")
        .await
        .expect("tool-controlled child action state"));
    assert!(env
        .state
        .repo
        .has_unfinished_actions("child_combined")
        .await
        .expect("combined-control sibling remains"));
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .expect("delegation")
            .expect("delegation exists")
            .status,
        DelegationStatus::Running
    );
    assert!(env
        .state
        .repo
        .session_exists("parent")
        .await
        .expect("parent still exists"));

    let first = steer_subagent_core(
        &env.state,
        "parent",
        json!({
            "subagent_id": "child_combined",
            "message": "stop the old work and check the retry",
            "interrupt": true,
            "client_control_id": "combined-once",
        }),
    )
    .await
    .expect("combined control succeeds");
    assert_eq!(first["accepted"], true);
    assert_eq!(first["interrupted"], true);
    let first_input_id = first["input_id"].as_str().expect("input id").to_string();

    let replay = steer_subagent_core(
        &env.state,
        "parent",
        json!({
            "subagent_id": "child_combined",
            "message": "stop the old work and check the retry",
            "interrupt": true,
            "client_control_id": "combined-once",
        }),
    )
    .await
    .expect("combined retry is accepted");
    assert_eq!(replay["input_id"], first_input_id);
    assert_eq!(replay["replayed"], true);
    assert_eq!(
        replay["interrupted"], true,
        "replay reports the durable historical interrupt outcome"
    );

    let conflict = steer_subagent_core(
        &env.state,
        "parent",
        json!({
            "subagent_id": "child_combined",
            "message": "different text",
            "interrupt": true,
            "client_control_id": "combined-once",
        }),
    )
    .await
    .expect_err("conflicting client_control_id reuse is rejected");
    assert_eq!(conflict.code, "client_control_id_conflict");

    let history = env
        .state
        .repo
        .active_branch("child_combined")
        .await
        .expect("child history");
    let matching_messages = history
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                &entry.item,
                TranscriptItem::UserMessage(message)
                    if message.as_text() == Some("stop the old work and check the retry")
            )
        })
        .count();
    assert_eq!(
        matching_messages, 1,
        "combined retry must not duplicate text"
    );

    env.cleanup().await;
}

#[tokio::test]
async fn interrupt_only_replay_never_interrupts_newer_generation_or_queues_text() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "durable interrupt-only",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "interrupt_parent").await;
    create_parent(&env, project_id, "interrupt_other_parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation(
            "interrupt_parent",
            DelegationKind::ReadonlyFanout,
            None,
            None,
            2,
        )
        .await
        .expect("create delegation");
    create_busy_subagent(
        &env,
        project_id,
        "interrupt_parent",
        &delegation.id,
        "interrupt_child",
        "reviewer",
        SubagentType::ReadOnly,
    )
    .await;
    create_busy_subagent(
        &env,
        project_id,
        "interrupt_parent",
        &delegation.id,
        "interrupt_sibling",
        "reviewer",
        SubagentType::ReadOnly,
    )
    .await;

    let wrong_parent = interrupt_subagent_core(
        &env.state,
        "interrupt_other_parent",
        json!({
            "subagent_id": "interrupt_child",
            "client_control_id": "lost-tool-call",
        }),
    )
    .await
    .expect_err("wrong parent cannot target child");
    assert_eq!(wrong_parent.code, "subagent_not_found");

    let first_tool_result = run_delegation_tool(
        &env.state,
        "interrupt_parent",
        &ToolCall {
            id: ToolCallId::new("call_interrupt"),
            tool_name: "interrupt_subagent".to_string(),
            args_json: json!({
                "subagent_id": "interrupt_child",
                "client_control_id": "provider-must-not-control-this",
            })
            .to_string(),
        },
    )
    .await;
    assert_eq!(
        first_tool_result.status,
        agent_vocab::ToolResultStatus::Success
    );
    let first: serde_json::Value =
        serde_json::from_str(&first_tool_result.output).expect("interrupt result JSON");
    assert_eq!(first["accepted"], true);
    assert_eq!(first["phase"], "ready");
    assert_eq!(first["interrupted"], true);
    assert_eq!(first["queued"], false);
    let input_id = first["input_id"].as_str().expect("control id").to_string();
    assert!(env
        .state
        .repo
        .has_unfinished_actions("interrupt_sibling")
        .await
        .expect("sibling remains live"));

    let sibling_conflict = interrupt_subagent_core(
        &env.state,
        "interrupt_parent",
        json!({
            "subagent_id": "interrupt_sibling",
            "client_control_id": "tool-call:call_interrupt",
        }),
    )
    .await
    .expect_err("delegation-scoped id cannot be reused for a sibling");
    assert_eq!(sibling_conflict.code, "client_control_id_conflict");
    assert!(env
        .state
        .repo
        .has_unfinished_actions("interrupt_sibling")
        .await
        .expect("conflict does not interrupt sibling"));

    env.state
        .repo
        .enqueue_user_input(
            "interrupt_child",
            InputPriority::FollowUp,
            &UserMessage::text("generation B"),
            Some("generation-b"),
            None,
        )
        .await
        .expect("enqueue newer generation");
    let driver = SessionDriver::acquire(&env.state, "interrupt_child").await;
    driver
        .recover_if_needed()
        .await
        .expect("recover interrupted boundary");
    driver
        .drive_until_blocked()
        .await
        .expect("start generation B");
    drop(driver);
    let generation_b = env
        .state
        .repo
        .pending_actions_for_dispatch("interrupt_child")
        .await
        .expect("load generation B");
    assert_eq!(generation_b.len(), 1);
    env.state
        .repo
        .mark_action_running_and_event(
            "interrupt_child",
            &generation_b[0].row_id,
            &generation_b[0].attempt_id,
            EventType::ModelRequested,
        )
        .await
        .expect("mark generation B running");

    let replay_tool_result = run_delegation_tool(
        &env.state,
        "interrupt_parent",
        &ToolCall {
            id: ToolCallId::new("call_interrupt"),
            tool_name: "interrupt_subagent".to_string(),
            args_json: json!({
                "subagent_id": "interrupt_child",
                "client_control_id": "another-provider-value",
            })
            .to_string(),
        },
    )
    .await;
    assert_eq!(
        replay_tool_result.status,
        agent_vocab::ToolResultStatus::Success
    );
    let replay: serde_json::Value =
        serde_json::from_str(&replay_tool_result.output).expect("replay result JSON");
    assert_eq!(replay["input_id"], input_id);
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["phase"], "ready");
    assert!(env
        .state
        .repo
        .action_can_complete(
            "interrupt_child",
            &generation_b[0].row_id,
            &generation_b[0].attempt_id,
            None,
        )
        .await
        .expect("generation B remains current"));

    env.state
        .repo
        .set_delegation_status(&delegation.id, DelegationStatus::Cancelled)
        .await
        .expect("make delegation terminal");
    let terminal_replay_tool_result = run_delegation_tool(
        &env.state,
        "interrupt_parent",
        &ToolCall {
            id: ToolCallId::new("call_interrupt"),
            tool_name: "interrupt_subagent".to_string(),
            args_json: json!({
                "subagent_id": "interrupt_child",
            })
            .to_string(),
        },
    )
    .await;
    assert_eq!(
        terminal_replay_tool_result.status,
        agent_vocab::ToolResultStatus::Success
    );
    let terminal_replay: serde_json::Value =
        serde_json::from_str(&terminal_replay_tool_result.output)
            .expect("terminal replay result JSON");
    assert_eq!(terminal_replay["input_id"], input_id);
    assert_eq!(terminal_replay["replayed"], true);
    assert!(env
        .state
        .repo
        .action_can_complete(
            "interrupt_child",
            &generation_b[0].row_id,
            &generation_b[0].attempt_id,
            None,
        )
        .await
        .expect("terminal replay still does not interrupt generation B"));

    let history = env
        .state
        .repo
        .active_branch("interrupt_child")
        .await
        .expect("load interrupt history");
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| matches!(
                entry.item,
                TranscriptItem::TurnFinished {
                    outcome: TurnOutcome::Interrupted,
                    ..
                }
            ))
            .count(),
        1,
        "replay must not append a second interrupted boundary"
    );
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| matches!(
                &entry.item,
                TranscriptItem::UserMessage(message)
                    if message.as_text() == Some("generation B")
            ))
            .count(),
        1
    );
    assert!(
        history.entries.iter().all(|entry| {
            !matches!(
                &entry.item,
                TranscriptItem::UserMessage(message)
                    if message.as_text().is_some_and(|text| text.contains("call_interrupt"))
            )
        }),
        "interrupt-only ledger identity must never be injected as text"
    );

    env.cleanup().await;
}

#[tokio::test]
async fn parent_control_task_aborted_after_commit_is_reconciled_by_detached_child_driver() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "detached combined control",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    create_busy_full_subagent(&env, project_id, "parent", &delegation.id, "detached_child").await;

    let client_input_id = format!("subagent-control:{}:aborted-owner", delegation.id);
    let (committed_tx, committed_rx) = tokio::sync::oneshot::channel();
    let owner_state = env.state.clone();
    let owner_delegation_id = delegation.id.clone();
    let owner_client_input_id = client_input_id.clone();
    let owner = tokio::spawn(async move {
        let queued = owner_state
            .repo
            .enqueue_scoped_subagent_steer(
                "parent",
                &owner_delegation_id,
                "detached_child",
                &UserMessage::text("continue after the durable interrupt"),
                &owner_client_input_id,
                true,
            )
            .await
            .expect("enqueue combined control")
            .expect("running delegation accepts control");
        committed_tx
            .send(queued.input_id)
            .expect("signal committed input");
        std::future::pending::<()>().await;
    });
    let input_id = committed_rx.await.expect("control commit observed");
    owner.abort();
    let _ = owner.await;

    let pending = env
        .state
        .repo
        .get_subagent_control_by_input_id("detached_child", &input_id)
        .await
        .expect("load pending control")
        .expect("pending control exists");
    assert_eq!(pending.phase, SubagentControlPhase::PendingInterrupt);
    assert!(
        env.state
            .repo
            .take_next_queued_input("detached_child")
            .await
            .expect("generic queue read")
            .is_none(),
        "generic driving cannot consume the parked combined message"
    );

    crate::spawn_drive_until_blocked(
        &env.state,
        "detached_child".to_string(),
        "test.detached_combined_control",
    );
    let recovered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let control = env
                .state
                .repo
                .get_subagent_control_by_input_id("detached_child", &input_id)
                .await
                .expect("load recovering control")
                .expect("control remains durable");
            if control.phase == SubagentControlPhase::Ready
                && control.interrupted
                && control.status == QueuedInputStatus::Consumed
            {
                break control;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("detached exact-child driver reconciles and consumes control");
    assert_eq!(recovered.interrupt_outcome.as_deref(), Some("interrupted"));

    let history = env
        .state
        .repo
        .active_branch("detached_child")
        .await
        .expect("child history after reconciliation");
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    &entry.item,
                    TranscriptItem::TurnFinished {
                        outcome: TurnOutcome::Interrupted,
                        ..
                    }
                )
            })
            .count(),
        1,
        "the captured old turn is interrupted exactly once"
    );
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    &entry.item,
                    TranscriptItem::UserMessage(message)
                        if message.as_text() == Some("continue after the durable interrupt")
                )
            })
            .count(),
        1,
        "the steer is driven exactly once after the interrupt phase"
    );

    env.cleanup().await;
}

#[tokio::test]
async fn restart_from_interrupt_applied_phase_does_not_repeat_interrupt() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "combined control crash recovery",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    create_busy_full_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "crash_phase_child",
    )
    .await;

    let queued = env
        .state
        .repo
        .enqueue_scoped_subagent_steer(
            "parent",
            &delegation.id,
            "crash_phase_child",
            &UserMessage::text("drive after crash recovery"),
            &format!("subagent-control:{}:crash-phase", delegation.id),
            true,
        )
        .await
        .expect("enqueue combined control")
        .expect("running delegation accepts control");
    let interrupted_leaf = "crash_phase_child_interrupted";
    let interrupted_entry = TranscriptStorageNode {
        id: interrupted_leaf.to_string(),
        parent_id: Some("crash_phase_child_a".to_string()),
        timestamp_ms: 4,
        item: TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Interrupted,
        },
        provider_replay: Vec::new(),
    };
    env.state
        .repo
        .persist_outputs(
            "crash_phase_child",
            OutputBatch::new(&[interrupted_entry], Some(interrupted_leaf), &[], &[])
                .with_control_interrupt(&queued.input_id),
        )
        .await
        .expect("simulate crash after atomic interrupt commit");
    let applied = env
        .state
        .repo
        .get_subagent_control_by_input_id("crash_phase_child", &queued.input_id)
        .await
        .expect("load applied control")
        .expect("control exists");
    assert_eq!(applied.phase, SubagentControlPhase::InterruptApplied);
    assert!(applied.interrupted);
    assert!(
        env.state
            .repo
            .take_next_queued_input("crash_phase_child")
            .await
            .expect("generic queue read")
            .is_none(),
        "the crash phase remains mailbox-blocking"
    );

    let mut future_default = env
        .state
        .repo
        .load_session_config("crash_phase_child")
        .await
        .expect("load future default");
    future_default.provider.reasoning_effort = ReasoningEffort::High;
    env.state
        .repo
        .configure_session("crash_phase_child", &future_default)
        .await
        .expect("change default after the control captured its route");

    // Simulate process loss: no volatile session or task ownership survives.
    env.state.active.lock().await.remove("crash_phase_child");
    crate::spawn_drive_until_blocked(
        &env.state,
        "crash_phase_child".to_string(),
        "test.interrupt_applied_recovery",
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let control = env
                .state
                .repo
                .get_subagent_control_by_input_id("crash_phase_child", &queued.input_id)
                .await
                .expect("load recovered control")
                .expect("control remains durable");
            if control.phase == SubagentControlPhase::Ready
                && control.status == QueuedInputStatus::Consumed
            {
                assert!(control.interrupted);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fresh child driver settles applied phase");
    assert_eq!(
        env.state
            .active
            .lock()
            .await
            .get("crash_phase_child")
            .expect("recovered active runtime")
            .lock()
            .await
            .config
            .provider
            .reasoning_effort,
        ReasoningEffort::Medium,
        "recovered ready steer restores its captured route over the new default"
    );

    let history = env
        .state
        .repo
        .active_branch("crash_phase_child")
        .await
        .expect("recovered history");
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.item,
                    TranscriptItem::TurnFinished {
                        outcome: TurnOutcome::Interrupted,
                        ..
                    }
                )
            })
            .count(),
        1,
        "an interrupt_applied retry must not append another boundary"
    );
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    &entry.item,
                    TranscriptItem::UserMessage(message)
                        if message.as_text() == Some("drive after crash recovery")
                )
            })
            .count(),
        1
    );
    tokio::task::yield_now().await;
    drop(SessionDriver::acquire(&env.state, "ready_child").await);

    env.cleanup().await;
}

#[tokio::test]
async fn combined_control_interrupts_complete_parallel_tool_generation_once() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "parallel control generation",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parallel_parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parallel_parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");

    let first_call = ToolCall {
        id: ToolCallId::from_u64(101),
        tool_name: "Bash".to_string(),
        args_json: r#"{"command":"sleep 10"}"#.to_string(),
    };
    let second_call = ToolCall {
        id: ToolCallId::from_u64(102),
        tool_name: "web_fetch".to_string(),
        args_json: r#"{"url":"https://example.com"}"#.to_string(),
    };
    let entries = vec![
        TranscriptStorageNode {
            id: "parallel_start".to_string(),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: "parallel_user".to_string(),
            parent_id: Some("parallel_start".to_string()),
            timestamp_ms: 2,
            item: TranscriptItem::UserMessage(UserMessage::text("run both")),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: "parallel_assistant".to_string(),
            parent_id: Some("parallel_user".to_string()),
            timestamp_ms: 3,
            item: TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![
                    AssistantItem::ToolCall(first_call.clone()),
                    AssistantItem::ToolCall(second_call.clone()),
                ],
            }),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: "parallel_first_started".to_string(),
            parent_id: Some("parallel_assistant".to_string()),
            timestamp_ms: 4,
            item: TranscriptItem::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: first_call.clone(),
            },
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: "parallel_second_started".to_string(),
            parent_id: Some("parallel_first_started".to_string()),
            timestamp_ms: 5,
            item: TranscriptItem::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: second_call.clone(),
            },
            provider_replay: Vec::new(),
        },
    ];
    let actions = vec![
        SessionAction::RequestTool {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            tool_call: first_call.clone(),
        },
        SessionAction::RequestTool {
            action_id: ActionId(3),
            turn_id: TurnId(1),
            tool_call: second_call.clone(),
        },
    ];
    env.state
        .repo
        .start_session_outputs_with_parent(
            "parallel_child",
            &session_config(&env, project_id, {
                let mut metadata = subagent_test_metadata("implementer", SubagentType::Full);
                metadata["harness"] = json!(true);
                metadata
            }),
            &entries,
            Some("parallel_second_started"),
            &[],
            &actions,
            InputPriority::FollowUp,
            &UserMessage::text("run both"),
            None,
            Some("parallel_parent"),
            Some(SubagentType::Full),
            Some(&delegation.id),
        )
        .await
        .expect("create parallel child");
    let attempts = env
        .state
        .repo
        .pending_actions_for_dispatch("parallel_child")
        .await
        .expect("load parallel attempts");
    assert_eq!(attempts.len(), 2);
    for attempt in &attempts {
        env.state
            .repo
            .mark_action_running_and_event(
                "parallel_child",
                &attempt.row_id,
                &attempt.attempt_id,
                EventType::ToolStarted,
            )
            .await
            .expect("mark captured attempt running");
        assert!(env
            .state
            .repo
            .action_can_complete("parallel_child", &attempt.row_id, &attempt.attempt_id, None,)
            .await
            .expect("captured attempt initially live"));
    }

    let client_input_id = format!("subagent-control:{}:parallel-tools", delegation.id);
    let queued = env
        .state
        .repo
        .enqueue_scoped_subagent_steer(
            "parallel_parent",
            &delegation.id,
            "parallel_child",
            &UserMessage::text("continue exactly once"),
            &client_input_id,
            true,
        )
        .await
        .expect("enqueue combined control")
        .expect("running delegation accepts control");
    let captured = env
        .state
        .repo
        .get_subagent_control_by_input_id("parallel_child", &queued.input_id)
        .await
        .expect("load captured generation")
        .expect("control exists");
    let mut expected_attempt_ids = attempts
        .iter()
        .map(|attempt| attempt.attempt_id.clone())
        .collect::<Vec<_>>();
    expected_attempt_ids.sort();
    let mut captured_attempt_ids = captured.target_action_attempt_ids.clone();
    captured_attempt_ids.sort();
    assert_eq!(captured_attempt_ids, expected_attempt_ids);

    // One captured sibling wins its completion race before reconciliation.
    // The remaining captured sibling must still be interrupted rather than
    // misclassified as a newer generation.
    let first_result_leaf = "parallel_first_result";
    let first_result = TranscriptStorageNode {
        id: first_result_leaf.to_string(),
        parent_id: Some("parallel_second_started".to_string()),
        timestamp_ms: 6,
        item: TranscriptItem::ToolResult(ToolResultMessage::success(
            first_call.id.clone(),
            first_call.tool_name.clone(),
            "completed before reconciliation",
        )),
        provider_replay: Vec::new(),
    };
    env.state
        .repo
        .persist_outputs(
            "parallel_child",
            OutputBatch::new(&[first_result], Some(first_result_leaf), &[], &[])
                .with_action_update(Some(ActionUpdate {
                    row_id: attempts[0].row_id.clone(),
                    attempt_id: attempts[0].attempt_id.clone(),
                    status: ActionStatus::Completed,
                    result: json!({ "completed": true }),
                    post_compaction_dispatch_lease: None,
                })),
        )
        .await
        .expect("one parallel sibling completes before reconciliation");

    let driver = SessionDriver::acquire(&env.state, "parallel_child").await;
    driver
        .reconcile_pending_subagent_controls()
        .await
        .expect("interrupt complete captured generation");
    env.state
        .active
        .lock()
        .await
        .get("parallel_child")
        .expect("live parallel runtime")
        .lock()
        .await
        .config
        .provider
        .reasoning_effort = ReasoningEffort::High;
    driver
        .drive_until_blocked()
        .await
        .expect("consume ready steer once");
    assert_eq!(
        env.state
            .active
            .lock()
            .await
            .get("parallel_child")
            .expect("continued live runtime")
            .lock()
            .await
            .config
            .provider
            .reasoning_effort,
        ReasoningEffort::Medium,
        "live ready steer restores the captured generation route"
    );
    drop(driver);

    for attempt in &attempts {
        assert!(
            !env.state
                .repo
                .action_can_complete("parallel_child", &attempt.row_id, &attempt.attempt_id, None,)
                .await
                .expect("late completion fence"),
            "every interrupted parallel attempt rejects late completion"
        );
    }
    assert!(
        env.state
            .repo
            .has_unfinished_actions("parallel_child")
            .await
            .expect("new generation remains"),
        "the steer-created model generation must remain live and un-interrupted"
    );
    let settled = env
        .state
        .repo
        .get_subagent_control_by_input_id("parallel_child", &queued.input_id)
        .await
        .expect("load settled control")
        .expect("control remains durable");
    assert_eq!(settled.phase, SubagentControlPhase::Ready);
    assert_eq!(settled.status, QueuedInputStatus::Consumed);

    let history = env
        .state
        .repo
        .active_branch("parallel_child")
        .await
        .expect("load valid interrupted history");
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| matches!(entry.item, TranscriptItem::ToolResult(_)))
            .count(),
        2,
        "both open parallel tool calls receive synthesized results"
    );
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| matches!(
                entry.item,
                TranscriptItem::TurnFinished {
                    outcome: TurnOutcome::Interrupted,
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| matches!(
                &entry.item,
                TranscriptItem::UserMessage(message)
                    if message.as_text() == Some("continue exactly once")
            ))
            .count(),
        1
    );

    let newer = env
        .state
        .repo
        .pending_actions_for_dispatch("parallel_child")
        .await
        .expect("load post-steer generation");
    assert_eq!(newer.len(), 1);
    crate::harness_model_complete(
        &env.state,
        json!({
            "session_id": "parallel_child",
            "action_row_id": newer[0].row_id,
            "assistant": { "items": [{ "type": "text", "text": "done" }] },
        }),
    )
    .await
    .expect("genuinely newer generation completes normally");
    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("barrier runs after settled steer");
    assert_eq!(
        env.state
            .repo
            .get_delegation(&delegation.id)
            .await
            .expect("load completed delegation")
            .expect("delegation exists")
            .status,
        DelegationStatus::Done
    );

    env.cleanup().await;
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
            &session_config(env, project_id, subagent_test_metadata(role, subagent_type)),
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
            id: format!("{session_id}_start"),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: format!("{session_id}_u"),
            parent_id: Some(format!("{session_id}_start")),
            timestamp_ms: 2,
            item: TranscriptItem::UserMessage(UserMessage::text("keep working")),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: mid_turn.clone(),
            parent_id: Some(format!("{session_id}_u")),
            timestamp_ms: 3,
            item: TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("working...".to_string())],
            }),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: boundary.clone(),
            parent_id: Some(mid_turn.clone()),
            timestamp_ms: 4,
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
                subagent_test_metadata(role, SubagentType::ReadOnly),
            ),
            &entries,
            Some(&mid_turn),
            &[],
            &[SessionAction::RequestModel {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                model_context: ModelContext::new(),
                context_leaf_id: Some(mid_turn.clone()),
            }],
            InputPriority::FollowUp,
            &UserMessage::text("keep working"),
            None,
            Some(parent_id),
            Some(SubagentType::ReadOnly),
            Some(delegation_id),
        )
        .await
        .expect("create running subagent");
    let driver = SessionDriver::acquire(&env.state, session_id).await;
    driver
        .ensure_active_loaded_preserving_open_turn()
        .await
        .expect("load live running subagent");
    // The active leaf is the assistant message and the model action is still
    // unfinished, so this is a valid busy, non-terminal subagent fixture.
    assert!(!env
        .state
        .repo
        .active_leaf_is_turn_boundary(session_id)
        .await
        .expect("boundary check"));
    boundary
}

/// A spawned delegation child whose session row exists but whose transcript has
/// not started yet (`active_leaf_id == None`). This is terminal/non-failed by
/// the store's progress convention.
async fn create_empty_subagent(
    env: &TestEnv,
    project_id: Uuid,
    parent_id: &str,
    delegation_id: &str,
    session_id: &str,
    role: &str,
    subagent_type: SubagentType,
) {
    env.state
        .repo
        .start_session_outputs_with_parent(
            session_id,
            &session_config(env, project_id, subagent_test_metadata(role, subagent_type)),
            &[],
            None,
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("spawned but not started"),
            None,
            Some(parent_id),
            Some(subagent_type),
            Some(delegation_id),
        )
        .await
        .expect("create empty delegation subagent");
    assert_eq!(
        env.state.repo.activity(session_id).await.expect("activity"),
        agent_store::SessionActivity::Idle
    );
    let history = env
        .state
        .repo
        .active_branch(session_id)
        .await
        .expect("empty active branch");
    assert_eq!(history.active_leaf_id, None);
    assert!(history.entries.is_empty());
}

fn tool_names(result: &serde_json::Value) -> Vec<String> {
    result["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| {
            tool["canonical_name"]
                .as_str()
                .expect("canonical name")
                .to_string()
        })
        .collect()
}

fn assert_delegation_tools_visible(names: &[String]) {
    assert!(names.contains(&"delegate_writing_task".to_string()));
    assert!(names.contains(&"delegate_readonly_tasks".to_string()));
    assert!(names.contains(&"inspect_delegation".to_string()));
    assert!(names.contains(&"cancel_delegation".to_string()));
    assert!(names.contains(&"steer_subagent".to_string()));
    assert!(names.contains(&"interrupt_subagent".to_string()));
}

fn assert_delegation_tools_hidden(names: &[String]) {
    assert!(names.contains(&"LoadSkill".to_string()));
    assert!(!names.contains(&"delegate_writing_task".to_string()));
    assert!(!names.contains(&"delegate_readonly_tasks".to_string()));
    assert!(!names.contains(&"inspect_delegation".to_string()));
    assert!(!names.contains(&"cancel_delegation".to_string()));
    assert!(!names.contains(&"steer_subagent".to_string()));
    assert!(!names.contains(&"interrupt_subagent".to_string()));
}

#[tokio::test]
async fn tools_list_filters_delegation_tools_for_subagent_session() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "tools list profile test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("impl"), 1)
        .await
        .expect("create delegation");
    create_empty_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "impl_child",
        "implementer",
        SubagentType::Full,
    )
    .await;

    let global_names = tool_names(
        &crate::tools_list(&env.state, json!({ "provider": "openai" }))
            .await
            .expect("global tools list"),
    );
    assert_delegation_tools_visible(&global_names);

    let parent_names = tool_names(
        &crate::tools_list(
            &env.state,
            json!({ "provider": "openai", "session_id": "parent" }),
        )
        .await
        .expect("parent tools list"),
    );
    assert_delegation_tools_visible(&parent_names);

    let openai_subagent_names = tool_names(
        &crate::tools_list(
            &env.state,
            json!({ "provider": "openai", "session_id": "impl_child" }),
        )
        .await
        .expect("openai subagent tools list"),
    );
    assert_delegation_tools_hidden(&openai_subagent_names);

    let claude_subagent_names = tool_names(
        &crate::tools_list(
            &env.state,
            json!({ "provider": "claude", "session_id": "impl_child" }),
        )
        .await
        .expect("claude subagent tools list"),
    );
    assert_delegation_tools_hidden(&claude_subagent_names);

    env.cleanup().await;
}

#[tokio::test]
async fn structural_subagent_stays_subagent_profile_after_session_configure() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "subagent configure profile test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("impl"), 1)
        .await
        .expect("create delegation");
    create_empty_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "impl_child",
        "implementer",
        SubagentType::Full,
    )
    .await;

    let response = crate::session_configure(
        &env.state,
        json!({
            "session_id": "impl_child",
            "metadata": {
                "prompt_profile": "parent",
                "subagent": false,
                "hidden": false,
                "role_name": "spoofed",
                "subagent_type": "read_only",
                "title": "configured child"
            }
        }),
    )
    .await
    .expect("configure structural subagent");

    assert_eq!(response["metadata"]["prompt_profile"], "subagent");
    assert_eq!(response["metadata"]["subagent"], true);
    assert_eq!(response["metadata"]["hidden"], true);
    assert_eq!(response["metadata"]["role_name"], "implementer");
    assert_eq!(response["metadata"]["subagent_type"], "full");
    assert_eq!(response["metadata"]["title"], "configured child");

    let mut config = env
        .state
        .repo
        .load_session_config("impl_child")
        .await
        .expect("subagent config");
    config.metadata = json!({ "prompt_profile": "parent" });
    config.system_prompt = "Subagent prompt".to_string();
    let request = build_model_request(
        &env.state,
        &config,
        "impl_child",
        None,
        ModelContext::new(),
        &mcp_snapshot_for_session(&config).expect("empty MCP snapshot"),
    )
    .await
    .expect("build structurally-subagent model request");
    let request_tool_names = request
        .tools
        .iter()
        .map(|tool| tool.canonical_name.clone())
        .collect::<Vec<_>>();
    assert_delegation_tools_hidden(&request_tool_names);

    let tools_list_names = tool_names(
        &crate::tools_list(
            &env.state,
            json!({ "provider": "openai", "session_id": "impl_child" }),
        )
        .await
        .expect("tools list after configure"),
    );
    assert_delegation_tools_hidden(&tools_list_names);

    env.cleanup().await;
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
    create_busy_subagent(
        env,
        project_id,
        parent_id,
        delegation_id,
        session_id,
        "implementer",
        SubagentType::Full,
    )
    .await;
}

/// A delegation subagent with an active, unfinished model action. This keeps the
/// session genuinely busy so a steer-priority input should be queued rather
/// than immediately consumed into a new turn.
async fn create_busy_subagent(
    env: &TestEnv,
    project_id: Uuid,
    parent_id: &str,
    delegation_id: &str,
    session_id: &str,
    role: &str,
    subagent_type: SubagentType,
) {
    let active_leaf = format!("{session_id}_a");
    let entries = vec![
        TranscriptStorageNode {
            id: format!("{session_id}_start"),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: format!("{session_id}_u"),
            parent_id: Some(format!("{session_id}_start")),
            timestamp_ms: 2,
            item: TranscriptItem::UserMessage(UserMessage::text("keep working")),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: active_leaf.clone(),
            parent_id: Some(format!("{session_id}_u")),
            timestamp_ms: 3,
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
            &session_config(env, project_id, {
                let mut metadata = subagent_test_metadata(role, subagent_type);
                metadata["harness"] = json!(true);
                metadata
            }),
            &entries,
            Some(&active_leaf),
            &[],
            &actions,
            InputPriority::FollowUp,
            &UserMessage::text("keep working"),
            None,
            Some(parent_id),
            Some(subagent_type),
            Some(delegation_id),
        )
        .await
        .expect("create busy subagent");
    let driver = SessionDriver::acquire(&env.state, session_id).await;
    driver
        .ensure_active_loaded_preserving_open_turn()
        .await
        .expect("load live busy subagent runtime");
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
        .cancel_unfinished_session_work(session_id, "test settled")
        .await
        .expect("settle action");
    env.state.active.lock().await.remove(session_id);
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

/// Delegation observations that reached the parent. An idle parent accepts the
/// daemon-authored observation as its next model-visible turn, so the typed
/// observation lands in the parent's transcript. This includes partial
/// still-running observations and the final terminal completion observation.
async fn parent_delegation_observations(
    env: &TestEnv,
    parent_id: &str,
    delegation_id: &str,
) -> Vec<DaemonToolObservation> {
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
            TranscriptItem::DaemonToolObservation(observation)
                if observation.tool_name == "inspect_delegation"
                    && observation
                        .args_json
                        .contains(&format!("\"delegation_id\":\"{delegation_id}\"")) =>
            {
                Some(observation.clone())
            }
            _ => None,
        })
        .collect()
}

async fn parent_completion_observations(
    env: &TestEnv,
    parent_id: &str,
    delegation_id: &str,
) -> Vec<DaemonToolObservation> {
    parent_delegation_observations(env, parent_id, delegation_id)
        .await
        .into_iter()
        .filter(|observation| {
            !matches!(
                observation
                    .result_json
                    .get("status")
                    .and_then(serde_json::Value::as_str),
                Some("running")
            )
        })
        .collect()
}

async fn wakeup_observations_to_parent(
    env: &TestEnv,
    parent_id: &str,
    delegation_id: &str,
) -> usize {
    parent_completion_observations(env, parent_id, delegation_id)
        .await
        .len()
}

async fn parent_completion_snapshot(
    env: &TestEnv,
    parent_id: &str,
    delegation_id: &str,
) -> serde_json::Value {
    let observations = parent_completion_observations(env, parent_id, delegation_id).await;
    assert_eq!(
        observations.len(),
        1,
        "expected exactly one completion wakeup"
    );
    observations[0].result_json.clone()
}

async fn parent_partial_client_input_ids(
    env: &TestEnv,
    parent_id: &str,
    delegation: &Delegation,
) -> Vec<String> {
    env.state
        .repo
        .queue_state(parent_id)
        .await
        .expect("parent queue")
        .queued_inputs
        .into_iter()
        .filter_map(|input| {
            let client_input_id = input.client_input_id?;
            client_input_id
                .starts_with(&format!(
                    "delegation-steer:{}:{}:",
                    delegation.id, delegation.attempt_id
                ))
                .then_some(client_input_id)
        })
        .collect()
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

fn assert_list_subagent_has_only_compact_fields(subagent: &serde_json::Value) {
    let object = subagent.as_object().expect("subagent object");
    for key in object.keys() {
        assert!(
            matches!(
                key.as_str(),
                "id" | "status"
                    | "activity"
                    | "role"
                    | "title"
                    | "type"
                    | "subagent_type"
                    | "task_prompt_file"
                    | "steerable"
                    | "outcome"
                    | "final_message_file"
                    | "transcript_file"
            ),
            "unexpected list subagent field: {key}"
        );
    }
}

fn handoff_root(env: &TestEnv, delegation_id: &str) -> PathBuf {
    env.cwd.path().join(".pi-handoff").join(delegation_id)
}

fn user_texts(entries: &[ModelTranscriptEntry]) -> Vec<&str> {
    entries
        .iter()
        .filter_map(|entry| match entry.item() {
            TranscriptItem::UserMessage(message) => message.as_text(),
            _ => None,
        })
        .collect()
}

fn compaction_input_texts(entries: &[ModelTranscriptEntry]) -> Vec<&str> {
    entries
        .iter()
        .filter_map(|entry| match entry.item() {
            TranscriptItem::UserMessage(message) => message.as_text(),
            TranscriptItem::CompactionSummary(summary) => Some(summary.summary.as_str()),
            _ => None,
        })
        .collect()
}

fn test_compaction_output(summary: &str) -> CompactionOutput {
    CompactionOutput {
        summary: summary.to_string(),
        summary_kind: CompactionSummaryKind::ProviderText,
        provider_replay: Vec::new(),
        provider: ProviderKind::OpenAi,
        usage: None,
    }
}

#[tokio::test]
async fn parent_model_context_does_not_inject_current_delegations() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "delegation context test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;

    let done = env
        .state
        .repo
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            None,
            Some("review"),
            1,
        )
        .await
        .expect("create done delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &done.id,
        "review_done",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "Looks good.\n\noutcome: approved",
    )
    .await;
    assert!(env
        .state
        .repo
        .finish_delegation(&done.id, &done.attempt_id, DelegationStatus::Done)
        .await
        .expect("finish done"));
    std::fs::create_dir_all(handoff_root(&env, &done.id).join("review_done")).expect("handoff dir");
    std::fs::write(
        handoff_root(&env, &done.id)
            .join("review_done")
            .join("final_message.md"),
        "Looks good.\n\noutcome: approved",
    )
    .expect("write final message artifact");
    let running = env
        .state
        .repo
        .create_delegation(
            "parent",
            DelegationKind::Full,
            Some("workflow-implement-review"),
            Some("implement"),
            1,
        )
        .await
        .expect("create running delegation");
    create_busy_full_subagent(&env, project_id, "parent", &running.id, "impl_busy").await;

    let mut config = env
        .state
        .repo
        .load_session_config("parent")
        .await
        .expect("parent config");
    config.system_prompt = "PI stable prompt".to_string();
    let request = build_model_request(
        &env.state,
        &config,
        "parent",
        None,
        ModelContext::new(),
        &mcp_snapshot_for_session(&config).expect("empty MCP snapshot"),
    )
    .await
    .expect("build model request");

    assert_eq!(
        request.prompt.stable_prefix.as_deref(),
        Some("PI stable prompt")
    );
    assert!(
        request.prompt.dynamic_context.is_none(),
        "normal parent turns should not receive current-delegations dynamic context"
    );
    assert_eq!(
        request.prompt.render_joined().as_deref(),
        Some("PI stable prompt")
    );
    assert!(
        !request
            .prompt
            .render_joined()
            .unwrap_or_default()
            .contains("## Current delegations"),
        "normal parent prompt must be stable PI/system prompt only"
    );
    let parent_tool_names = request
        .tools
        .iter()
        .map(|tool| tool.canonical_name.as_str())
        .collect::<Vec<_>>();
    assert!(parent_tool_names.contains(&"delegate_writing_task"));
    assert!(parent_tool_names.contains(&"delegate_readonly_tasks"));
    assert!(parent_tool_names.contains(&"inspect_delegation"));
    assert!(parent_tool_names.contains(&"cancel_delegation"));
    assert!(parent_tool_names.contains(&"steer_subagent"));

    env.cleanup().await;
}

#[tokio::test]
async fn configure_and_rename_refresh_non_provider_state_without_retargeting_active_work() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    let session_id = "active-route-refresh";
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "active route refresh",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    let original = session_config(&env, project_id, json!({ "title": "Before" }));
    env.state
        .repo
        .create_session(session_id, &original)
        .await
        .expect("create session");
    let active = Arc::new(Mutex::new(RuntimeSession {
        session: AgentSession::new(),
        config: original.clone(),
        persisted_active_leaf_id: None,
    }));
    env.state
        .active
        .lock()
        .await
        .insert(session_id.to_string(), active.clone());

    let response = crate::session_configure(
        &env.state,
        json!({
            "session_id": session_id,
            "provider": {
                "kind": "openai",
                "model": original.provider.model,
                "reasoning_effort": "high"
            }
        }),
    )
    .await
    .expect("configure future default");
    assert_eq!(response["provider"]["reasoning_effort"], "high");
    assert_eq!(
        active.lock().await.config.provider.reasoning_effort,
        ReasoningEffort::Medium,
        "active work retains its captured route"
    );

    crate::session_rename(
        &env.state,
        json!({ "session_id": session_id, "title": "After" }),
    )
    .await
    .expect("rename active session");
    let active = active.lock().await;
    assert_eq!(active.config.metadata["title"], "After");
    assert_eq!(
        active.config.provider.reasoning_effort,
        ReasoningEffort::Medium,
        "rename refresh also preserves the active route"
    );

    drop(active);
    env.cleanup().await;
}

#[tokio::test]
async fn subagent_model_context_does_not_get_parent_delegation_summary() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "subagent context test",
            &[],
            json!({}),
        )
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

    let mut config = env
        .state
        .repo
        .load_session_config("impl_busy")
        .await
        .expect("subagent config");
    config.system_prompt = "Subagent PI prompt".to_string();
    let request = build_model_request(
        &env.state,
        &config,
        "impl_busy",
        None,
        ModelContext::new(),
        &mcp_snapshot_for_session(&config).expect("empty MCP snapshot"),
    )
    .await
    .expect("build subagent model request");

    assert_eq!(
        request.prompt.stable_prefix.as_deref(),
        Some("Subagent PI prompt")
    );
    assert!(
        request.prompt.dynamic_context.is_none(),
        "subagents should not receive parent current-delegations context"
    );
    assert_eq!(
        request.prompt.render_joined().as_deref(),
        Some("Subagent PI prompt")
    );
    let subagent_tool_names = request
        .tools
        .iter()
        .map(|tool| tool.canonical_name.as_str())
        .collect::<Vec<_>>();
    assert!(subagent_tool_names.contains(&"LoadSkill"));
    assert!(subagent_tool_names.contains(&"Bash"));
    assert!(!subagent_tool_names.contains(&"delegate_writing_task"));
    assert!(!subagent_tool_names.contains(&"delegate_readonly_tasks"));
    assert!(!subagent_tool_names.contains(&"inspect_delegation"));
    assert!(!subagent_tool_names.contains(&"cancel_delegation"));
    assert!(!subagent_tool_names.contains(&"steer_subagent"));

    env.cleanup().await;
}

#[tokio::test]
async fn parent_compaction_output_appends_complete_delegation_ledger_after_provider_summary() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    std::fs::write(
        env.cwd.path().join("PI.compaction.md"),
        "Produce a compact continuation summary.",
    )
    .expect("write compaction prompt");
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "parent compaction ledger test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;

    let done = env
        .state
        .repo
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            None,
            Some("review"),
            1,
        )
        .await
        .expect("create done delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &done.id,
        "review_done",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "Looks good.\n\noutcome: approved",
    )
    .await;
    assert!(env
        .state
        .repo
        .finish_delegation(&done.id, &done.attempt_id, DelegationStatus::Done)
        .await
        .expect("finish done"));
    std::fs::create_dir_all(handoff_root(&env, &done.id).join("review_done")).expect("handoff dir");
    std::fs::write(
        handoff_root(&env, &done.id)
            .join("review_done")
            .join("final_message.md"),
        "Looks good.\n\noutcome: approved",
    )
    .expect("write final message artifact");

    let done_with_failures = env
        .state
        .repo
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            None,
            Some("review-failed"),
            1,
        )
        .await
        .expect("create done_with_failures delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &done_with_failures.id,
        "review_failed",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Crashed,
        "Tests failed.\n\noutcome: changes_requested",
    )
    .await;
    assert!(env
        .state
        .repo
        .finish_delegation(
            &done_with_failures.id,
            &done_with_failures.attempt_id,
            DelegationStatus::DoneWithFailures,
        )
        .await
        .expect("finish done_with_failures"));
    std::fs::create_dir_all(handoff_root(&env, &done_with_failures.id).join("review_failed"))
        .expect("handoff dir");
    std::fs::write(
        handoff_root(&env, &done_with_failures.id)
            .join("review_failed")
            .join("final_message.md"),
        "Tests failed.\n\noutcome: changes_requested",
    )
    .expect("write final message artifact");

    let cancelled = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("cancelled"), 1)
        .await
        .expect("create cancelled delegation");
    create_busy_full_subagent(&env, project_id, "parent", &cancelled.id, "impl_cancelled").await;
    env.state
        .repo
        .set_delegation_status(&cancelled.id, DelegationStatus::Cancelled)
        .await
        .expect("mark cancelled");

    let failed = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("failed"), 1)
        .await
        .expect("create failed delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &failed.id,
        "impl_failed",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Crashed,
        "Failed before handoff publication.",
    )
    .await;
    env.state
        .repo
        .set_delegation_status(&failed.id, DelegationStatus::Failed)
        .await
        .expect("mark failed");
    let running = env
        .state
        .repo
        .create_delegation(
            "parent",
            DelegationKind::Full,
            Some("workflow-implement-review"),
            Some("implement"),
            1,
        )
        .await
        .expect("create running delegation");
    create_busy_full_subagent(&env, project_id, "parent", &running.id, "impl_running").await;

    let mut config = env
        .state
        .repo
        .load_session_config("parent")
        .await
        .expect("parent config");
    config.system_prompt = "PI stable prompt".to_string();
    let transcript = vec![
        TranscriptItem::CompactionSummary(CompactionSummary::new(
            "parent",
            "old_leaf",
            "older provider summary\n\n## Delegation state at compaction time\n\nold prior delegation ledger",
            Some(123),
            TurnId(1),
        ))
        .into(),
        TranscriptItem::UserMessage(UserMessage::text("history before compaction")).into(),
        TranscriptItem::AssistantMessage(AssistantMessage {
            items: vec![AssistantItem::Text("assistant history".to_string())],
        })
        .into(),
    ];
    let snapshot = mcp_snapshot_for_session(&config).expect("load MCP snapshot");
    let native_request =
        native_compaction_request(&env.state, &config, "parent", transcript, &snapshot)
            .await
            .expect("build native compaction request");

    assert_eq!(
        native_request.prompt.stable_prefix.as_deref(),
        Some("PI stable prompt")
    );
    assert!(
        native_request.prompt.dynamic_context.is_none(),
        "compaction ledger must not be PromptSections.dynamic_context"
    );
    let native_input_texts = compaction_input_texts(&native_request.transcript);
    assert!(native_input_texts
        .iter()
        .any(|text| text.contains("older provider summary")));
    assert!(
        native_input_texts
            .iter()
            .any(|text| text.contains("old prior delegation ledger")),
        "native compaction input should preserve prior summary text, including old ledgers: {native_input_texts:?}"
    );
    assert!(native_input_texts.contains(&"history before compaction"));
    assert!(
        native_input_texts
            .iter()
            .any(|text| text.contains("## Delegation state at compaction time")),
        "native compaction input should preserve old ledger text only as ordinary prior summary text: {native_input_texts:?}"
    );
    let native_joined = native_input_texts.join("\n\n");
    assert!(!native_joined.contains(&format!("delegation_id: `{}`", running.id)));
    assert!(!native_joined.contains(&format!("delegation_id: `{}`", failed.id)));
    assert!(!native_joined.contains("## Current delegations"));

    let output = append_delegation_ledger_to_output(
        &env.state,
        "parent",
        test_compaction_output(
            "provider summary\n\n## Delegation state at compaction time\n\nold provider-emitted ledger text",
        ),
    )
    .await
    .expect("append ledger to output");
    assert!(output.summary.starts_with("provider summary\n\n"));
    assert!(
        output.summary.contains("old provider-emitted ledger text"),
        "provider output should not have old ledger text manually stripped: {}",
        output.summary
    );
    let marker = "## Delegation state at compaction time";
    assert_eq!(
        output.summary.matches(marker).count(),
        2,
        "fresh appended ledger supersedes any older ledger by being the latest section: {}",
        output.summary
    );
    let ledger = output
        .summary
        .rsplit_once(marker)
        .map(|(_, rest)| format!("{marker}{rest}"))
        .expect("post-compaction summary includes fresh ledger");
    assert!(ledger.starts_with("## Delegation state at compaction time"));
    assert!(!ledger.contains("## Current delegations"));
    assert!(ledger.contains(&format!("delegation_id: `{}`", running.id)));
    assert!(
        ledger.contains("status: running; progress: expected 1, spawned 1, terminal 0, running 1")
    );
    assert!(ledger.contains("running at compaction time; point-in-time only"));
    assert!(ledger.contains(&format!("delegation_id: `{}`", done.id)));
    assert!(ledger.contains("status: done"));
    assert!(ledger.contains("completed before compaction"));
    assert!(ledger.contains("final_message_file: `review_done/final_message.md`"));
    assert!(ledger.contains("outcome: \"approved\""));
    assert!(!ledger.contains("\"Looks good.\\n\\noutcome: approved\""));
    assert!(ledger.contains(&format!("delegation_id: `{}`", done_with_failures.id)));
    assert!(ledger.contains("status: done_with_failures"));
    assert!(ledger.contains("completed with failures before compaction"));
    assert!(ledger.contains(&format!("delegation_id: `{}`", cancelled.id)));
    assert!(ledger.contains("status: cancelled"));
    assert!(ledger.contains("cancelled before compaction"));
    assert!(ledger.contains("transcript_file: `cancelled/impl_cancelled.transcript.md`"));
    assert!(ledger.contains(&format!("delegation_id: `{}`", failed.id)));
    assert!(ledger.contains("status: failed"));
    assert!(ledger.contains("failed before compaction"));
    assert!(ledger.contains("transcript_file: null"));
    assert!(ledger.contains("Full transcript and final-message contents are not inlined"));
    assert!(!ledger.contains("## User"));
    assert!(!ledger.contains("## Assistant"));
    assert!(!ledger.contains("transcript body"));

    env.cleanup().await;
}

#[tokio::test]
async fn subagent_compaction_excludes_parent_delegation_ledger_and_sibling_state() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    std::fs::write(
        env.cwd.path().join("PI.compaction.md"),
        "Produce a subagent continuation summary.",
    )
    .expect("write compaction prompt");
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "subagent compaction ledger test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;

    let parent_delegation = env
        .state
        .repo
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            Some("workflow-explore"),
            Some("fanout"),
            2,
        )
        .await
        .expect("create parent delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &parent_delegation.id,
        "subagent_under_compaction",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "Own subagent facts should remain available.",
    )
    .await;
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &parent_delegation.id,
        "sibling_subagent",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "Sibling-only state must not be injected into subagent compaction.",
    )
    .await;

    let mut subagent_config = env
        .state
        .repo
        .load_session_config("subagent_under_compaction")
        .await
        .expect("subagent config");
    subagent_config.system_prompt = "Subagent role contract".to_string();
    let own_transcript = vec![
        TranscriptItem::UserMessage(UserMessage::text("delegated task context")).into(),
        TranscriptItem::AssistantMessage(AssistantMessage {
            items: vec![AssistantItem::Text(
                "Own subagent observation and tool facts.".to_string(),
            )],
        })
        .into(),
    ];

    let native_request = native_compaction_request(
        &env.state,
        &subagent_config,
        "subagent_under_compaction",
        own_transcript,
        &mcp_snapshot_for_session(&subagent_config).expect("load subagent MCP snapshot"),
    )
    .await
    .expect("build native subagent compaction request");
    assert_eq!(
        native_request.transcript.len(),
        2,
        "subagent native compaction should not append parent delegation state"
    );
    let native_joined = user_texts(&native_request.transcript).join("\n\n");
    assert!(native_joined.contains("delegated task context"));
    assert!(!native_joined.contains("## Delegation state at compaction time"));
    assert!(!native_joined.contains("## Current delegations"));
    assert!(!native_joined.contains(&parent_delegation.id));
    assert!(!native_joined.contains("sibling_subagent"));
    assert!(!native_joined.contains("workflow-explore"));

    let output = append_delegation_ledger_to_output(
        &env.state,
        "subagent_under_compaction",
        test_compaction_output("subagent provider summary"),
    )
    .await
    .expect("post-process subagent compaction output");
    assert_eq!(output.summary, "subagent provider summary");

    env.cleanup().await;
}

#[tokio::test]
async fn parent_compaction_ledger_bounds_large_fanout_subagents() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "delegation context bound test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;

    let delegation = env
        .state
        .repo
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            None,
            Some("large"),
            12,
        )
        .await
        .expect("create large delegation");
    for index in 0..12 {
        create_terminal_subagent(
            &env,
            project_id,
            "parent",
            &delegation.id,
            &format!("review_{index:02}"),
            "reviewer",
            SubagentType::ReadOnly,
            TurnOutcome::Graceful,
            "Done.",
        )
        .await;
    }
    assert!(env
        .state
        .repo
        .finish_delegation(
            &delegation.id,
            &delegation.attempt_id,
            DelegationStatus::Done
        )
        .await
        .expect("finish large delegation"));

    let output = append_delegation_ledger_to_output(
        &env.state,
        "parent",
        test_compaction_output("provider summary"),
    )
    .await
    .expect("append ledger to output");
    let ledger = output.summary;

    assert!(ledger.contains("## Delegation state at compaction time"));
    assert!(ledger.contains(&format!("delegation_id: `{}`", delegation.id)));
    assert!(ledger.contains("progress: expected 12, spawned 12"));
    assert!(ledger.contains("... 4 more subagent(s) omitted"));
    assert!(ledger.contains("subagent_id: `review_00`"));
    assert!(ledger.contains("subagent_id: `review_07`"));
    assert!(
        !ledger.contains("subagent_id: `review_08`"),
        "limit+1 probe row must not be rendered: {ledger}"
    );
    assert!(!ledger.contains("review_11/final_message.md"));

    env.cleanup().await;
}

#[tokio::test]
async fn parent_compaction_ledger_marks_failed_transcripts_unavailable() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "failed delegation context test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;

    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("failed"), 1)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "impl_failed",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Crashed,
        "Failed before handoff publication.",
    )
    .await;
    env.state
        .repo
        .set_delegation_status(&delegation.id, DelegationStatus::Failed)
        .await
        .expect("mark failed");

    let output = append_delegation_ledger_to_output(
        &env.state,
        "parent",
        test_compaction_output("provider summary"),
    )
    .await
    .expect("append ledger to output");
    let ledger = output.summary;

    assert!(ledger.contains(&format!("delegation_id: `{}`", delegation.id)));
    assert!(ledger.contains("status: failed"));
    assert!(ledger.contains("failed before compaction"));
    assert!(ledger.contains("subagent_id: `impl_failed`"));
    assert!(ledger.contains("transcript_file: null"));
    assert!(!ledger.contains("impl_failed/transcript.md"));
    assert!(!ledger.contains("final_message_file: `impl_failed/final_message.md`"));

    env.cleanup().await;
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
        .create_project(project_id, "runtime-test", "steer test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    create_parent(&env, project_id, "other_parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("impl"), 1)
        .await
        .expect("create delegation");
    create_busy_full_subagent(&env, project_id, "parent", &delegation.id, "impl_busy").await;

    let scoped_error = steer_subagent_core(
        &env.state,
        "other_parent",
        json!({ "subagent_id": "impl_busy", "message": "not your child" }),
    )
    .await
    .expect_err("other parent cannot steer this subagent");
    assert_eq!(scoped_error.code, "subagent_not_found");
    assert!(env
        .state
        .repo
        .queue_state("impl_busy")
        .await
        .expect("queue before valid steer")
        .queued_inputs
        .is_empty());

    let tool_result = run_delegation_tool(
        &env.state,
        "parent",
        &ToolCall {
            id: ToolCallId::new("call_steer"),
            tool_name: "steer_subagent".to_string(),
            args_json: json!({
                "subagent_id": "impl_busy",
                "message": "Please also update the docs.",
                "client_control_id": "provider-must-not-control-this",
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
    let expected_client_input_id =
        format!("subagent-control:{}:tool-call:call_steer", delegation.id);
    assert_eq!(
        queued.client_input_id.as_deref(),
        Some(expected_client_input_id.as_str()),
        "model dispatch unconditionally derives the ledger key from the tool-call id"
    );
    assert_eq!(
        queued.content.as_text(),
        Some("Please also update the docs.")
    );

    env.cleanup().await;
}

#[tokio::test]
async fn raw_session_input_steer_rejects_direct_subagent_target() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "raw steer rejection",
            &[],
            json!({}),
        )
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

    let error = enqueue_session_input(
        &env.state,
        SessionInputRequest {
            session_id: "impl_busy".to_string(),
            priority: InputPriority::Steer,
            content: UserMessage::text("raw direct steer should fail"),
            client_input_id: Some("raw-direct-steer".to_string()),
            base_leaf_id: None,
            expected_active_leaf_id: None,
        },
    )
    .await
    .expect_err("raw steer to a subagent must be rejected");
    assert_eq!(error.code, "subagent_steer_requires_parent_scope");
    assert!(env
        .state
        .repo
        .queue_state("impl_busy")
        .await
        .expect("queue state")
        .queued_inputs
        .is_empty());

    env.cleanup().await;
}

#[tokio::test]
async fn websocket_delegation_steer_subagent_uses_parent_scope() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "websocket steer test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 1)
        .await
        .expect("create delegation");
    create_busy_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "readonly_busy",
        "reviewer",
        SubagentType::ReadOnly,
    )
    .await;

    let error = crate::delegation_tools::rpc_steer_subagent(
        &env.state,
        json!({ "subagent_id": "readonly_busy", "message": "missing parent" }),
    )
    .await
    .expect_err("parent scope is required");
    assert_eq!(error.code, "invalid_params");

    let result = crate::delegation_tools::rpc_steer_subagent(
        &env.state,
        json!({
            "parent_session_id": "parent",
            "subagent_id": "readonly_busy",
            "message": "Please inspect one more file."
        }),
    )
    .await
    .expect("parent-scoped websocket steer succeeds");
    assert_eq!(result["subagent_id"], "readonly_busy");
    assert_eq!(result["queued"], true);

    env.cleanup().await;
}

#[tokio::test]
async fn model_and_websocket_steers_share_one_durable_subagent_mailbox() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "shared steer mailbox test",
            &[],
            json!({}),
        )
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

    let model_call = ToolCall {
        id: ToolCallId::new("call_model_steer"),
        tool_name: "steer_subagent".to_string(),
        args_json: json!({
            "subagent_id": "impl_busy",
            "message": "instruction from the parent model"
        })
        .to_string(),
    };
    let (model_result, web_result) = tokio::join!(
        run_delegation_tool(&env.state, "parent", &model_call),
        crate::delegation_tools::rpc_steer_subagent(
            &env.state,
            json!({
                "parent_session_id": "parent",
                "subagent_id": "impl_busy",
                "message": "instruction from the user transcript"
            }),
        ),
    );

    assert_eq!(model_result.status, agent_vocab::ToolResultStatus::Success);
    let model_output: serde_json::Value =
        serde_json::from_str(&model_result.output).expect("model steer output JSON");
    let web_output = web_result.expect("websocket steer succeeds");
    assert_eq!(model_output["queued"], true);
    assert!(model_output["input_id"].as_str().is_some());
    assert_eq!(web_output["queued"], true);
    assert!(web_output["input_id"].as_str().is_some());
    assert_ne!(model_output["input_id"], web_output["input_id"]);

    let queue = env
        .state
        .repo
        .queue_state("impl_busy")
        .await
        .expect("shared mailbox state");
    assert_eq!(queue.queued_inputs.len(), 2);
    assert!(queue
        .queued_inputs
        .iter()
        .all(|input| input.priority == InputPriority::Steer));
    let messages = queue
        .queued_inputs
        .iter()
        .filter_map(|input| input.content.as_text())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        messages,
        std::collections::BTreeSet::from([
            "instruction from the parent model",
            "instruction from the user transcript",
        ])
    );

    env.cleanup().await;
}

#[tokio::test]
async fn model_facing_delegation_tools_reject_subagent_sessions() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "subagent delegation tool rejection",
            &[],
            json!({}),
        )
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
        "impl_busy",
        &ToolCall {
            id: ToolCallId::new("call_inspect_from_subagent"),
            tool_name: "inspect_delegation".to_string(),
            args_json: json!({ "delegation_id": delegation.id }).to_string(),
        },
    )
    .await;
    assert_eq!(tool_result.status, agent_vocab::ToolResultStatus::Error);
    assert!(tool_result
        .output
        .contains("delegations_not_allowed_for_subagent"));

    env.cleanup().await;
}

#[tokio::test]
async fn model_facing_steer_subagent_queues_steer_for_running_read_only_subagent() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "runtime-test", "steer test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 1)
        .await
        .expect("create delegation");
    create_busy_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "readonly_running",
        "reviewer",
        SubagentType::ReadOnly,
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
    assert_eq!(tool_result.status, agent_vocab::ToolResultStatus::Success);
    let output: serde_json::Value =
        serde_json::from_str(&tool_result.output).expect("tool output JSON");
    assert_eq!(output["subagent_id"], "readonly_running");
    assert_eq!(output["queued"], true);
    assert!(output["input_id"].as_str().is_some());

    let queue = env
        .state
        .repo
        .queue_state("readonly_running")
        .await
        .expect("queue state");
    assert_eq!(queue.queued_inputs.len(), 1);
    let queued = &queue.queued_inputs[0];
    assert_eq!(queued.priority, InputPriority::Steer);
    assert_eq!(queued.status, QueuedInputStatus::Queued);
    assert_eq!(
        queued.content.as_text(),
        Some("Please check one more file.")
    );

    env.cleanup().await;
}

#[tokio::test]
async fn running_read_only_snapshot_reports_steerable_only_when_accepted() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "steerable snapshot test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 2)
        .await
        .expect("create delegation");
    create_busy_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "readonly_busy",
        "reviewer",
        SubagentType::ReadOnly,
    )
    .await;
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "readonly_done",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "Done.",
    )
    .await;
    // Give the terminal subagent a session title so we can assert `rpc_list`
    // surfaces `metadata.title` without any per-child session warm.
    env.state
        .repo
        .update_session_metadata(
            "readonly_done",
            &json!({
                "created_by": "test",
                "subagent": true,
                "prompt_profile": "subagent",
                "subagent_type": SubagentType::ReadOnly.as_str(),
                "role_name": "reviewer",
                "title": "Review the docs",
            }),
        )
        .await
        .expect("set subagent title");
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    let snapshot_subagents = snapshot["subagents"].as_array().expect("subagents");
    let busy = snapshot_subagents
        .iter()
        .find(|subagent| subagent["id"] == "readonly_busy")
        .expect("busy subagent");
    assert_eq!(busy["status"], "running");
    assert_eq!(busy["activity"], "running");
    assert_eq!(busy["steerable"], true);
    let done = snapshot_subagents
        .iter()
        .find(|subagent| subagent["id"] == "readonly_done")
        .expect("done subagent");
    assert_eq!(done["status"], "done");
    assert_eq!(done["steerable"], false);

    let list = rpc_list(&env.state, json!({ "parent_session_id": "parent" }))
        .await
        .expect("list delegations");
    let listed = list["delegations"].as_array().unwrap()[0]["subagents"]
        .as_array()
        .expect("listed subagents");
    let listed_busy = listed
        .iter()
        .find(|subagent| subagent["id"] == "readonly_busy")
        .expect("listed busy subagent");
    assert_eq!(listed_busy["status"], "running");
    assert_eq!(listed_busy["steerable"], true);
    let listed_done = listed
        .iter()
        .find(|subagent| subagent["id"] == "readonly_done")
        .expect("listed done subagent");
    assert_eq!(listed_done["status"], "done");
    assert_eq!(listed_done["steerable"], false);
    // `rpc_list` surfaces the subagent's `metadata.title` directly, so the web
    // Agents outline can render the name without a per-child session warm.
    assert_eq!(listed_done["title"], "Review the docs");
    assert_eq!(listed_busy["title"], serde_json::Value::Null);

    env.cleanup().await;
}

#[tokio::test]
async fn queued_work_on_boundary_subagent_reports_running_and_steerable() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "queued boundary snapshot test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 1)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "readonly_boundary",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "Done for now.",
    )
    .await;
    env.state
        .repo
        .enqueue_user_input(
            "readonly_boundary",
            InputPriority::Steer,
            &UserMessage::text("queued work before barrier"),
            Some("queued-boundary-work"),
            None,
        )
        .await
        .expect("queue steer");

    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    assert_eq!(snapshot["status"], "running");
    assert_eq!(snapshot["progress"]["terminal"], 0);
    assert_eq!(snapshot["progress"]["running"], 1);
    let subagent = &snapshot["subagents"].as_array().unwrap()[0];
    assert_eq!(subagent["status"], "queued");
    assert_eq!(subagent["activity"], "queued");
    assert_eq!(subagent["outcome"], serde_json::Value::Null);
    assert_eq!(subagent["steerable"], true);

    let list = rpc_list(&env.state, json!({ "parent_session_id": "parent" }))
        .await
        .expect("list delegations");
    let listed = &list["delegations"].as_array().unwrap()[0]["subagents"]
        .as_array()
        .unwrap()[0];
    assert_eq!(listed["status"], "queued");
    assert_eq!(listed["activity"], "queued");
    assert_eq!(listed["steerable"], true);

    let consumed = env
        .state
        .repo
        .take_next_queued_input("readonly_boundary")
        .await
        .expect("take queued input")
        .expect("queued input exists");
    env.state
        .repo
        .persist_outputs(
            "readonly_boundary",
            agent_store::OutputBatch::new(&[], None, &[], &[]).with_consumed_input(Some(consumed)),
        )
        .await
        .expect("mark queued input consumed");
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    assert_eq!(snapshot["progress"]["terminal"], 1);
    let subagent = &snapshot["subagents"].as_array().unwrap()[0];
    assert_eq!(subagent["status"], "done");
    assert_eq!(subagent["steerable"], false);

    env.cleanup().await;
}

#[tokio::test]
async fn boundary_controls_settle_without_double_boundary_and_keep_mailbox_live() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "boundary control matrix",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 4)
        .await
        .expect("create delegation");

    for (child, combined, boundary_action) in [
        ("combined_no_action", true, false),
        ("combined_boundary_action", true, true),
        ("interrupt_no_action", false, false),
        ("interrupt_boundary_action", false, true),
    ] {
        create_terminal_subagent(
            &env,
            project_id,
            "parent",
            &delegation.id,
            child,
            "reviewer",
            SubagentType::ReadOnly,
            TurnOutcome::Graceful,
            "Ready for more work.",
        )
        .await;
        let mut config = env
            .state
            .repo
            .load_session_config(child)
            .await
            .expect("load child config");
        config.metadata["harness"] = json!(true);
        env.state
            .repo
            .configure_session(child, &config)
            .await
            .expect("make post-control model dispatch deterministic");
        let ordinary_key = format!("{child}-ordinary");
        env.state
            .repo
            .enqueue_user_input(
                child,
                InputPriority::FollowUp,
                &UserMessage::text(format!("ordinary work for {child}")),
                Some(&ordinary_key),
                None,
            )
            .await
            .expect("queue work on boundary child");

        let boundary_job = if boundary_action {
            let created = env
                .state
                .repo
                .create_compaction_action(child, CompactionTrigger::Manual)
                .await
                .expect("create boundary-hosted compaction action");
            let action_row_id = created.job.action_row_id.clone();
            env.state.tasks.lock().expect("task registry").insert(
                action_row_id.clone(),
                RunningTask {
                    session_id: child.to_string(),
                    action_row_id,
                    registration_id: TaskRegistrationId::new(),
                    post_compaction_dispatch_lease: None,
                    kind: ActionKind::Compaction,
                    handle: tokio::spawn(std::future::pending()),
                },
            );
            Some(created.job)
        } else {
            None
        };

        let result = if combined {
            steer_subagent_core(
                &env.state,
                "parent",
                json!({
                    "subagent_id": child,
                    "message": format!("priority control for {child}"),
                    "interrupt": true,
                    "client_control_id": format!("{child}-control"),
                }),
            )
            .await
            .expect("combined boundary control")
        } else {
            interrupt_subagent_core(
                &env.state,
                "parent",
                json!({
                    "subagent_id": child,
                    "client_control_id": format!("{child}-control"),
                }),
            )
            .await
            .expect("interrupt-only boundary control")
        };
        assert_eq!(result["accepted"], true);
        assert_eq!(result["phase"], "ready");
        assert_eq!(result["interrupted"], boundary_action);
        assert_eq!(
            result["interrupt_outcome"],
            if boundary_action {
                json!("interrupted")
            } else {
                json!("already_between_turns")
            }
        );
        assert_eq!(
            result["queued"], false,
            "the control ledger/message must make progress"
        );

        let input_id = result["input_id"].as_str().expect("control input id");
        let control = env
            .state
            .repo
            .get_subagent_control_by_input_id(child, input_id)
            .await
            .expect("load settled control")
            .expect("control remains durable");
        assert_eq!(control.phase, SubagentControlPhase::Ready);
        assert_eq!(control.status, QueuedInputStatus::Consumed);
        assert_eq!(control.interrupted, boundary_action);
        if let Some(job) = boundary_job {
            assert!(
                !env.state
                    .repo
                    .action_can_complete(child, &job.action_row_id, &job.attempt_id, None)
                    .await
                    .expect("boundary action completion fence"),
                "the captured boundary action generation is no longer completable"
            );
        }

        let ordinary = env
            .state
            .repo
            .find_client_input(child, &ordinary_key)
            .await
            .expect("load ordinary queued work")
            .expect("ordinary work remains recorded");
        assert_eq!(
            ordinary.status,
            if combined {
                QueuedInputStatus::Queued
            } else {
                QueuedInputStatus::Consumed
            },
            "combined text runs ahead of normal follow-ups; interrupt-only unblocks them"
        );
        assert!(
            env.state
                .repo
                .has_unfinished_actions(child)
                .await
                .expect("new generation state"),
            "the next accepted mailbox item starts normally instead of wedging"
        );

        let history = env
            .state
            .repo
            .active_branch(child)
            .await
            .expect("load valid child transcript");
        assert_eq!(
            history
                .entries
                .iter()
                .filter(|entry| matches!(
                    entry.item,
                    TranscriptItem::TurnFinished {
                        outcome: TurnOutcome::Graceful,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            history
                .entries
                .iter()
                .filter(|entry| matches!(
                    entry.item,
                    TranscriptItem::TurnFinished {
                        outcome: TurnOutcome::Interrupted,
                        ..
                    }
                ))
                .count(),
            0,
            "interrupting boundary-hosted work must not append a second turn boundary"
        );
        AgentSession::from_stored_session_preserving_open_turn(
            env.state
                .repo
                .load_stored_session(child)
                .await
                .expect("load stored child transcript"),
        )
        .expect("the resulting transcript remains structurally valid");
        tokio::task::yield_now().await;
        drop(SessionDriver::acquire(&env.state, child).await);
    }

    env.cleanup().await;
}

#[tokio::test]
async fn aborted_ready_steer_tool_future_is_recovered_by_live_control_sweep() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "ready steer live recovery",
            &[],
            json!({}),
        )
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
        "ready_child",
        "implementer",
        SubagentType::Full,
        TurnOutcome::Graceful,
        "Waiting.",
    )
    .await;
    let mut config = env
        .state
        .repo
        .load_session_config("ready_child")
        .await
        .expect("load child config");
    config.metadata["harness"] = json!(true);
    env.state
        .repo
        .configure_session("ready_child", &config)
        .await
        .expect("configure harness child");
    env.state
        .repo
        .enqueue_user_input(
            "ready_child",
            InputPriority::FollowUp,
            &UserMessage::text("normal follow-up"),
            Some("normal-follow-up"),
            None,
        )
        .await
        .expect("keep boundary child active");

    env.state
        .pause_subagent_control_after_commit
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let committed_signal = env.state.subagent_control_committed.notified();
    let state = env.state.clone();
    let owner = tokio::spawn(async move {
        steer_subagent_core(
            &state,
            "parent",
            json!({
                "subagent_id": "ready_child",
                "message": "accepted before parent abort",
                "client_control_id": "ready-abort",
            }),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), committed_signal)
        .await
        .expect("actual steer future reached durable commit");
    let client_input_id = format!("subagent-control:{}:ready-abort", delegation.id);
    let committed = env
        .state
        .repo
        .get_scoped_subagent_control(
            "ready_child",
            &client_input_id,
            "parent",
            &delegation.id,
            &UserMessage::text("accepted before parent abort"),
            false,
        )
        .await
        .expect("load committed control")
        .expect("commit notification follows durable enqueue");
    assert_eq!(committed.phase, SubagentControlPhase::Ready);
    assert_eq!(committed.status, QueuedInputStatus::Queued);
    owner.abort();
    let _ = owner.await;
    env.state
        .pause_subagent_control_after_commit
        .store(false, std::sync::atomic::Ordering::SeqCst);

    crate::sweep_pending_subagent_controls_once(&env.state)
        .await
        .expect("bounded live sweep recovers ready steer");
    let recovered = env
        .state
        .repo
        .get_subagent_control_by_input_id("ready_child", &committed.input_id)
        .await
        .expect("load recovered ready control")
        .expect("control remains durable");
    assert_eq!(recovered.phase, SubagentControlPhase::Ready);
    assert_eq!(recovered.status, QueuedInputStatus::Consumed);
    assert!(env
        .state
        .repo
        .has_unfinished_actions("ready_child")
        .await
        .expect("steer-created generation"));
    let normal = env
        .state
        .repo
        .find_client_input("ready_child", "normal-follow-up")
        .await
        .expect("load normal follow-up")
        .expect("normal follow-up remains recorded");
    assert_eq!(
        normal.status,
        QueuedInputStatus::Queued,
        "recovered scoped steer keeps steer priority"
    );
    let history = env
        .state
        .repo
        .active_branch("ready_child")
        .await
        .expect("load recovered transcript");
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| matches!(
                &entry.item,
                TranscriptItem::UserMessage(message)
                    if message.as_text() == Some("accepted before parent abort")
            ))
            .count(),
        1
    );

    env.cleanup().await;
}

#[tokio::test]
async fn interrupt_only_status_reload_failure_returns_accepted_fallback() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "interrupt accepted fallback",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    create_busy_full_subagent(&env, project_id, "parent", &delegation.id, "fallback_child").await;
    env.state
        .fail_subagent_control_reload_after_commit
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let result = interrupt_subagent_core(
        &env.state,
        "parent",
        json!({
            "subagent_id": "fallback_child",
            "client_control_id": "reload-failure",
        }),
    )
    .await
    .expect("a postcommit reload failure cannot reject a durable interrupt");
    assert_eq!(result["accepted"], true);
    assert_eq!(result["drive_status"], "pending");
    assert_eq!(result["phase"], "pending_interrupt");
    assert_eq!(result["interrupted"], serde_json::Value::Null);
    assert!(result["drive_error"]
        .as_str()
        .expect("fallback diagnostic")
        .contains("injected accepted-control status reload failure"));
    let control = env
        .state
        .repo
        .get_scoped_subagent_interrupt(
            "fallback_child",
            &format!("subagent-control:{}:reload-failure", delegation.id),
            "parent",
            &delegation.id,
        )
        .await
        .expect("reload durable interrupt after injected failure")
        .expect("interrupt remains accepted");
    assert_eq!(control.phase, SubagentControlPhase::Ready);
    assert_eq!(control.status, QueuedInputStatus::Consumed);
    assert!(control.interrupted);
    tokio::task::yield_now().await;
    drop(SessionDriver::acquire(&env.state, "fallback_child").await);

    env.cleanup().await;
}

#[tokio::test]
async fn steer_subagent_rejects_idle_terminal_subagent_without_reactivating_it() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "runtime-test", "steer test", &[], json!({}))
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 1)
        .await
        .expect("create delegation");
    let active_leaf = "ro_idle_finish";
    let entries = vec![
        TranscriptStorageNode {
            id: "ro_idle_start".to_string(),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: "ro_idle_u".to_string(),
            parent_id: Some("ro_idle_start".to_string()),
            timestamp_ms: 2,
            item: TranscriptItem::UserMessage(UserMessage::text("keep working")),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: active_leaf.to_string(),
            parent_id: Some("ro_idle_u".to_string()),
            timestamp_ms: 3,
            item: TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            provider_replay: Vec::new(),
        },
    ];
    env.state
        .repo
        .start_session_outputs_with_parent(
            "ro_idle",
            &session_config(
                &env,
                project_id,
                json!({ "created_by": "test", "role_name": "reviewer" }),
            ),
            &entries,
            Some(active_leaf),
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("keep working"),
            None,
            Some("parent"),
            Some(SubagentType::ReadOnly),
            Some(&delegation.id),
        )
        .await
        .expect("create idle nonterminal subagent");
    assert!(env
        .state
        .repo
        .active_leaf_is_turn_boundary("ro_idle")
        .await
        .expect("terminal boundary"));
    assert_eq!(
        env.state.repo.activity("ro_idle").await.expect("activity"),
        agent_store::SessionActivity::Idle
    );

    let error = steer_subagent_core(
        &env.state,
        "parent",
        json!({ "subagent_id": "ro_idle", "message": "one more thing" }),
    )
    .await
    .expect_err("idle terminal subagent rejected");
    assert_eq!(error.code, "delegation_not_running");
    assert!(env
        .state
        .repo
        .queue_state("ro_idle")
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
        .create_project(project_id, "runtime-test", "steer test", &[], json!({}))
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
async fn terminal_historical_control_replays_without_recovering_child() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "terminal control replay",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, None, 1)
        .await
        .expect("create delegation");
    create_busy_full_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "terminal_replay_child",
    )
    .await;
    let first = steer_subagent_core(
        &env.state,
        "parent",
        json!({
            "subagent_id": "terminal_replay_child",
            "message": "remember this accepted control",
            "client_control_id": "terminal-replay",
        }),
    )
    .await
    .expect("accept control while delegation runs");
    assert_eq!(first["accepted"], true);
    assert_eq!(first["replayed"], false);

    // Wait behind any detached driver spawned by the accepted call, then make
    // the durable scope terminal and discard all volatile child state.
    tokio::task::yield_now().await;
    drop(SessionDriver::acquire(&env.state, "terminal_replay_child").await);
    env.state
        .repo
        .set_delegation_status(&delegation.id, DelegationStatus::Done)
        .await
        .expect("mark delegation terminal");
    env.state
        .active
        .lock()
        .await
        .remove("terminal_replay_child");

    let replay = steer_subagent_core(
        &env.state,
        "parent",
        json!({
            "subagent_id": "terminal_replay_child",
            "message": "remember this accepted control",
            "client_control_id": "terminal-replay",
        }),
    )
    .await
    .expect("historical terminal replay remains readable");
    assert_eq!(replay["accepted"], true);
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["input_id"], first["input_id"]);
    assert!(
        !env.state
            .active
            .lock()
            .await
            .contains_key("terminal_replay_child"),
        "terminal replay returns before child recovery/reactivation"
    );

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
        .create_project(project_id, "runtime-test", "cancel test", &[], json!({}))
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
            None,
        )
        .await
        .expect("queue follow-up before cancellation");
    let pending_control = env
        .state
        .repo
        .enqueue_scoped_subagent_steer(
            "parent",
            &delegation.id,
            "impl_to_cancel",
            &UserMessage::text("pending combined control to settle"),
            &format!("subagent-control:{}:cancel-pending", delegation.id),
            true,
        )
        .await
        .expect("enqueue pending combined control")
        .expect("running delegation accepts pending control");

    let result = cancel_core(
        &env.state,
        "parent",
        json!({ "delegation_id": delegation.id }),
    )
    .await
    .expect("cancel delegation");
    assert_eq!(result["cancelled"], true);
    assert_eq!(result["delegation_id"], delegation.id);
    let expected_handoff_dir = handoff_root(&env, &delegation.id)
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        result["handoff_dir"].as_str(),
        Some(expected_handoff_dir.as_str())
    );
    let result_subagents = result["subagents"].as_array().expect("subagents array");
    assert_eq!(result_subagents.len(), 1);
    assert_eq!(result_subagents[0]["subagent_id"], "impl_to_cancel");
    assert_eq!(
        result_subagents[0]["transcript_file"],
        "cancelled/impl_to_cancel.transcript.md"
    );
    assert!(result_subagents[0].get("transcript").is_none());
    let transcript_path = handoff_root(&env, &delegation.id)
        .join("cancelled")
        .join("impl_to_cancel.transcript.md");
    let transcript = std::fs::read_to_string(transcript_path).expect("transcript readable");
    assert!(transcript.contains("## User"));
    assert!(transcript.contains("keep working"));
    assert!(transcript.contains("## Assistant"));
    assert!(transcript.contains("working..."));
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    let subagent = snapshot["subagents"].as_array().unwrap()[0].clone();
    assert_eq!(snapshot["status"], "cancelled");
    assert_eq!(subagent["status"], "cancelled");
    assert_eq!(subagent["final_message_file"], serde_json::Value::Null);
    assert_eq!(
        subagent["transcript_file"],
        format!("cancelled/{}.transcript.md", "impl_to_cancel")
    );
    assert!(subagent.get("final_message_path").is_none());
    assert!(subagent.get("transcript_path").is_none());
    assert!(subagent.get("cancellation_transcript_path").is_none());
    let list = rpc_list(&env.state, json!({ "parent_session_id": "parent" }))
        .await
        .expect("list delegations");
    let listed_subagent = &list["delegations"].as_array().unwrap()[0]["subagents"]
        .as_array()
        .unwrap()[0];
    assert_eq!(listed_subagent["transcript_file"], serde_json::Value::Null);
    assert!(listed_subagent
        .get("cancellation_transcript_path")
        .is_none());
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
    assert!(
        queue.queued_inputs.is_empty(),
        "whole-delegation cancellation settles every active child input"
    );
    let cancelled_control = env
        .state
        .repo
        .get_subagent_control_by_input_id("impl_to_cancel", &pending_control.input_id)
        .await
        .expect("load cancelled control")
        .expect("control ledger retained");
    assert_eq!(cancelled_control.status, QueuedInputStatus::Cancelled);
    assert_eq!(cancelled_control.phase, SubagentControlPhase::Cancelled);
    assert!(!env
        .state
        .repo
        .sessions_with_recoverable_subagent_controls()
        .await
        .expect("pending control sessions after cancellation")
        .contains(&"impl_to_cancel".to_string()));

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
        .create_project(
            project_id,
            "runtime-test",
            "cancel race test",
            &[],
            json!({}),
        )
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
async fn terminal_subagent_wakes_parent_before_fanout_barrier_and_allows_scoped_steering() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "partial wakeup test",
            &[],
            json!({}),
        )
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
            Some("parallel investigation"),
            2,
        )
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "fast_child",
        "explorer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "Found a decisive issue.\n\noutcome: done",
    )
    .await;
    create_busy_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "slow_child",
        "explorer",
        SubagentType::ReadOnly,
    )
    .await;

    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("partial wakeup");
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
    assert_eq!(
        parent_completion_observations(&env, "parent", &delegation.id)
            .await
            .len(),
        0,
        "partial wakeup must not masquerade as terminal completion"
    );
    let observations = parent_delegation_observations(&env, "parent", &delegation.id).await;
    assert_eq!(observations.len(), 1);
    let partial = &observations[0];
    assert!(partial
        .summary
        .as_deref()
        .unwrap_or_default()
        .contains("Subagent fast_child finished"));
    assert_eq!(partial.result_json["status"], "running");
    assert_eq!(partial.result_json["progress"]["terminal"], 1);
    assert_eq!(partial.result_json["progress"]["running"], 1);
    let fast = partial.result_json["subagents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|subagent| subagent["id"] == "fast_child")
        .unwrap();
    assert_eq!(fast["status"], "done");
    assert_eq!(fast["outcome"], "done");
    assert_eq!(fast["final_message_file"], "fast_child/final_message.md");
    assert!(handoff_root(&env, &delegation.id)
        .join("fast_child")
        .join("final_message.md")
        .exists());
    let slow = partial.result_json["subagents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|subagent| subagent["id"] == "slow_child")
        .unwrap();
    assert_eq!(slow["status"], "running");
    assert_eq!(slow["steerable"], true);
    assert_eq!(slow["final_message_file"], serde_json::Value::Null);

    let scoped_error = steer_subagent_core(
        &env.state,
        "other_parent",
        json!({ "subagent_id": "slow_child", "message": "not your child" }),
    )
    .await
    .expect_err("partial wakeup must not reintroduce raw/direct child steering");
    assert_eq!(scoped_error.code, "subagent_not_found");
    let steer = steer_subagent_core(
        &env.state,
        "parent",
        json!({ "subagent_id": "slow_child", "message": "You can stop after checking the new clue." }),
    )
    .await
    .expect("steer running read-only subagent");
    assert_eq!(steer["queued"], true);
    let queue = env
        .state
        .repo
        .queue_state("slow_child")
        .await
        .expect("queue state");
    assert_eq!(queue.queued_inputs.len(), 1);
    assert_eq!(queue.queued_inputs[0].priority, InputPriority::Steer);

    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("partial replay");
    assert_eq!(
        parent_delegation_observations(&env, "parent", &delegation.id)
            .await
            .len(),
        1,
        "partial child wakeup is deterministic and idempotent"
    );

    env.cleanup().await;
}

#[tokio::test]
async fn partial_wakeup_waits_until_expected_fanout_members_have_spawned() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "partial spawn wakeup test",
            &[],
            json!({}),
        )
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
        "fast_only_child",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "Finished before sibling spawned.\n\noutcome: done",
    )
    .await;

    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("partial spawn check");
    assert_eq!(
        parent_delegation_observations(&env, "parent", &delegation.id)
            .await
            .len(),
        0,
        "no running partial should be published before the expected fan-out set exists"
    );

    create_busy_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "late_sibling",
        "reviewer",
        SubagentType::ReadOnly,
    )
    .await;

    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("partial after full spawn");
    let observations = parent_delegation_observations(&env, "parent", &delegation.id).await;
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].result_json["status"], "running");
    assert_eq!(observations[0].result_json["progress"]["spawned"], 2);
    assert_eq!(
        observations[0].result_json["subagents"]
            .as_array()
            .expect("subagents")
            .len(),
        2,
        "the delivered snapshot should include the late-spawned sibling"
    );

    env.cleanup().await;
}

#[tokio::test]
async fn partial_wakeup_queues_only_one_terminal_child_per_parent_decision_point_and_cancels_on_cancel(
) {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "partial wakeup queue test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let parent_lock = SessionDriver::acquire(&env.state, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 3)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "fast_a",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "First result.\n\noutcome: done",
    )
    .await;
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "fast_b",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "Second result.\n\noutcome: done",
    )
    .await;
    create_busy_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "slow_child",
        "reviewer",
        SubagentType::ReadOnly,
    )
    .await;

    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("partial wakeup");
    assert_eq!(
        parent_delegation_observations(&env, "parent", &delegation.id)
            .await
            .len(),
        0,
        "held parent lock keeps the partial queued"
    );
    let partial_ids = parent_partial_client_input_ids(&env, "parent", &delegation).await;
    assert_eq!(
        partial_ids.len(),
        1,
        "only one partial wakeup should be queued before the parent decides"
    );
    let record = env
        .state
        .repo
        .find_client_input("parent", &partial_ids[0])
        .await
        .expect("find partial")
        .expect("partial row");
    assert_eq!(record.status, QueuedInputStatus::Queued);

    drop(parent_lock);
    let cancelled = cancel_core(
        &env.state,
        "parent",
        json!({ "delegation_id": delegation.id }),
    )
    .await
    .expect("cancel delegation");
    assert_eq!(cancelled["cancelled"], true);
    assert!(
        parent_partial_client_input_ids(&env, "parent", &delegation)
            .await
            .is_empty(),
        "queued partial wakeup should be removed when cancellation wins"
    );
    let record = env
        .state
        .repo
        .find_client_input("parent", &partial_ids[0])
        .await
        .expect("find cancelled partial")
        .expect("partial row remains for idempotency");
    assert_eq!(record.status, QueuedInputStatus::Cancelled);

    env.cleanup().await;
}

#[tokio::test]
async fn final_completion_cancels_stale_queued_partial_wakeup() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "partial completion race test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let parent_lock = SessionDriver::acquire(&env.state, "parent").await;
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
        "fast_child",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "First result.\n\noutcome: done",
    )
    .await;
    let slow_boundary = create_running_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "slow_child",
        "reviewer",
        TurnOutcome::Graceful,
    )
    .await;

    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("partial wakeup");
    let partial_ids = parent_partial_client_input_ids(&env, "parent", &delegation).await;
    assert_eq!(
        partial_ids.len(),
        1,
        "partial should be queued while parent is busy"
    );

    settle_subagent_terminal(&env, "slow_child", &slow_boundary).await;
    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("final completion");
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
    assert!(
        parent_partial_client_input_ids(&env, "parent", &delegation)
            .await
            .is_empty(),
        "stale queued running partial should be cancelled before final completion wakeup remains"
    );
    let stale_partial = env
        .state
        .repo
        .find_client_input("parent", &partial_ids[0])
        .await
        .expect("find stale partial")
        .expect("stale partial row remains for idempotency");
    assert_eq!(stale_partial.status, QueuedInputStatus::Cancelled);
    let final_key = format!(
        "delegation-steer:{}:{}",
        delegation.id, delegation.attempt_id
    );
    let final_input = env
        .state
        .repo
        .find_client_input("parent", &final_key)
        .await
        .expect("find final wakeup")
        .expect("final wakeup exists");
    assert_eq!(final_input.status, QueuedInputStatus::Queued);

    drop(parent_lock);
    env.cleanup().await;
}

#[tokio::test]
async fn consumed_partial_wakeup_triggers_next_already_terminal_sibling() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "partial next sibling test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let parent_lock = SessionDriver::acquire(&env.state, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::ReadonlyFanout, None, None, 3)
        .await
        .expect("create delegation");
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "first_done",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "First result.\n\noutcome: done",
    )
    .await;
    let second_boundary = create_running_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "second_later",
        "reviewer",
        TurnOutcome::Graceful,
    )
    .await;
    create_busy_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "still_running",
        "reviewer",
        SubagentType::ReadOnly,
    )
    .await;

    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("first partial");
    let partial_ids = parent_partial_client_input_ids(&env, "parent", &delegation).await;
    assert_eq!(partial_ids.len(), 1);
    assert!(partial_ids[0].ends_with(":first_done"));

    settle_subagent_terminal(&env, "second_later", &second_boundary).await;
    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("second terminal while first partial queued");
    assert_eq!(
        parent_partial_client_input_ids(&env, "parent", &delegation).await,
        partial_ids,
        "do not pre-publish the second terminal sibling before the parent handles the first"
    );

    drop(parent_lock);
    let consumed_first = env
        .state
        .repo
        .take_next_queued_steer_input("parent")
        .await
        .expect("take first partial")
        .expect("first partial queued");
    assert_eq!(
        consumed_first.client_input_id.as_deref(),
        Some(partial_ids[0].as_str())
    );
    env.state
        .repo
        .persist_outputs(
            "parent",
            OutputBatch::new(&[], None, &[], &[]).with_consumed_input(Some(consumed_first)),
        )
        .await
        .expect("mark first partial consumed");

    publish_next_partial_after_parent_decision(&env.state, "parent", Some(&partial_ids[0]))
        .await
        .expect("next partial after parent decision");
    let observations = parent_delegation_observations(&env, "parent", &delegation.id).await;
    assert_eq!(
        observations.len(),
        1,
        "the next already-terminal sibling should get its own parent decision point"
    );
    assert!(observations[0]
        .summary
        .as_deref()
        .unwrap_or_default()
        .contains("Subagent second_later finished"));
    assert_eq!(observations[0].result_json["status"], "running");
    assert_eq!(observations[0].result_json["progress"]["terminal"], 2);
    assert_eq!(observations[0].result_json["progress"]["running"], 1);

    env.cleanup().await;
}

#[tokio::test]
async fn boot_sweep_repairs_partial_subagent_wakeup_for_still_running_delegation() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "partial boot repair test",
            &[],
            json!({}),
        )
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
        "finished",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "Finished early.\n\noutcome: done",
    )
    .await;
    create_running_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "not_done",
        "reviewer",
        TurnOutcome::Graceful,
    )
    .await;

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
    let observations = parent_delegation_observations(&env, "parent", &delegation.id).await;
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].result_json["status"], "running");
    assert_eq!(observations[0].result_json["progress"]["terminal"], 1);
    assert_eq!(observations[0].result_json["progress"]["running"], 1);

    sweep_running_delegations_on_boot(&env.state).await;
    assert_eq!(
        parent_delegation_observations(&env, "parent", &delegation.id)
            .await
            .len(),
        1,
        "boot repair must be idempotent for partial wakeups"
    );

    env.cleanup().await;
}

#[tokio::test]
async fn cancelling_after_partial_wakeup_preserves_completed_child_handoff_only() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "partial cancel test",
            &[],
            json!({}),
        )
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
        "finished",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Graceful,
        "This is enough to stop the rest.\n\noutcome: done",
    )
    .await;
    create_busy_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "still_running",
        "reviewer",
        SubagentType::ReadOnly,
    )
    .await;

    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("partial wakeup");
    assert!(handoff_root(&env, &delegation.id)
        .join("finished")
        .join("final_message.md")
        .exists());
    assert!(
        handoff_root(&env, &delegation.id)
            .join("still_running")
            .join("transcript.md")
            .exists(),
        "running snapshots write normal transcripts before cancellation"
    );

    let cancelled = cancel_core(
        &env.state,
        "parent",
        json!({ "delegation_id": delegation.id }),
    )
    .await
    .expect("cancel delegation");
    assert_eq!(cancelled["cancelled"], true);
    let read = read_handoff_file_core(
        &env.state,
        "parent",
        json!({
            "delegation_id": delegation.id,
            "subagent_id": "finished",
            "file": "final_message.md",
        }),
    )
    .await
    .expect("completed child final message remains readable after cancellation");
    assert!(read["content"]
        .as_str()
        .unwrap()
        .contains("This is enough to stop the rest."));
    let running_transcript = read_handoff_file_core(
        &env.state,
        "parent",
        json!({
            "delegation_id": delegation.id,
            "subagent_id": "still_running",
            "file": "transcript.md",
        }),
    )
    .await
    .expect_err("stale normal transcript for cancelled running child is not readable");
    assert_eq!(running_transcript.code, "handoff_file_not_found");
    let finished_transcript = read_handoff_file_core(
        &env.state,
        "parent",
        json!({
            "delegation_id": delegation.id,
            "subagent_id": "finished",
            "file": "transcript.md",
        }),
    )
    .await
    .expect_err("normal transcripts are not exposed after cancellation");
    assert_eq!(finished_transcript.code, "handoff_file_not_found");

    env.cleanup().await;
}

#[tokio::test]
async fn barrier_wakes_parent_once_after_all_terminal_with_handoff_for_every_subagent() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "runtime-test", "barrier test", &[], json!({}))
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
        "All good.\n\noutcome: approved",
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
    assert_eq!(
        wakeup_observations_to_parent(&env, "parent", &delegation.id).await,
        0
    );

    // Settle the second subagent terminal at a Crashed boundary — the barrier
    // classifies a non-graceful TurnFinished as a failure, exactly as a child
    // that died mid-task and was recovered to a boundary would appear.
    settle_subagent_terminal(&env, "still_running", &boundary_leaf).await;

    // Now all terminal -> exactly one durable wakeup publication,
    // done_with_failures, and handoff for all. The parent may still be busy
    // with the earlier partial-progress observation, so the final observation
    // can legitimately remain queued rather than already be in its transcript.
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
    assert_eq!(
        durable_parent_wakeup_observation_events(&env, "parent", &delegation)
            .await
            .len(),
        1
    );

    // Re-delivered events must not double-publish a wakeup (idempotent via the
    // CAS).
    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("barrier (replay)");
    sweep_running_delegations_on_boot(&env.state).await;
    assert_eq!(
        durable_parent_wakeup_observation_events(&env, "parent", &delegation)
            .await
            .len(),
        1
    );

    // Handoff: inspect_delegation is the control-flow snapshot; the
    // handoff dir contains per-subagent files for EVERY subagent (incl. failed)
    // but no delegation-root index.json.
    let root = handoff_root(&env, &delegation.id);
    assert!(!root.join("index.json").exists());
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    let wakeup_observation =
        published_parent_completion_observation(&env, "parent", &delegation).await;
    assert_eq!(wakeup_observation.tool_name, "inspect_delegation");
    assert_eq!(
        wakeup_observation.args_json,
        format!("{{\"delegation_id\":\"{}\"}}", delegation.id)
    );
    assert!(wakeup_observation
        .summary
        .as_deref()
        .unwrap_or_default()
        .contains("completed with status done_with_failures"));
    let fallback = wakeup_observation.render_text().expect("fallback renders");
    assert!(!fallback.contains("index.json"));
    assert!(!fallback.contains("## User"));
    assert!(fallback.contains("large prompts/messages are not inlined"));
    let wakeup_snapshot = wakeup_observation.result_json.clone();
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
            subagent["final_message_file"],
            format!("{id}/final_message.md")
        );
        assert_eq!(subagent["transcript_file"], format!("{id}/transcript.md"));
        assert!(subagent.get("final_message_path").is_none());
        assert!(subagent.get("transcript_path").is_none());
    }
    let ok = subagents.iter().find(|s| s["id"] == "ok_a").unwrap();
    assert_eq!(ok["role"], "reviewer");
    assert_eq!(ok["type"], "read_only");
    assert_eq!(ok["subagent_type"], "read_only");
    assert_eq!(ok["status"], "done");
    assert_eq!(ok["outcome"], "approved");
    let wakeup_ok = wakeup_snapshot["subagents"]
        .as_array()
        .expect("wakeup subagents array")
        .iter()
        .find(|subagent| subagent["id"] == "ok_a")
        .expect("ok_a in wakeup snapshot");
    assert_eq!(wakeup_ok["outcome"], "approved");
    assert_eq!(wakeup_ok["transcript_file"], "ok_a/transcript.md");
    assert_eq!(ok["steerable"], false);
    let failed = subagents
        .iter()
        .find(|s| s["id"] == "still_running")
        .unwrap();
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["outcome"], serde_json::Value::Null);

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
        .create_project(
            project_id,
            "runtime-test",
            "inspect refresh test",
            &[],
            json!({}),
        )
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
        "Found the answer.\n\noutcome: done",
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
    assert_eq!(done["outcome"], "done");
    assert_eq!(done["final_message_file"], "done_child/final_message.md");
    assert!(
        std::fs::read_to_string(root.join("done_child").join("final_message.md"))
            .expect("terminal final message artifact")
            .contains("Found the answer."),
        "running inspect snapshots include final-message refs for completion-terminal children"
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
    assert_eq!(running["activity"], "running");
    assert_eq!(running["status"], "running");
    assert_eq!(running["outcome"], serde_json::Value::Null);
    assert_eq!(running["final_message_file"], serde_json::Value::Null);
    assert!(running.get("final_message_path").is_none());
    assert!(root.join("running_child").join("transcript.md").exists());
    assert!(
        !root.join("running_child").join("final_message.md").exists(),
        "mid-turn child should not get a premature final_message artifact"
    );

    let list = rpc_list(&env.state, json!({ "parent_session_id": "parent" }))
        .await
        .expect("list delegations");
    let listed = list["delegations"]
        .as_array()
        .expect("delegations array")
        .iter()
        .find(|row| row["delegation_id"] == delegation.id)
        .expect("listed delegation");
    assert_eq!(listed["progress"]["expected"], 2);
    assert_eq!(listed["progress"]["spawned"], 2);
    assert_eq!(listed["progress"]["terminal"], 1);
    assert_eq!(listed["progress"]["running"], 1);
    assert_eq!(listed["progress"]["failed"], 0);
    let listed_done = listed["subagents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|subagent| subagent["id"] == "done_child")
        .unwrap();
    assert_eq!(listed_done["status"], "done");
    assert_eq!(listed_done["activity"], "idle");
    assert_eq!(listed_done["outcome"], serde_json::Value::Null);
    assert_eq!(listed_done["final_message_file"], serde_json::Value::Null);
    assert_eq!(listed_done["transcript_file"], serde_json::Value::Null);
    assert_list_subagent_has_only_compact_fields(listed_done);
    let listed_running = listed["subagents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|subagent| subagent["id"] == "running_child")
        .unwrap();
    assert_eq!(listed_running["status"], "running");
    assert_eq!(listed_running["activity"], "running");
    assert_eq!(listed_running["outcome"], serde_json::Value::Null);
    assert_eq!(listed_running["transcript_file"], serde_json::Value::Null);
    assert_list_subagent_has_only_compact_fields(listed_running);

    // Mutate the stale artifact on disk; a later inspect must refresh it from
    // the durable Postgres transcript before returning the file path.
    std::fs::write(
        root.join("done_child").join("transcript.md"),
        "stale local artifact",
    )
    .expect("overwrite transcript artifact");
    std::fs::write(
        root.join("done_child").join("final_message.md"),
        "stale local final message",
    )
    .expect("overwrite final message artifact");
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    assert_eq!(snapshot["progress"]["terminal"], 1);
    let refreshed = std::fs::read_to_string(root.join("done_child").join("transcript.md")).unwrap();
    assert!(refreshed.contains("Found the answer."));
    assert!(!refreshed.contains("stale local artifact"));
    let refreshed_final =
        std::fs::read_to_string(root.join("done_child").join("final_message.md")).unwrap();
    assert!(refreshed_final.contains("Found the answer."));
    assert!(!refreshed_final.contains("stale local final message"));

    env.cleanup().await;
}

#[tokio::test]
async fn delegation_list_treats_empty_active_branch_as_terminal_non_failed() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "delegation empty child list test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation(
            "parent",
            DelegationKind::ReadonlyFanout,
            None,
            Some("empty"),
            2,
        )
        .await
        .expect("create delegation");

    create_empty_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "empty_child",
        "reviewer",
        SubagentType::ReadOnly,
    )
    .await;
    create_terminal_subagent(
        &env,
        project_id,
        "parent",
        &delegation.id,
        "failed_child",
        "reviewer",
        SubagentType::ReadOnly,
        TurnOutcome::Crashed,
        "Failed with durable evidence.",
    )
    .await;

    let list = rpc_list(&env.state, json!({ "parent_session_id": "parent" }))
        .await
        .expect("list delegations");
    let listed = list["delegations"]
        .as_array()
        .expect("delegations array")
        .iter()
        .find(|row| row["delegation_id"] == delegation.id)
        .expect("listed delegation");
    assert_eq!(listed["progress"]["expected"], 2);
    assert_eq!(listed["progress"]["spawned"], 2);
    assert_eq!(listed["progress"]["terminal"], 2);
    assert_eq!(listed["progress"]["running"], 0);
    assert_eq!(listed["progress"]["failed"], 1);

    let empty = listed["subagents"]
        .as_array()
        .expect("subagents")
        .iter()
        .find(|subagent| subagent["id"] == "empty_child")
        .expect("empty child");
    assert_eq!(empty["status"], "done");
    assert_eq!(empty["activity"], "idle");
    assert_eq!(empty["outcome"], serde_json::Value::Null);
    assert_eq!(empty["final_message_file"], serde_json::Value::Null);
    assert_eq!(empty["transcript_file"], serde_json::Value::Null);
    assert_list_subagent_has_only_compact_fields(empty);

    let failed = listed["subagents"]
        .as_array()
        .expect("subagents")
        .iter()
        .find(|subagent| subagent["id"] == "failed_child")
        .expect("failed child");
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["activity"], "idle");
    assert_list_subagent_has_only_compact_fields(failed);

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
        .create_project(
            project_id,
            "runtime-test",
            "failed inspect test",
            &[],
            json!({}),
        )
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
    assert_eq!(subagent["final_message_file"], serde_json::Value::Null);
    assert_eq!(subagent["transcript_file"], serde_json::Value::Null);
    assert!(subagent.get("final_message_path").is_none());
    assert!(subagent.get("transcript_path").is_none());
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
        .create_project(
            project_id,
            "runtime-test",
            "completion cancel race test",
            &[],
            json!({}),
        )
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
    let (won_cancel, events) = env
        .state
        .repo
        .cancel_running_delegation_and_queued_partials(
            "parent",
            &delegation.id,
            &delegation.attempt_id,
            "test cancellation wins",
        )
        .await
        .expect("cancellation wins");
    assert!(won_cancel);
    assert!(events.is_empty());
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
    assert_eq!(
        wakeup_observations_to_parent(&env, "parent", &delegation.id).await,
        0
    );

    env.cleanup().await;
}

#[tokio::test]
async fn missing_task_metadata_omits_task_prompt_handoff_metadata() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "missing task prompt test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("legacy"), 1)
        .await
        .expect("create delegation");
    create_busy_full_subagent(&env, project_id, "parent", &delegation.id, "impl_legacy").await;

    let list = rpc_list(&env.state, json!({ "parent_session_id": "parent" }))
        .await
        .expect("list delegations");
    let listed_subagent = &list["delegations"].as_array().unwrap()[0]["subagents"]
        .as_array()
        .unwrap()[0];
    assert_eq!(listed_subagent["task_prompt_file"], serde_json::Value::Null);
    assert!(listed_subagent.get("task_prompt_path").is_none());

    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    let subagent = &snapshot["subagents"].as_array().unwrap()[0];
    assert_eq!(subagent["task_prompt_file"], serde_json::Value::Null);
    assert!(subagent.get("task_prompt_path").is_none());
    assert!(
        !handoff_root(&env, &delegation.id)
            .join("impl_legacy")
            .join("task_prompt.md")
            .exists(),
        "missing task metadata must not create an empty task_prompt.md"
    );

    let error = read_handoff_file_core(
        &env.state,
        "parent",
        json!({
            "delegation_id": delegation.id,
            "subagent_id": "impl_legacy",
            "file": "task_prompt.md",
        }),
    )
    .await
    .expect_err("missing task prompt read rejected");
    assert_eq!(error.code, "handoff_file_not_found");

    env.cleanup().await;
}

#[tokio::test]
async fn read_task_prompt_validates_subagent_segment_before_refreshing_artifact() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "task prompt path validation test",
            &[],
            json!({}),
        )
        .await
        .expect("create project");
    create_parent(&env, project_id, "parent").await;
    let delegation = env
        .state
        .repo
        .create_delegation("parent", DelegationKind::Full, None, Some("impl"), 1)
        .await
        .expect("create delegation");
    create_busy_full_subagent(&env, project_id, "parent", &delegation.id, "impl").await;
    let metadata_with_task = json!({
        "created_by": "test",
        "role_name": "implementer",
        "task": "write the follow-up fix",
    });
    env.state
        .repo
        .update_session_metadata("impl", &metadata_with_task)
        .await
        .expect("store task metadata");

    let error = read_handoff_file_core(
        &env.state,
        "parent",
        json!({
            "delegation_id": delegation.id,
            "subagent_id": "../impl",
            "file": "task_prompt.md",
        }),
    )
    .await
    .expect_err("invalid path segment rejected before artifact refresh");
    assert_eq!(error.code, "invalid_params");
    assert!(
        !handoff_root(&env, &delegation.id)
            .join("impl")
            .join("task_prompt.md")
            .exists(),
        "invalid subagent_id must not refresh task_prompt.md"
    );

    env.cleanup().await;
}

#[tokio::test]
async fn out_of_set_outcome_is_recorded_verbatim() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "runtime-test", "barrier test", &[], json!({}))
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
        "Done.\noutcome: ship_it_immediately",
    )
    .await;

    complete_delegation_if_ready(&env.state, &delegation.id)
        .await
        .expect("barrier");
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    assert_eq!(snapshot["status"], "done");
    assert_eq!(snapshot["subagents"][0]["outcome"], "ship_it_immediately");

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
        .create_project(project_id, "runtime-test", "barrier test", &[], json!({}))
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
        "Done.",
    )
    .await;

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
        .create_project(project_id, "runtime-test", "barrier test", &[], json!({}))
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
    assert_eq!(
        wakeup_observations_to_parent(&env, "parent", &delegation.id).await,
        1
    );

    // A second sweep (another restart) must not double-publish a wakeup.
    sweep_running_delegations_on_boot(&env.state).await;
    assert_eq!(
        wakeup_observations_to_parent(&env, "parent", &delegation.id).await,
        1
    );

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
        .create_project(project_id, "runtime-test", "guard test", &[], json!({}))
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
        .create_project(project_id, "runtime-test", "guard test", &[], json!({}))
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
                runtime_id: "runtime-test".to_string(),
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

fn delegation_wakeup_client_input_id(delegation: &Delegation) -> String {
    format!(
        "delegation-steer:{}:{}",
        delegation.id, delegation.attempt_id
    )
}

/// Durable parent `input.queued` wakeup-observation events for a delegation.
///
/// This is intentionally based on the persistent event log rather than the
/// active queue: after the parent consumes the deterministic wakeup, the queue
/// row may no longer be active, but the event log still records whether that
/// client-input key was published exactly once.
async fn durable_parent_wakeup_observation_events(
    env: &TestEnv,
    parent_id: &str,
    delegation: &Delegation,
) -> Vec<serde_json::Value> {
    let client_input_id = delegation_wakeup_client_input_id(delegation);
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
                && event
                    .data
                    .get("client_input_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(client_input_id.as_str())
        })
        .map(|event| event.data)
        .collect()
}

async fn published_parent_completion_observation(
    env: &TestEnv,
    parent_id: &str,
    delegation: &Delegation,
) -> DaemonToolObservation {
    let transcript_observations =
        parent_completion_observations(env, parent_id, &delegation.id).await;
    if !transcript_observations.is_empty() {
        assert_eq!(
            transcript_observations.len(),
            1,
            "completion observation must be consumed at most once"
        );
        return transcript_observations[0].clone();
    }

    let client_input_id = delegation_wakeup_client_input_id(delegation);
    let queue = env
        .state
        .repo
        .queue_state(parent_id)
        .await
        .expect("parent queue");
    queue
        .queued_inputs
        .into_iter()
        .find_map(|input| {
            if input.client_input_id.as_deref() != Some(client_input_id.as_str()) {
                return None;
            }
            match input.content {
                QueuedInputContent::DaemonToolObservation(observation) => Some(observation),
                QueuedInputContent::UserMessage(_) => {
                    panic!("delegation wakeup must remain a typed daemon observation")
                }
                QueuedInputContent::SubagentControl => {
                    panic!("delegation wakeup must not be a child control marker")
                }
            }
        })
        .expect("completion observation is either consumed or durably queued")
}

fn assert_minimal_wakeup_event_payload(payload: &serde_json::Value, client_input_id: &str) {
    assert_eq!(
        payload.get("priority").and_then(serde_json::Value::as_str),
        Some(InputPriority::Steer.as_str())
    );
    assert_eq!(
        payload.get("status").and_then(serde_json::Value::as_str),
        Some(QueuedInputStatus::Queued.as_str())
    );
    assert_eq!(
        payload
            .get("client_input_id")
            .and_then(serde_json::Value::as_str),
        Some(client_input_id)
    );

    for field in [
        "content_type",
        "content",
        "editable",
        "summary",
        "tool_name",
        "delegation_id",
        "result_json",
        "result",
    ] {
        assert!(
            payload.get(field).is_none(),
            "typed daemon wakeup input.queued payload should not inline {field}: {payload}"
        );
    }

    let queued = payload["queued_inputs"]
        .as_array()
        .expect("queued inputs")
        .iter()
        .find(|input| {
            input
                .get("client_input_id")
                .and_then(serde_json::Value::as_str)
                == Some(client_input_id)
        })
        .expect("wakeup queue projection");
    assert_eq!(queued["content_type"], "daemon_tool_observation");
    assert_eq!(queued["content"], json!([]));
    assert_eq!(queued["editable"], false);
    assert!(queued.get("summary").is_none());
    assert!(queued.get("tool_name").is_none());
    assert!(queued.get("result_json").is_none());

    let payload_text = payload.to_string();
    assert!(!payload_text.contains("completed"));
    assert!(!payload_text.contains("implemented"));
    assert!(!payload_text.contains("inspect_delegation"));
    assert!(!payload_text.contains("subagents"));
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
        .create_project(project_id, "runtime-test", "barrier test", &[], json!({}))
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
    assert_eq!(
        wakeup_observations_to_parent(&env, "parent", &delegation.id).await,
        0
    );

    // The sibling arrives terminal too; now the full set exists -> one wakeup
    // observation.
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
    assert_eq!(
        wakeup_observations_to_parent(&env, "parent", &delegation.id).await,
        1
    );

    env.cleanup().await;
}

/// Simulate a crash after the finish_delegation status claim but before handoff
/// files / wakeup-observation publication. Boot repair must publish the files,
/// enqueue the deterministic daemon-authored observation, and remain idempotent
/// on later restarts.
#[tokio::test]
async fn boot_repair_publishes_handoff_and_wakeup_observation_after_finish_claim_crash() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "runtime-test", "barrier test", &[], json!({}))
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
    let key = delegation_wakeup_client_input_id(&delegation);
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
        .expect("find wakeup")
        .is_none());
    assert!(!handoff_root(&env, &delegation.id)
        .join("index.json")
        .exists());
    assert_eq!(
        durable_parent_wakeup_observation_events(&env, "parent", &delegation)
            .await
            .len(),
        0
    );

    sweep_running_delegations_on_boot(&env.state).await;
    assert!(env
        .state
        .repo
        .find_client_input("parent", &key)
        .await
        .expect("find repaired wakeup")
        .is_some());
    let root = handoff_root(&env, &delegation.id);
    assert!(!root.join("index.json").exists());
    assert!(root.join("impl").join("final_message.md").exists());
    assert!(root.join("impl").join("transcript.md").exists());
    let snapshot = inspect_delegation_snapshot(&env, &delegation.id).await;
    assert_eq!(snapshot["status"], "done");
    assert_eq!(
        wakeup_observations_to_parent(&env, "parent", &delegation.id).await,
        1
    );
    let wakeup_snapshot = parent_completion_snapshot(&env, "parent", &delegation.id).await;
    assert_eq!(wakeup_snapshot, snapshot);
    assert_eq!(
        wakeup_snapshot["subagents"][0]["transcript_file"],
        "impl/transcript.md"
    );
    let wakeup_events = durable_parent_wakeup_observation_events(&env, "parent", &delegation).await;
    assert_eq!(
        wakeup_events.len(),
        1,
        "first repair publishes exactly one durable completion observation"
    );
    assert!(env
        .state
        .repo
        .list_completed_delegations_for_repair()
        .await
        .expect("list remaining repairs")
        .is_empty());
    assert_minimal_wakeup_event_payload(&wakeup_events[0], &key);

    let repaired_input = env
        .state
        .repo
        .find_client_input("parent", &key)
        .await
        .expect("find repaired wakeup")
        .expect("repaired wakeup exists");
    assert!(matches!(
        repaired_input.status,
        QueuedInputStatus::Queued | QueuedInputStatus::Consuming | QueuedInputStatus::Consumed
    ));

    // A second repair sweep must not double-publish or double-drive. The first
    // repair may already have driven the idle parent and consumed the queued
    // input, so assert the deterministic idempotency row rather than requiring
    // the completion observation to remain in the active queue.
    sweep_running_delegations_on_boot(&env.state).await;
    let repaired_again = env
        .state
        .repo
        .find_client_input("parent", &key)
        .await
        .expect("find repaired wakeup after replay")
        .expect("repaired wakeup still exists");
    assert_eq!(
        repaired_again.input_id, repaired_input.input_id,
        "deterministic wakeup client id reuses the original row"
    );
    assert_eq!(
        wakeup_observations_to_parent(&env, "parent", &delegation.id).await,
        1
    );
    assert_eq!(
        durable_parent_wakeup_observation_events(&env, "parent", &delegation)
            .await
            .len(),
        1,
        "second repair must not publish any duplicate durable completion observation"
    );

    env.cleanup().await;
}

/// FIX C: a delegation subagent at a NON-boundary leaf (mid-turn) with its action
/// stale-marked (as the boot stale-mark does) and no queued input must NOT cause
/// the boot sweep to complete/wake the delegation.
#[tokio::test]
async fn boot_sweep_does_not_complete_mid_turn_subagent() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "runtime-test", "barrier test", &[], json!({}))
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
    assert_eq!(
        wakeup_observations_to_parent(&env, "parent", &delegation.id).await,
        0
    );

    env.cleanup().await;
}

/// FIX D: a terminal delegation member produces ZERO parent-visible `subagent.idle`
/// rows, yet the single delegation wakeup observation is still delivered (and
/// the once-gate fired).
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
        .create_project(project_id, "runtime-test", "barrier test", &[], json!({}))
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
    // ...yet the delegation completed and the single wakeup observation was
    // delivered.
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
    assert_eq!(
        wakeup_observations_to_parent(&env, "parent", &delegation.id).await,
        1
    );

    env.cleanup().await;
}

/// Server-side guard: raw `session.input` with `priority=steer` is not a
/// subagent-control surface.
/// Parents must use the scoped `steer_subagent` tool so the daemon can verify
/// parent/delegation membership and running state.
#[tokio::test]
async fn raw_session_input_steer_to_any_subagent_is_rejected_server_side() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "steer guard test",
            &[],
            json!({}),
        )
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

    let steer = |session_id: &str| {
        json!({
            "session_id": session_id,
            "priority": "steer",
            "content": [{ "type": "text", "text": "stop" }],
        })
    };

    // Raw steering the read-only subagent is rejected by the server guard.
    let rejected = crate::input_user(&env.state, steer("ro"))
        .await
        .expect_err("raw steering a read_only subagent must be rejected");
    assert_eq!(rejected.code, "subagent_steer_requires_parent_scope");

    // Raw steering a genuinely running full subagent is also rejected; use the
    // parent-scoped `steer_subagent` tool instead.
    let rejected = crate::input_user(&env.state, steer("full_running"))
        .await
        .expect_err("raw steering a running full subagent must be rejected");
    assert_eq!(rejected.code, "subagent_steer_requires_parent_scope");

    // A follow-up to the read-only subagent is unaffected by the raw steer guard.
    let follow_up = crate::input_user(
        &env.state,
        json!({
            "session_id": "ro",
            "priority": "follow_up",
            "content": [{ "type": "text", "text": "fyi" }],
        }),
    )
    .await
    .expect("a follow-up to a read_only subagent is allowed");
    let follow_up_input_id = follow_up["input_id"].as_str().expect("input id");
    let rejected = crate::input_promote_queued(
        &env.state,
        json!({ "session_id": "ro", "input_id": follow_up_input_id }),
    )
    .await
    .expect_err("raw promotion to subagent steer must be rejected");
    assert_eq!(rejected.code, "subagent_steer_requires_parent_scope");

    // A terminal full subagent must not be reactivated by a raw steer-priority
    // input either.
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
        .expect_err("raw steering a terminal full subagent must be rejected");
    assert_eq!(rejected.code, "subagent_steer_requires_parent_scope");

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
        .create_project(project_id, "runtime-test", "barrier test", &[], json!({}))
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

/// FIX F: two sibling delegation members reaching idle through the LIVE seam —
/// one triggering recovery of the other — wake the parent EXACTLY once, and
/// neither surfaces a per-child idle.
#[tokio::test]
async fn two_siblings_wake_parent_exactly_once_via_live_seam() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let project_id = Uuid::new_v4();
    env.state
        .repo
        .create_project(project_id, "runtime-test", "barrier test", &[], json!({}))
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
    assert_eq!(
        wakeup_observations_to_parent(&env, "parent", &delegation.id).await,
        1
    );

    env.cleanup().await;
}
