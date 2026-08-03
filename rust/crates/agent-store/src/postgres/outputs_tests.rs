use std::sync::atomic::{AtomicU64, Ordering};

use agent_session::{SessionAction, SessionActionKind, SessionEvent, TranscriptStorageNode};
use agent_vocab::{
    AssistantItem, AssistantMessage, ContentBlock, ProviderConfig, ProviderKind,
    ProviderReplayItem, ReasoningEffort, ToolCallId, ToolResultMessage, TranscriptItem,
    UserMessage,
};
use serde_json::json;

use super::*;
use crate::{AcceptedInput, InputPriority, QueuedInput, QueuedInputContent};

static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(70_000);

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

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn tool_action_result_persistence_admits_only_durable_or_sanctioned_results() {
    let Ok(admin_url) = std::env::var("PI_RELAY_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let name = format!(
        "pi_relay_tool_result_test_{}_{}",
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
    let store = PostgresAgentStore::connect(&database_url_with_name(&admin_url, &name))
        .await
        .expect("connect isolated test database");
    store
        .migrate()
        .await
        .expect("migrate isolated test database");
    let provider_config = serde_json::to_value(ProviderConfig {
        kind: ProviderKind::OpenAi,
        model: "test-model".to_string(),
        reasoning_effort: ReasoningEffort::Medium,
        max_tokens: None,
        prompt_cache: None,
    })
    .expect("serialize provider config");
    sqlx::query(
        r#"
        insert into sessions (
            id, runtime_id, workspace_id, workspaces, system_prompt, provider_config
        ) values ('session', 'runtime', 'workspace', '[]', 'test', $1)
        "#,
    )
    .bind(provider_config)
    .execute(&store.pool)
    .await
    .expect("insert session");

    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAACklEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg==";
    let artifact = store
        .put_inline_image("image/png", png)
        .await
        .expect("insert referenced image");
    let missing = format!("sha256:{}", "f".repeat(64));
    let canonical = serde_json::to_value(ToolResultMessage::success_content(
        ToolCallId::new("canonical-call"),
        "ReadImage",
        vec![ContentBlock::image(artifact.artifact_id.clone())],
    ))
    .unwrap();
    let cases = [
        (
            "canonical",
            canonical.clone(),
            ActionStatus::Completed,
            true,
        ),
        (
            "canonical-error-status",
            serde_json::to_value(ToolResultMessage::error(
                ToolCallId::new("canonical-error-call"),
                "Bash",
                "command failed",
            ))
            .unwrap(),
            ActionStatus::Error,
            true,
        ),
        (
            "completed-reason",
            json!({"reason":"session interrupted"}),
            ActionStatus::Completed,
            false,
        ),
        (
            "error-reason",
            json!({"reason":"session interrupted"}),
            ActionStatus::Error,
            false,
        ),
        (
            "completed-control",
            json!({"reason":"combined subagent control","control_input_id":"input-2"}),
            ActionStatus::Completed,
            false,
        ),
        (
            "error-control",
            json!({"reason":"combined subagent control","control_input_id":"input-3"}),
            ActionStatus::Error,
            false,
        ),
        (
            "completed-error",
            json!({"error":"failed before content"}),
            ActionStatus::Completed,
            false,
        ),
        (
            "interrupted-error",
            json!({"error":"failed before content"}),
            ActionStatus::Interrupted,
            false,
        ),
        (
            "interrupted-canonical",
            canonical,
            ActionStatus::Interrupted,
            false,
        ),
        (
            "completed-error-result",
            serde_json::to_value(ToolResultMessage::error(
                ToolCallId::new("completed-error-call"),
                "Bash",
                "wrong outcome",
            ))
            .unwrap(),
            ActionStatus::Completed,
            false,
        ),
        (
            "error-success-result",
            serde_json::to_value(ToolResultMessage::success(
                ToolCallId::new("error-success-call"),
                "Bash",
                "wrong outcome",
            ))
            .unwrap(),
            ActionStatus::Error,
            false,
        ),
        (
            "missing-ref",
            json!({
                "tool_call_id":"missing-call",
                "tool_name":"ReadImage",
                "content":[{"type":"image","artifact_id":missing}],
                "status":"Success"
            }),
            ActionStatus::Completed,
            false,
        ),
        (
            "text-with-inline-field",
            json!({
                "tool_call_id":"hybrid-text-call",
                "tool_name":"ReadImage",
                "content":[{
                    "type":"text",
                    "text":"ok",
                    "image":{"source":{"kind":"base64","mime_type":"image/png","data":png}}
                }],
                "status":"Success"
            }),
            ActionStatus::Completed,
            false,
        ),
        (
            "artifact-with-inline-field",
            json!({
                "tool_call_id":"hybrid-artifact-call",
                "tool_name":"ReadImage",
                "content":[{
                    "type":"image",
                    "artifact_id":artifact.artifact_id.as_str(),
                    "image":{"source":{"kind":"base64","mime_type":"image/png","data":png}}
                }],
                "status":"Success"
            }),
            ActionStatus::Completed,
            false,
        ),
        (
            "artifact-with-unknown-field",
            json!({
                "tool_call_id":"unknown-artifact-call",
                "tool_name":"ReadImage",
                "content":[{
                    "type":"image",
                    "artifact_id":artifact.artifact_id.as_str(),
                    "future_field":{"keep":false}
                }],
                "status":"Success"
            }),
            ActionStatus::Completed,
            false,
        ),
        (
            "legacy-inline-image",
            json!({
                "tool_call_id":"legacy-inline-call",
                "tool_name":"ReadImage",
                "content":[{
                    "type":"image",
                    "image":{"source":{"kind":"base64","mime_type":"image/png","data":png}}
                }],
                "status":"Success"
            }),
            ActionStatus::Completed,
            false,
        ),
        (
            "inline-image",
            json!({
                "tool_call_id":"inline-call",
                "tool_name":"ReadImage",
                "content":[{"type":"image","mime_type":"image/png","data":png}],
                "status":"Success"
            }),
            ActionStatus::Completed,
            false,
        ),
        (
            "malformed-content",
            json!({
                "tool_call_id":"malformed-call",
                "tool_name":"Bash",
                "content":{"type":"text","text":"not an array"},
                "status":"Success"
            }),
            ActionStatus::Completed,
            false,
        ),
        (
            "near-result",
            json!({"reason":"session interrupted","content":[]}),
            ActionStatus::Interrupted,
            false,
        ),
        (
            "interrupted",
            json!({"reason":"session interrupted"}),
            ActionStatus::Interrupted,
            true,
        ),
        (
            "control",
            json!({"reason":"combined subagent control","control_input_id":"input-1"}),
            ActionStatus::Interrupted,
            true,
        ),
        (
            "terminal-error",
            json!({"error":"failed before content"}),
            ActionStatus::Error,
            true,
        ),
    ];

    for (index, (id, result, status, accepted)) in cases.into_iter().enumerate() {
        let attempt_id = format!("attempt-{index}");
        sqlx::query(
            r#"
            insert into actions (
                id, session_id, action_id, attempt_id, kind, status, payload
            ) values ($1, 'session', $2, $3, 'tool', 'running', '{}')
            "#,
        )
        .bind(id)
        .bind(index as i64 + 1)
        .bind(&attempt_id)
        .execute(&store.pool)
        .await
        .expect("insert running action");
        let update = ActionUpdate {
            row_id: id.to_string(),
            attempt_id,
            post_compaction_dispatch_lease: None,
            status,
            result: result.clone(),
        };
        let persisted = store
            .persist_outputs(
                "session",
                OutputBatch::new(&[], None, &[], &[])
                    .with_unchanged_active_leaf()
                    .with_action_update(Some(update)),
            )
            .await;
        assert_eq!(
            persisted.is_ok(),
            accepted,
            "{id} persistence result: {persisted:?}"
        );
        let row: (String, Option<serde_json::Value>) =
            sqlx::query_as("select status, result from actions where id=$1")
                .bind(id)
                .fetch_one(&store.pool)
                .await
                .expect("load action after persistence attempt");
        if accepted {
            assert_eq!(row.1, Some(result), "{id} result changed");
        } else {
            assert_eq!(
                row,
                ("running".to_string(), None),
                "{id} was partly written"
            );
        }
    }

    store.close().await;
    let admin = sqlx::PgPool::connect(&admin_url)
        .await
        .expect("reconnect test administrator database");
    sqlx::query(&format!(r#"drop database "{name}""#))
        .execute(&admin)
        .await
        .expect("drop isolated test database");
    admin.close().await;
}

#[test]
fn tool_action_result_classifier_is_exact_and_fail_closed() {
    let canonical = ToolResultMessage::success(ToolCallId::new("call"), "Bash", "ok");
    let canonical_value = serde_json::to_value(canonical.clone()).unwrap();
    assert_eq!(
        classify_tool_action_result(ActionStatus::Completed, &canonical_value).unwrap(),
        Some((canonical, canonical_value.clone()))
    );
    for (status, value) in [
        (
            ActionStatus::Interrupted,
            json!({"reason":"session interrupted"}),
        ),
        (
            ActionStatus::Interrupted,
            json!({"reason":"combined subagent control","control_input_id":"input-1"}),
        ),
        (
            ActionStatus::Error,
            json!({"error":"failed before content"}),
        ),
    ] {
        assert_eq!(classify_tool_action_result(status, &value).unwrap(), None);
    }
    for (status, value) in [
        (
            ActionStatus::Interrupted,
            json!({"reason":"session interrupted","content":[]}),
        ),
        (
            ActionStatus::Completed,
            json!({"tool_call_id":"call","tool_name":"Bash","content":"not an array","status":"Success"}),
        ),
        (
            ActionStatus::Completed,
            json!({
                "tool_call_id":"call",
                "tool_name":"ReadImage",
                "content":[{"type":"image","mime_type":"image/png","data":"inline"}],
                "status":"Success"
            }),
        ),
        (
            ActionStatus::Completed,
            json!({
                "tool_call_id":"call",
                "tool_name":"Bash",
                "content":[{"type":"text","text":"ok","inline":"forbidden"}],
                "status":"Success"
            }),
        ),
        (ActionStatus::Completed, json!({"completed":true})),
        (
            ActionStatus::Completed,
            json!({"reason":"session interrupted"}),
        ),
        (ActionStatus::Error, json!({"reason":"session interrupted"})),
        (
            ActionStatus::Completed,
            json!({"reason":"control","control_input_id":"input-2"}),
        ),
        (
            ActionStatus::Error,
            json!({"reason":"control","control_input_id":"input-3"}),
        ),
        (
            ActionStatus::Completed,
            json!({"error":"failed before content"}),
        ),
        (
            ActionStatus::Interrupted,
            json!({"error":"failed before content"}),
        ),
        (ActionStatus::Interrupted, canonical_value),
        (
            ActionStatus::Completed,
            serde_json::to_value(ToolResultMessage::error(
                ToolCallId::new("call-error"),
                "Bash",
                "wrong",
            ))
            .unwrap(),
        ),
        (
            ActionStatus::Error,
            serde_json::to_value(ToolResultMessage::success(
                ToolCallId::new("call-success"),
                "Bash",
                "wrong",
            ))
            .unwrap(),
        ),
    ] {
        assert!(classify_tool_action_result(status, &value).is_err());
    }
}

fn transcript_entry() -> TranscriptStorageNode {
    TranscriptStorageNode {
        id: "entry".to_string(),
        parent_id: None,
        timestamp_ms: 1,
        item: TranscriptItem::UserMessage(UserMessage::text("hello")),
        provider_replay: Vec::new(),
    }
}

fn transcript_entry_with_provider_replay() -> TranscriptStorageNode {
    TranscriptStorageNode {
        id: "assistant-entry".to_string(),
        parent_id: None,
        timestamp_ms: 1,
        item: TranscriptItem::AssistantMessage(AssistantMessage {
            items: vec![AssistantItem::Text("hello".to_string())],
        }),
        provider_replay: vec![ProviderReplayItem::new(
            ProviderKind::OpenAi,
            &json!({ "type": "message" }),
        )
        .expect("provider replay serializes")],
    }
}

fn action_update() -> ActionUpdate {
    ActionUpdate {
        row_id: "action".to_string(),
        attempt_id: "attempt".to_string(),
        post_compaction_dispatch_lease: None,
        status: ActionStatus::Completed,
        result: json!({}),
    }
}

fn consumed_input() -> QueuedInput {
    QueuedInput {
        id: "input".to_string(),
        priority: InputPriority::FollowUp,
        content: QueuedInputContent::user_message(UserMessage::text("hello")),
        route: ProviderConfig {
            kind: ProviderKind::OpenAi,
            model: "test-model".to_string(),
            reasoning_effort: ReasoningEffort::Medium,
            max_tokens: None,
            prompt_cache: None,
        }
        .into(),
        client_input_id: None,
        claim_id: "claim".to_string(),
        row_version: "1".to_string(),
    }
}

fn accepted_input() -> AcceptedInput {
    AcceptedInput {
        priority: InputPriority::FollowUp,
        content: UserMessage::text("hello"),
        client_input_id: None,
    }
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn only_a_batch_with_no_durable_obligations_skips_the_transaction() {
    let Ok(admin_url) = std::env::var("PI_RELAY_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let name = format!(
        "pi_relay_outputs_test_{}_{}",
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
    let store = PostgresAgentStore::connect(&database_url_with_name(&admin_url, &name))
        .await
        .expect("connect isolated test database");
    store
        .migrate()
        .await
        .expect("migrate isolated test database");

    let empty = store
        .persist_outputs(
            "session",
            OutputBatch::new(&[], None, &[], &[]).with_unchanged_active_leaf(),
        )
        .await
        .expect("empty batch must not acquire a connection");
    assert!(empty.0.is_empty());
    assert!(empty.1.is_empty());

    let entry = transcript_entry();
    let entries = [entry];
    let replay_entry = transcript_entry_with_provider_replay();
    let replay_entries = [replay_entry];
    let event = SessionEvent::ActionCompleted {
        kind: SessionActionKind::Model,
        id: "1".to_string(),
    };
    let events = [event];
    let action = SessionAction::CancelSessionWork;
    let actions = [action];
    let obligations = [
        (
            "transcript entry",
            OutputBatch::new(&entries, Some("entry"), &[], &[]).with_unchanged_active_leaf(),
        ),
        (
            "active leaf change",
            OutputBatch::new(&[], Some("entry"), &[], &[]),
        ),
        ("active leaf cleared", OutputBatch::new(&[], None, &[], &[])),
        (
            "session event / activity transition",
            OutputBatch::new(&[], None, &events, &[]).with_unchanged_active_leaf(),
        ),
        (
            "provider route with action",
            OutputBatch::new(&[], None, &[], &actions)
                .with_unchanged_active_leaf()
                .with_provider_route(
                    ProviderConfig {
                        kind: ProviderKind::OpenAi,
                        model: "test-model".to_string(),
                        reasoning_effort: ReasoningEffort::High,
                        max_tokens: None,
                        prompt_cache: None,
                    }
                    .into(),
                ),
        ),
        (
            "action update / compaction completion",
            OutputBatch::new(&[], None, &[], &[])
                .with_unchanged_active_leaf()
                .with_action_update(Some(action_update())),
        ),
        (
            "consumed input",
            OutputBatch::new(&[], None, &[], &[])
                .with_unchanged_active_leaf()
                .with_consumed_input(Some(consumed_input())),
        ),
        (
            "provider route with accepted input",
            OutputBatch::new(&[], None, &[], &[])
                .with_unchanged_active_leaf()
                .with_accepted_input(Some(accepted_input()))
                .with_provider_route(
                    ProviderConfig {
                        kind: ProviderKind::OpenAi,
                        model: "test-model".to_string(),
                        reasoning_effort: ReasoningEffort::High,
                        max_tokens: None,
                        prompt_cache: None,
                    }
                    .into(),
                ),
        ),
        (
            "transcript entry with provider replay attachment",
            OutputBatch::new(&replay_entries, None, &[], &[]).with_unchanged_active_leaf(),
        ),
        (
            "selected-subagent control transition",
            OutputBatch::new(&[], None, &[], &[])
                .with_unchanged_active_leaf()
                .with_control_interrupt("input"),
        ),
    ];

    for (name, batch) in obligations {
        let error = store
            .persist_outputs("session", batch)
            .await
            .expect_err(name);
        assert!(
            error.to_string().contains("session not found"),
            "{name} did not reach durable session persistence: {error:#}"
        );
    }

    store.close().await;
    let route_only = store
        .persist_outputs(
            "session",
            OutputBatch::new(&[], None, &[], &[])
                .with_unchanged_active_leaf()
                .with_provider_route(
                    ProviderConfig {
                        kind: ProviderKind::OpenAi,
                        model: "test-model".to_string(),
                        reasoning_effort: ReasoningEffort::High,
                        max_tokens: None,
                        prompt_cache: None,
                    }
                    .into(),
                ),
        )
        .await
        .expect("provider route alone must not touch the closed pool");
    assert!(route_only.0.is_empty());
    assert!(route_only.1.is_empty());
    let admin = sqlx::PgPool::connect(&admin_url)
        .await
        .expect("connect test database admin for cleanup");
    sqlx::query(&format!(r#"drop database if exists "{name}""#))
        .execute(&admin)
        .await
        .expect("drop isolated test database");
    admin.close().await;
}
