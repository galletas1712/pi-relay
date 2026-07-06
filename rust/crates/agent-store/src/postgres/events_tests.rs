use std::collections::HashSet;

use agent_session::{ModelContext, SessionAction, SessionActionKind, SessionEvent};
use agent_vocab::{
    ActionId, AssistantItem, AssistantMessage, ToolCall, ToolCallId, TranscriptItem, TurnId,
    TurnOutcome, UserMessage,
};
use serde_json::{json, Value};
use sqlx::Row;

use super::*;
use crate::postgres::transcript::tests::{create_session as create_test_session, test_store};
use crate::postgres::transcript::tree_node_from_entry;
use crate::{AcceptedInput, EventFrame, InputPriority, OutputBatch, TranscriptEntryRecord};

async fn create_session(store: &PostgresAgentStore, session_id: &str) {
    create_test_session(store, session_id).await;
    store.clear_session_events(session_id).await.unwrap();
}

fn assert_ordered(frames: &[EventFrame]) {
    assert!(frames
        .windows(2)
        .all(|frames| frames[0].event_id < frames[1].event_id));
}

#[test]
fn event_rows_inject_activity_once_without_changing_generic_payloads() {
    assert_eq!(
        EventRow::new(EventType::InputQueued, Value::Null).payload,
        Value::Null
    );
    assert_eq!(
        EventRow::with_activity_hint(EventType::InputConsumed, json!("scalar")).payload,
        json!({ "activity": "running" })
    );
    assert_eq!(
        EventRow::with_activity_hint(
            EventType::SessionIdle,
            json!({ "activity": "explicit", "value": 1 }),
        )
        .payload,
        json!({ "activity": "explicit", "value": 1 })
    );
}

#[test]
fn batch_sql_allocates_only_ids_from_one_ordered_non_recursive_input_scan() {
    let sql = INSERT_EVENT_ROWS_SQL
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    assert!(!sql.contains("recursive"));
    assert_eq!(sql.matches("nextval(").count(), 1);
    assert!(sql.contains(
        "allocated as materialized ( select nextval(pg_get_serial_sequence('events', 'id')::regclass) as id, input_ordinal from ( select input_ordinal from input order by input_ordinal ) ordered_input )"
    ));
    assert!(sql.contains(
        "insert into events (id, session_id, type, payload) select allocated.id, $1::text, input.event_type, input.payload from allocated join input using (input_ordinal)"
    ));
    assert!(
        sql.contains("from inserted join allocated using (id) order by allocated.input_ordinal")
    );
}

fn plan_nodes<'a>(plan: &'a Value, nodes: &mut Vec<&'a Value>) {
    nodes.push(plan);
    if let Some(children) = plan.get("Plans").and_then(Value::as_array) {
        for child in children {
            plan_nodes(child, nodes);
        }
    }
}

#[tokio::test]
async fn batch_plan_scans_large_input_linearly_and_allocates_ids_in_ordinal_order() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let session_id = "event-batch-plan";
    create_session(store, session_id).await;
    let event_types = vec![EventType::ModelCompleted.as_str(); EVENT_INSERT_BATCH_CAPACITY];
    let payloads = (1..=EVENT_INSERT_BATCH_CAPACITY)
        .map(|ordinal| json!({ "ordinal": ordinal, "large": "x".repeat(16 * 1024) }))
        .collect::<Vec<_>>();
    let explain_sql = format!(
        "explain (analyze, format json, costs off, timing off, summary off) {INSERT_EVENT_ROWS_SQL}"
    );
    let mut tx = store.pool.begin().await.expect("transaction starts");
    let row = sqlx::query(&explain_sql)
        .bind(session_id)
        .bind(event_types)
        .bind(payloads)
        .fetch_one(&mut *tx)
        .await
        .expect("batch insert explains");
    let explain = row.get::<Value, _>("QUERY PLAN");
    let plan = &explain[0]["Plan"];
    let mut nodes = Vec::new();
    plan_nodes(plan, &mut nodes);
    assert!(nodes
        .iter()
        .all(|node| node["Node Type"] != "Recursive Union"));
    let input_scans = nodes
        .iter()
        .filter(|node| node["CTE Name"] == "input")
        .collect::<Vec<_>>();
    assert_eq!(input_scans.len(), 2);
    assert!(input_scans
        .iter()
        .all(|node| node["Actual Loops"] == 1 && node["Actual Rows"] == 128));
    let allocated = nodes
        .iter()
        .find(|node| node["Subplan Name"] == "CTE allocated")
        .expect("allocated CTE is planned");
    assert_eq!(allocated["Actual Loops"], 1);
    assert_eq!(allocated["Actual Rows"], 128);
    let rows = sqlx::query(
        "select id, payload->>'ordinal' as ordinal from events where session_id=$1 order by (payload->>'ordinal')::bigint",
    )
    .bind(session_id)
    .fetch_all(&mut *tx)
    .await
    .expect("inserted event ordinals load");
    let ids = rows
        .iter()
        .map(|row| row.get::<i64, _>("id"))
        .collect::<Vec<_>>();
    assert!(ids.windows(2).all(|ids| ids[0] < ids[1]));
    assert_eq!(
        rows.into_iter()
            .map(|row| row.get::<String, _>("ordinal").parse::<i64>().unwrap())
            .collect::<Vec<_>>(),
        (1..=EVENT_INSERT_BATCH_CAPACITY as i64).collect::<Vec<_>>()
    );
    tx.rollback().await.expect("plan insert rolls back");

    db.cleanup().await;
}

