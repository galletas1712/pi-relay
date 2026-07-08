use agent_session::{SessionAction, SessionActionKind, SessionEvent, TranscriptStorageNode};
use agent_vocab::{TranscriptItem, UserMessage};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

use super::*;
use crate::{AcceptedInput, InputPriority, QueuedInput, QueuedInputContent};

fn transcript_entry() -> TranscriptStorageNode {
    TranscriptStorageNode {
        id: "entry".to_string(),
        parent_id: None,
        timestamp_ms: 1,
        item: TranscriptItem::UserMessage(UserMessage::text("hello")),
        provider_replay: Vec::new(),
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

#[tokio::test]
async fn only_a_batch_with_no_durable_obligations_skips_the_transaction() {
    let store = PostgresAgentStore {
        pool: PgPoolOptions::new()
            .connect_lazy("postgresql://localhost/closed")
            .expect("valid test URL"),
    };
    store.close().await;

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
            "action",
            OutputBatch::new(&[], None, &[], &actions).with_unchanged_active_leaf(),
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
            "accepted input",
            OutputBatch::new(&[], None, &[], &[])
                .with_unchanged_active_leaf()
                .with_accepted_input(Some(accepted_input())),
        ),
        (
            "provider replay attachment",
            OutputBatch::new(&[], None, &[], &[])
                .with_unchanged_active_leaf()
                .with_provider_replay_attachment(),
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
            error.to_string().contains("closed"),
            "{name} did not attempt to open a transaction: {error:#}"
        );
    }
}
