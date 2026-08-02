use std::sync::atomic::{AtomicU64, Ordering};

use agent_session::{AgentSession, TranscriptStorageNode};
use agent_vocab::{
    AssistantItem, AssistantMessage, CompactionSummary, ProviderConfig, ProviderKind,
    ProviderReplayItem, ReasoningEffort, ToolCall, ToolCallId, TranscriptItem, TurnId, TurnOutcome,
    UserMessage,
};
use serde_json::json;
use uuid::Uuid;

use crate::{
    CreateContextForkRequest, CreateForkRequest, DelegationKind, HistoryChanged, HistoryTarget,
    HistoryTargetNotTurnBoundary, OutputBatch, PostgresAgentStore, PreparedWorkspace,
    SessionConfig, SourceMutationConflict, SwitchActiveLeafRequest, TranscriptEntryBodyMode,
    WorkspaceOwnerKind,
};

static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(40_000);

struct TestDb {
    store: PostgresAgentStore,
    admin_url: String,
    name: String,
}

async fn prepare_test_workspace(
    store: &PostgresAgentStore,
    owner_session_id: &str,
    config: &SessionConfig,
    owner_kind: WorkspaceOwnerKind,
) -> PreparedWorkspace {
    let generation = format!("test-generation-{}", Uuid::new_v4());
    store
        .begin_workspace_provisioning(
            owner_session_id,
            &config.runtime_id,
            &config.workspace_id,
            &generation,
            owner_kind,
            300,
        )
        .await
        .expect("workspace provisioning begins");
    store
        .finish_workspace_provisioning(
            owner_session_id,
            &config.runtime_id,
            &config.workspace_id,
            &generation,
            &config.workspaces,
        )
        .await
        .expect("workspace provisioning finishes")
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn context_fork_excludes_in_progress_parent_delegation_tool_call() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "completed context boundary test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    let parent_id = "context-open-delegation-parent";
    let config = create_session(store, project_id, parent_id, true).await;
    let delegation_call = ToolCall {
        id: ToolCallId::new("call_delegate_in_progress"),
        tool_name: "delegate_readonly_tasks".to_string(),
        args_json: r#"{"tasks":[{"role":"reviewer","prompt":"inspect"}]}"#.to_string(),
    };
    store
        .persist_outputs(
            parent_id,
            OutputBatch::new(
                &[
                    entry(
                        "open-start",
                        Some("finish"),
                        TranscriptItem::TurnStarted { turn_id: TurnId(2) },
                    ),
                    entry(
                        "open-user",
                        Some("open-start"),
                        TranscriptItem::UserMessage(UserMessage::text("delegate this")),
                    ),
                    entry(
                        "open-assistant",
                        Some("open-user"),
                        TranscriptItem::AssistantMessage(AssistantMessage {
                            items: vec![AssistantItem::ToolCall(delegation_call)],
                        }),
                    ),
                ],
                Some("open-assistant"),
                &[],
                &[],
            ),
        )
        .await
        .expect("open delegation turn persists");

    let task = UserMessage::text("inspect the completed conversation");
    let result = store
        .create_context_fork(CreateContextForkRequest {
            child_session_id: "context-open-delegation-child",
            config: &config,
            parent_session_id: parent_id,
            subagent_type: crate::SubagentType::Full,
            delegation_id: None,
            task: &task,
            workspace: None,
        })
        .await
        .expect("context fork commits");

    assert_eq!(result.active_leaf_id.as_deref(), Some("finish"));
    let stored = store
        .load_stored_session("context-open-delegation-child")
        .await
        .expect("child loads");
    assert_eq!(stored.active_leaf_id.as_deref(), Some("finish"));
    assert!(stored
        .entries
        .iter()
        .all(|entry| !entry.id.starts_with("open-")));
    let child = AgentSession::from_stored_session(stored).expect("child history rehydrates");
    assert!(child
        .model_context()
        .transcript_items()
        .iter()
        .all(|item| !matches!(
            item,
            TranscriptItem::AssistantMessage(message)
                if message
                    .tool_calls()
                    .any(|call| call.tool_name.starts_with("delegate_"))
        )));
    let queue = store
        .queue_state("context-open-delegation-child")
        .await
        .expect("delegated task queued");
    assert_eq!(queue.queued_inputs.len(), 1);
    db.cleanup().await;
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn private_workspace_cleanup_routes_root_full_and_read_only_sessions() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "workspace routing test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");

    let mut root_config = session_config(project_id);
    root_config.workspace_id = "workspace-root-private".to_string();
    let root_workspace = prepare_test_workspace(
        store,
        "workspace-root-session",
        &root_config,
        WorkspaceOwnerKind::Root,
    )
    .await;
    store
        .start_session_outputs_with_parent_and_workspace(
            "workspace-root-session",
            &root_config,
            &[],
            None,
            &[],
            &[],
            crate::InputPriority::FollowUp,
            &UserMessage::text("root"),
            None,
            None,
            None,
            None,
            Some(root_workspace.attachment()),
        )
        .await
        .expect("root session attaches");

    let mut full_config = root_config.clone();
    full_config.metadata = json!({ "delegation_spawn_index": 0 });
    store
        .start_session_outputs_with_parent(
            "workspace-full-child",
            &full_config,
            &[],
            None,
            &[],
            &[],
            crate::InputPriority::FollowUp,
            &UserMessage::text("full"),
            None,
            Some("workspace-root-session"),
            Some(crate::SubagentType::Full),
            None,
        )
        .await
        .expect("full child creates");
    assert!(!store
        .request_session_workspace_cleanup(
            "workspace-full-child",
            crate::WorkspaceCleanupMode::DeleteSession,
        )
        .await
        .expect("full cleanup routes"));
    assert!(store
        .delete_session("workspace-full-child")
        .await
        .expect("full identity deletes"));
    assert!(store
        .session_exists("workspace-root-session")
        .await
        .expect("root remains"));

    let mut read_only_config = root_config.clone();
    read_only_config.workspace_id = "workspace-read-only-private".to_string();
    let read_only_workspace = prepare_test_workspace(
        store,
        "workspace-read-only-child",
        &read_only_config,
        WorkspaceOwnerKind::ReadOnly,
    )
    .await;
    store
        .start_session_outputs_with_parent_and_workspace(
            "workspace-read-only-child",
            &read_only_config,
            &[],
            None,
            &[],
            &[],
            crate::InputPriority::FollowUp,
            &UserMessage::text("readonly"),
            None,
            Some("workspace-root-session"),
            Some(crate::SubagentType::ReadOnly),
            None,
            Some(read_only_workspace.attachment()),
        )
        .await
        .expect("read-only child attaches");
    assert!(store
        .request_session_workspace_cleanup(
            "workspace-read-only-child",
            crate::WorkspaceCleanupMode::RetainSession,
        )
        .await
        .expect("read-only cleanup intent persists"));
    let read_only_claim = store
        .claim_due_workspace_deletions(None)
        .await
        .expect("read-only cleanup claims")
        .into_iter()
        .find(|resource| resource.owner_session_id == "workspace-read-only-child")
        .expect("read-only resource claimed");
    assert!(store
        .complete_workspace_cleanup(&read_only_claim)
        .await
        .expect("read-only cleanup completes"));
    assert!(store
        .session_exists("workspace-read-only-child")
        .await
        .expect("read-only transcript retained"));
    assert_eq!(
        store
            .workspace_resource_for_session("workspace-read-only-child")
            .await
            .expect("read-only tombstone loads")
            .expect("read-only tombstone exists")
            .state,
        crate::WorkspaceResourceState::Deleted
    );
    assert!(store
        .request_session_workspace_cleanup(
            "workspace-read-only-child",
            crate::WorkspaceCleanupMode::DeleteSession,
        )
        .await
        .expect("retained read-only identity deletes"));
    assert!(!store
        .session_exists("workspace-read-only-child")
        .await
        .expect("read-only identity gone"));

    assert!(store
        .request_session_workspace_cleanup(
            "workspace-root-session",
            crate::WorkspaceCleanupMode::DeleteSession,
        )
        .await
        .expect("root cleanup intent persists"));
    let root_claim = store
        .claim_due_workspace_deletions(None)
        .await
        .expect("root cleanup claims")
        .into_iter()
        .find(|resource| resource.owner_session_id == "workspace-root-session")
        .expect("root resource claimed");
    assert!(store
        .complete_workspace_cleanup(&root_claim)
        .await
        .expect("root cleanup completes"));
    assert!(!store
        .session_exists("workspace-root-session")
        .await
        .expect("root identity gone"));
    db.cleanup().await;
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn deleting_offline_session_rejects_later_input_before_reconnect_cleanup() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "offline deletion admission test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    let session_id = "offline-deleting-session";
    let client_input_id = "input-after-delete";
    let mut config = session_config(project_id);
    config.workspace_id = "workspace-offline-deleting".to_string();
    let workspace =
        prepare_test_workspace(store, session_id, &config, WorkspaceOwnerKind::Root).await;
    store
        .start_session_outputs_with_parent_and_workspace(
            session_id,
            &config,
            &[],
            None,
            &[],
            &[],
            crate::InputPriority::FollowUp,
            &UserMessage::text("initial input"),
            None,
            None,
            None,
            None,
            Some(workspace.attachment()),
        )
        .await
        .expect("session attaches while runtime is online");

    assert!(store
        .request_session_workspace_cleanup(session_id, crate::WorkspaceCleanupMode::DeleteSession)
        .await
        .expect("logical deletion persists while runtime is offline"));
    let error = match store
        .enqueue_user_input(
            session_id,
            crate::InputPriority::FollowUp,
            &UserMessage::text("must not be accepted"),
            Some(client_input_id),
            None,
        )
        .await
    {
        Ok(_) => panic!("deleting owner accepted later input"),
        Err(error) => error,
    };
    assert!(error.downcast_ref::<crate::SessionDeleting>().is_some());
    assert!(store
        .find_client_input(session_id, client_input_id)
        .await
        .expect("client input lookup succeeds")
        .is_none());

    let reconnect_claim = store
        .claim_due_workspace_deletions(Some(&config.runtime_id))
        .await
        .expect("runtime reconnect claims pending cleanup")
        .into_iter()
        .find(|resource| resource.owner_session_id == session_id)
        .expect("offline cleanup remains pending for reconnect");
    assert!(store
        .complete_workspace_cleanup(&reconnect_claim)
        .await
        .expect("reconnect cleanup completes"));
    assert!(!store
        .session_exists(session_id)
        .await
        .expect("deleted identity stays gone"));
    db.cleanup().await;
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn root_cleanup_waits_for_full_and_offline_read_only_descendants() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "workspace descendant cleanup test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");

    let root_id = "cleanup-tree-root";
    let full_id = "cleanup-tree-full";
    let read_only_id = "cleanup-tree-read-only";
    let mut root_config = session_config(project_id);
    root_config.workspace_id = "workspace-cleanup-tree-root".to_string();
    let root_workspace =
        prepare_test_workspace(store, root_id, &root_config, WorkspaceOwnerKind::Root).await;
    store
        .start_session_outputs_with_parent_and_workspace(
            root_id,
            &root_config,
            &[],
            None,
            &[],
            &[],
            crate::InputPriority::FollowUp,
            &UserMessage::text("root"),
            None,
            None,
            None,
            None,
            Some(root_workspace.attachment()),
        )
        .await
        .expect("root session attaches");

    let full_delegation = store
        .create_delegation_idempotent(crate::CreateDelegationRequest {
            parent_session_id: root_id,
            launch_key: "cleanup-tree-full-launch",
            launch_shape: r#"{"kind":"full","role":"implementer","prompt":"work"}"#,
            kind: DelegationKind::Full,
            workflow: None,
            label: None,
            expected_subagents: 1,
        })
        .await
        .expect("full delegation creates");
    let mut full_config = root_config.clone();
    full_config.metadata = json!({ "delegation_spawn_index": 0 });
    store
        .start_session_outputs_with_parent(
            full_id,
            &full_config,
            &[],
            None,
            &[],
            &[],
            crate::InputPriority::FollowUp,
            &UserMessage::text("full task"),
            None,
            Some(root_id),
            Some(crate::SubagentType::Full),
            Some(&full_delegation.id),
        )
        .await
        .expect("full child creates");

    let read_only_delegation = store
        .create_delegation_idempotent(crate::CreateDelegationRequest {
            parent_session_id: root_id,
            launch_key: "cleanup-tree-read-only-launch",
            launch_shape:
                r#"{"kind":"readonly_fanout","tasks":[{"role":"reviewer","prompt":"inspect"}]}"#,
            kind: DelegationKind::ReadonlyFanout,
            workflow: None,
            label: None,
            expected_subagents: 1,
        })
        .await
        .expect("read-only delegation creates");
    let mut read_only_config = root_config.clone();
    read_only_config.workspace_id = "workspace-cleanup-tree-read-only".to_string();
    read_only_config.metadata = json!({ "delegation_spawn_index": 0 });
    let read_only_workspace = prepare_test_workspace(
        store,
        read_only_id,
        &read_only_config,
        WorkspaceOwnerKind::ReadOnly,
    )
    .await;
    store
        .start_session_outputs_with_parent_and_workspace(
            read_only_id,
            &read_only_config,
            &[],
            None,
            &[],
            &[],
            crate::InputPriority::FollowUp,
            &UserMessage::text("read-only task"),
            None,
            Some(root_id),
            Some(crate::SubagentType::ReadOnly),
            Some(&read_only_delegation.id),
            Some(read_only_workspace.attachment()),
        )
        .await
        .expect("read-only child attaches");

    assert!(
        store
            .request_session_workspace_cleanup(
                read_only_id,
                crate::WorkspaceCleanupMode::DeleteSession,
            )
            .await
            .expect("read-only cleanup becomes pending")
    );
    assert!(store
        .request_session_workspace_cleanup(root_id, crate::WorkspaceCleanupMode::DeleteSession)
        .await
        .expect("root cleanup becomes pending"));

    let child_claim = store
        .claim_due_workspace_deletions(None)
        .await
        .expect("eligible child cleanup claims")
        .into_iter()
        .find(|resource| resource.owner_session_id == read_only_id)
        .expect("read-only child claims before root");
    store
        .record_workspace_cleanup_failure(&child_claim, "runtime offline")
        .await
        .expect("offline child remains pending");
    let root_resource = store
        .workspace_resource_for_session(root_id)
        .await
        .expect("root resource loads")
        .expect("root resource exists");
    assert!(!store
        .complete_workspace_cleanup(&root_resource)
        .await
        .expect("root finalization is durably blocked by descendants"));
    assert!(store
        .claim_due_workspace_deletions(None)
        .await
        .expect("blocked root does not claim")
        .is_empty());

    assert!(store
        .delete_session(full_id)
        .await
        .expect("unrelated full child identity progresses independently"));
    assert!(
        store
            .request_session_workspace_cleanup(
                read_only_id,
                crate::WorkspaceCleanupMode::DeleteSession,
            )
            .await
            .expect("reconnect makes offline child immediately retryable")
    );
    let retried_child = store
        .claim_due_workspace_deletions(None)
        .await
        .expect("retried child cleanup claims")
        .into_iter()
        .find(|resource| resource.owner_session_id == read_only_id)
        .expect("read-only child reclaims");
    assert!(store
        .complete_workspace_cleanup(&retried_child)
        .await
        .expect("read-only child cleanup completes"));

    let root_claim = store
        .claim_due_workspace_deletions(None)
        .await
        .expect("root becomes eligible after descendants disappear")
        .into_iter()
        .find(|resource| resource.owner_session_id == root_id)
        .expect("root cleanup claims last");
    assert!(store
        .complete_workspace_cleanup(&root_claim)
        .await
        .expect("root cleanup completes"));
    assert!(!store
        .session_exists(root_id)
        .await
        .expect("root identity is gone"));
    db.cleanup().await;
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn workspace_reconciliation_adopts_exact_owner_and_retries_offline_cleanup() {
    let Some(db) = test_store().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let project_id = Uuid::new_v4();
    store
        .create_project(
            project_id,
            "workspace recovery test",
            "runtime-test",
            &[],
            json!({}),
        )
        .await
        .expect("project creates");
    let mut config = session_config(project_id);
    config.workspace_id = "workspace-recovery-private".to_string();
    let prepared = prepare_test_workspace(
        store,
        "workspace-recovery-session",
        &config,
        WorkspaceOwnerKind::Root,
    )
    .await;
    // Simulate a restart between session commit and attachment. Reconciliation
    // adopts the exact identity immediately, without waiting for the lease or a
    // second runtime Hello.
    store
        .create_session("workspace-recovery-session", &config)
        .await
        .expect("session commits without attachment");
    store
        .reconcile_workspace_resources()
        .await
        .expect("exact owner adopts");
    let adopted = store
        .workspace_resource_for_session("workspace-recovery-session")
        .await
        .expect("resource loads")
        .expect("resource exists");
    assert_eq!(adopted.state, crate::WorkspaceResourceState::Ready);

    assert!(store
        .request_workspace_cleanup_exact(
            &prepared.owner_session_id,
            &prepared.runtime_id,
            &prepared.workspace_id,
            &prepared.generation,
            crate::WorkspaceCleanupMode::DeleteSession,
        )
        .await
        .expect("cleanup persists"));
    let claim = store
        .claim_due_workspace_deletions(None)
        .await
        .expect("cleanup claims")
        .into_iter()
        .find(|resource| resource.owner_session_id == "workspace-recovery-session")
        .expect("resource claimed");
    store
        .record_workspace_cleanup_failure(&claim, "runtime unavailable")
        .await
        .expect("offline failure records");
    let pending = store
        .workspace_resource_for_session("workspace-recovery-session")
        .await
        .expect("pending resource loads")
        .expect("pending resource exists");
    assert_eq!(pending.state, crate::WorkspaceResourceState::Deleting);
    assert!(store
        .session_exists("workspace-recovery-session")
        .await
        .expect("identity retained while offline"));
    assert!(store
        .claim_due_workspace_deletions(None)
        .await
        .expect("immediate duplicate claim fenced")
        .is_empty());

    store
        .begin_workspace_provisioning(
            "workspace-abandoned-session",
            &config.runtime_id,
            "workspace-abandoned-private",
            "workspace-abandoned-generation",
            WorkspaceOwnerKind::Root,
            1,
        )
        .await
        .expect("abandoned provisioning intent persists");
    store
        .reconcile_workspace_resources()
        .await
        .expect("quick restart reconciliation runs before lease expiry");
    assert_eq!(
        store
            .workspace_resource_for_session("workspace-abandoned-session")
            .await
            .expect("pre-expiry resource loads")
            .expect("pre-expiry resource exists")
            .state,
        crate::WorkspaceResourceState::Provisioning
    );
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    store
        .reconcile_workspace_resources()
        .await
        .expect("periodic reconciliation expires lease without another Hello");
    let abandoned = store
        .workspace_resource_for_session("workspace-abandoned-session")
        .await
        .expect("abandoned resource loads")
        .expect("abandoned resource exists");
    assert_eq!(abandoned.state, crate::WorkspaceResourceState::Deleting);
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
    let workspace = prepare_test_workspace(
        store,
        "fork-child",
        &child_config,
        WorkspaceOwnerKind::HistoryFork,
    )
    .await;

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
            workspace: workspace.attachment(),
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
    let workspace = prepare_test_workspace(
        store,
        child_session_id,
        config,
        WorkspaceOwnerKind::HistoryFork,
    )
    .await;
    store
        .create_fork(CreateForkRequest {
            source_session_id,
            child_session_id,
            config,
            target,
            workspace: workspace.attachment(),
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
        delegation_target,
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