#[tokio::test]
async fn bounded_insert_preserves_rows_transactions_replay_and_global_ids() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    for (case, (len, statements)) in [
        (0, 0),
        (1, 1),
        (EVENT_INSERT_BATCH_CAPACITY, 1),
        (EVENT_INSERT_BATCH_CAPACITY + 1, 2),
        (EVENT_INSERT_BATCH_CAPACITY * 2 + 1, 3),
    ]
    .into_iter()
    .enumerate()
    {
        let session_id = format!("event-boundary-{case}");
        create_session(store, &session_id).await;
        let events = vec![(EventType::ModelCompleted, json!({ "duplicate": true })); len];
        let (result, actual_statements) =
            with_event_insert_statement_count(store.insert_events(&session_id, events)).await;
        let frames = result.expect("boundary events insert");
        assert_eq!((frames.len(), actual_statements), (len, statements));
        assert_ordered(&frames);
        assert_eq!(
            store.events_after(&session_id, None).await.expect("replay"),
            frames
        );
    }

    create_session(store, "event-rollback").await;
    let mut events = vec![(EventType::AssistantMessage, json!({})); EVENT_INSERT_BATCH_CAPACITY];
    events.push((EventType::AssistantMessage, json!("\0")));
    store
        .insert_events("event-rollback", events)
        .await
        .expect_err("later batch must fail");
    assert!(store
        .events_after("event-rollback", None)
        .await
        .expect("rollback replay")
        .is_empty());

    create_session(store, "event-visibility").await;
    let rows = vec![EventRow::new(EventType::SessionIdle, json!({}))];
    let mut tx = store.pool.begin().await.expect("transaction starts");
    let frames = insert_event_rows_tx(&mut tx, "event-visibility", rows)
        .await
        .expect("event inserts");
    assert!(store
        .events_after("event-visibility", None)
        .await
        .expect("uncommitted replay")
        .is_empty());
    tx.commit().await.expect("transaction commits");
    assert_eq!(
        store
            .events_after("event-visibility", None)
            .await
            .expect("committed replay"),
        frames
    );
    assert_eq!(
        store.last_event_id("event-visibility").await.unwrap(),
        frames[0].event_id
    );
    store
        .clear_session_events("event-visibility")
        .await
        .unwrap();
    assert!(store
        .events_after("event-visibility", None)
        .await
        .unwrap()
        .is_empty());

    for session_id in ["event-concurrent-a", "event-concurrent-b"] {
        create_session(store, session_id).await;
    }
    let events = || {
        vec![
            (EventType::ModelCompleted, json!({ "duplicate": true }));
            EVENT_INSERT_BATCH_CAPACITY + 1
        ]
    };
    let (a, b) = tokio::join!(
        store.insert_events("event-concurrent-a", events()),
        store.insert_events("event-concurrent-b", events())
    );
    let (a, b) = (a.expect("first insert"), b.expect("second insert"));
    assert_ordered(&a);
    assert_ordered(&b);
    assert_eq!(
        a.iter()
            .chain(&b)
            .map(|frame| frame.event_id)
            .collect::<HashSet<_>>()
            .len(),
        a.len() + b.len()
    );

    db.cleanup().await;
}

