use std::sync::atomic::{AtomicU64, Ordering};

use agent_session::{SessionAction, SessionActionKind, SessionEvent, TranscriptStorageNode};
use agent_vocab::{
    AssistantItem, AssistantMessage, ProviderConfig, ProviderKind, ProviderReplayItem,
    ReasoningEffort, TranscriptItem, UserMessage,
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