fn entry(
    id: &str,
    parent_id: Option<&str>,
    item: TranscriptItem,
) -> agent_session::TranscriptStorageNode {
    agent_session::TranscriptStorageNode {
        id: id.to_string(),
        parent_id: parent_id.map(str::to_string),
        timestamp_ms: 100,
        item,
        provider_replay: Vec::new(),
    }
}

fn appended(entry: &agent_session::TranscriptStorageNode) -> SessionEvent {
    SessionEvent::TranscriptItemAppended {
        entry_id: entry.id.clone(),
        item: entry.item.clone(),
    }
}

#[tokio::test]
async fn output_persistence_batches_every_session_expansion_with_queue_state() {
    let Some(db) = test_store().await else {
        eprintln!("skipping postgres test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let store = &db.store;
    let session_id = "batched-output-events";
    create_session(store, session_id).await;
    store
        .enqueue_user_input(
            session_id,
            InputPriority::FollowUp,
            &UserMessage::text("queued"),
            Some("queued-client"),
            None,
        )
        .await
        .expect("input enqueues");
    let consumed = store
        .take_next_queued_input(session_id)
        .await
        .expect("input loads")
        .expect("input exists");
    let consumed_event = consumed.clone();
    store
        .clear_session_events(session_id)
        .await
        .expect("events clear");

    let entries = vec![
        entry(
            "start",
            None,
            TranscriptItem::TurnStarted { turn_id: TurnId(9) },
        ),
        entry(
            "assistant",
            Some("start"),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("done".to_string())],
            }),
        ),
        entry(
            "finish",
            Some("assistant"),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(9),
                outcome: TurnOutcome::Graceful,
            },
        ),
    ];
    let model = SessionAction::RequestModel {
        action_id: ActionId(51),
        turn_id: TurnId(9),
        model_context: ModelContext::new(),
        context_leaf_id: Some("finish".to_string()),
    };
    let tool = SessionAction::RequestTool {
        action_id: ActionId(52),
        turn_id: TurnId(9),
        tool_call: ToolCall {
            id: ToolCallId::new("call-output"),
            tool_name: "Bash".to_string(),
            args_json: r#"{"command":"pwd"}"#.to_string(),
        },
    };
    let actions = vec![model.clone(), tool.clone()];
    let events = vec![
        appended(&entries[0]),
        appended(&entries[1]),
        appended(&entries[2]),
        SessionEvent::ActionRequested { action: model },
        SessionEvent::ActionRequested { action: tool },
        SessionEvent::ActionRequested {
            action: SessionAction::CancelSessionWork,
        },
        SessionEvent::ActionCompleted {
            kind: SessionActionKind::Model,
            id: "51".to_string(),
        },
        SessionEvent::ActionCompleted {
            kind: SessionActionKind::Tool,
            id: "52".to_string(),
        },
        SessionEvent::ActionFailed {
            kind: SessionActionKind::Model,
            id: "53".to_string(),
            error: "model failed".to_string(),
        },
        SessionEvent::ActionFailed {
            kind: SessionActionKind::Tool,
            id: "54".to_string(),
            error: "tool failed".to_string(),
        },
    ];
    let batch = OutputBatch::new(&entries, Some("finish"), &events, &actions)
        .with_consumed_input(Some(consumed))
        .with_accepted_input(Some(AcceptedInput {
            priority: InputPriority::FollowUp,
            content: UserMessage::text("accepted"),
            client_input_id: Some("accepted-client".to_string()),
        }));
    let (result, statements) =
        with_event_insert_statement_count(store.persist_outputs(session_id, batch)).await;
    let (frames, dispatch) = result.expect("outputs persist");

    assert_eq!(statements, 1);
    assert_ordered(&frames);
    let queue = store.queue_state(session_id).await.expect("queue loads");
    assert!(queue.queued_inputs.is_empty());
    let accepted_id = store
        .find_client_input(session_id, "accepted-client")
        .await
        .expect("accepted input loads")
        .expect("accepted input exists")
        .input_id;
    let records = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| TranscriptEntryRecord {
            id: entry.id.clone(),
            parent_id: entry.parent_id.clone(),
            timestamp_ms: entry.timestamp_ms,
            sequence: index as i64 + 1,
            item: entry.item.clone(),
            provider_replay: Vec::new(),
        })
        .collect::<Vec<_>>();
    let queue_payload = json!({
        "session_revision": queue.session_revision,
        "queue_revision": queue.queue_revision,
        "transcript_revision": queue.transcript_revision,
        "activity": queue.activity,
        "queued_inputs": [],
    });
    let transcript_payload = |record: &TranscriptEntryRecord| {
        json!({
            "entry_id": record.id,
            "item": record.item,
            "entry": {
                "id": record.id,
                "parent_id": record.parent_id,
                "timestamp_ms": record.timestamp_ms,
                "sequence": record.sequence,
                "item": record.item,
            },
            "tree_node": tree_node_from_entry(record),
            "active_leaf_id": "finish",
            "session_revision": queue.session_revision,
            "queue_revision": queue.queue_revision,
            "transcript_revision": queue.transcript_revision,
        })
    };
    let expected = vec![
        (EventType::InputConsumed, json!({
            "input_id": consumed_event.id,
            "priority": consumed_event.priority,
            "client_input_id": consumed_event.client_input_id,
        })),
        (EventType::InputAccepted, json!({
            "input_id": accepted_id,
            "priority": "follow_up",
            "client_input_id": "accepted-client",
            "content": [{ "type": "text", "text": "accepted" }],
            "content_type": "user_message",
        })),
        (EventType::TranscriptAppended, transcript_payload(&records[0])),
        (EventType::TurnStarted, json!({ "turn_id": 9, "entry_id": "start" })),
        (EventType::TranscriptAppended, transcript_payload(&records[1])),
        (EventType::AssistantMessage, json!({
            "entry_id": "assistant",
            "assistant": { "items": [{ "type": "text", "text": "done" }] },
        })),
        (EventType::TranscriptAppended, transcript_payload(&records[2])),
        (EventType::TurnFinished, json!({
            "turn_id": 9, "outcome": "Graceful", "entry_id": "finish",
        })),
        (EventType::ActionRequested, json!({
            "kind": "model", "action_id": 51, "action_row_id": dispatch[0].row_id,
            "payload": { "context_leaf_id": "finish" }, "activity": "running",
        })),
        (EventType::ModelRequested, json!({
            "action_row_id": dispatch[0].row_id, "action_id": 51, "activity": "running",
        })),
        (EventType::ActionRequested, json!({
            "kind": "tool", "action_id": 52, "action_row_id": dispatch[1].row_id,
            "payload": { "id": "call-output", "tool_name": "Bash", "args_json": "{\"command\":\"pwd\"}" },
            "activity": "running",
        })),
        (EventType::ToolRequested, json!({
            "action_row_id": dispatch[1].row_id, "action_id": 52, "activity": "running",
        })),
        (EventType::SessionWorkCancelled, json!({})),
        (EventType::ModelCompleted, json!({ "action_id": "51" })),
        (EventType::ToolCompleted, json!({ "action_id": "52" })),
        (EventType::ModelError, json!({ "action_id": "53", "error": "model failed" })),
        (EventType::ToolError, json!({ "action_id": "54", "error": "tool failed" })),
    ]
    .into_iter()
    .map(|(event, payload)| {
        let mut data = queue_payload.clone();
        if matches!(event, EventType::InputConsumed | EventType::InputAccepted) {
            data.as_object_mut()
                .expect("queue payload is an object")
                .extend(payload.as_object().expect("extra payload is an object").clone());
        } else {
            data = payload;
        }
        (event, data)
    })
    .collect::<Vec<_>>();
    assert_eq!(
        frames
            .iter()
            .map(|frame| (frame.event, frame.data.clone()))
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        store.events_after(session_id, None).await.expect("replay"),
        frames
    );

    db.cleanup().await;
}
