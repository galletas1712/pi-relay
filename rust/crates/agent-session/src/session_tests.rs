use super::*;
use crate::SessionActionKind;
use agent_core::AgentInput;
use agent_vocab::{
    ActionId, AssistantItem, AssistantMessage, CompactionSummary, ProviderReplayItem, ToolCall,
    ToolCallId, ToolResultMessage, ToolResultStatus, TranscriptItem, TurnId, TurnOutcome,
    UserMessage,
};

fn finished_model_context(input: &str) -> ModelContext {
    ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(1) },
        TranscriptItem::UserMessage(UserMessage::text(input)),
        TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Graceful,
        },
    ])
}

fn finished_turn(turn_id: u64, user: &str, assistant: &str) -> Vec<TranscriptItem> {
    vec![
        TranscriptItem::TurnStarted {
            turn_id: TurnId(turn_id),
        },
        TranscriptItem::UserMessage(UserMessage::text(user)),
        TranscriptItem::AssistantMessage(AssistantMessage {
            items: vec![AssistantItem::Text(assistant.to_string())],
        }),
        TranscriptItem::TurnFinished {
            turn_id: TurnId(turn_id),
            outcome: TurnOutcome::Graceful,
        },
    ]
}

fn assert_single_request_model(
    actions: Vec<SessionAction>,
    expected_action_id: ActionId,
    expected_turn_id: TurnId,
) -> ModelContext {
    let [SessionAction::RequestModel {
        action_id,
        turn_id,
        model_context,
        ..
    }] = actions.as_slice()
    else {
        panic!("expected one RequestModel action, got {actions:?}");
    };
    assert_eq!(
        (*action_id, *turn_id),
        (expected_action_id, expected_turn_id)
    );
    model_context.clone()
}

fn empty_assistant() -> AssistantMessage {
    AssistantMessage { items: Vec::new() }
}

#[test]
fn rewind_requires_boundaries() {
    let mut session = AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(1) },
        TranscriptItem::UserMessage(UserMessage::text("first")),
        TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Graceful,
        },
        TranscriptItem::TurnStarted { turn_id: TurnId(2) },
        TranscriptItem::UserMessage(UserMessage::text("second")),
        TranscriptItem::TurnFinished {
            turn_id: TurnId(2),
            outcome: TurnOutcome::Graceful,
        },
    ]));
    let mid_turn_id = session.transcript_store().entries()[1].id.clone();
    let turn_one_end_id = session.transcript_store().entries()[2].id.clone();

    assert_eq!(
        session.rewind(Some(&mid_turn_id)),
        Err(HistoryOperationError::Store(
            TranscriptStoreError::NotTurnBoundary
        ))
    );
    session
        .rewind(Some(&turn_one_end_id))
        .expect("turn end is a valid rewind point");
    assert_eq!(session.model_context().last_turn_id(), TurnId(1));
}

#[test]
fn invalid_rewind_target_does_not_cancel_live_work() {
    let mut session = AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(1) },
        TranscriptItem::UserMessage(UserMessage::text("first")),
        TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Graceful,
        },
    ]));
    let mid_turn_id = session.transcript_store().entries()[1].id.clone();
    session
        .enqueue_input(AgentInput::follow_up("second"))
        .expect("plain follow-up is valid");
    session.drive();

    assert_eq!(
        session.rewind(Some(&mid_turn_id)),
        Err(HistoryOperationError::Store(
            TranscriptStoreError::NotTurnBoundary
        ))
    );

    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(2));
}

#[test]
fn history_operation_preserves_queued_user_inputs_and_drops_queued_completions() {
    let mut session = AgentSession::from_model_context(finished_model_context("first"));

    session
        .enqueue_input(AgentInput::follow_up("queued follow-up"))
        .expect("plain follow-up is valid");
    session
        .enqueue_input(AgentInput::steer("queued steer"))
        .expect("plain steer is valid");
    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(99),
            turn_id: TurnId(99),
            assistant: AssistantMessage {
                items: vec![AssistantItem::Text("stale".to_string())],
            },
        })
        .expect("stale completion is valid input");

    session
        .rewind(None)
        .expect("edit can preserve user inputs and drop queued completions");
    session.drive();

    let first_request_context =
        assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(1));
    assert!(matches!(
        first_request_context.transcript_items().last(),
        Some(TranscriptItem::UserMessage(text)) if text == "queued steer"
    ));
    assert!(!session
        .model_context()
        .transcript_items()
        .iter()
        .any(|item| matches!(
            item,
            TranscriptItem::AssistantMessage(message)
                if message.items == vec![AssistantItem::Text("stale".to_string())]
        )));

    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        })
        .expect("matching model completion is valid");
    session.drive();

    let second_request_context =
        assert_single_request_model(session.drain_actions(), ActionId(2), TurnId(2));
    assert!(matches!(
        second_request_context.transcript_items().last(),
        Some(TranscriptItem::UserMessage(text)) if text == "queued follow-up"
    ));
}

#[test]
fn context_tracks_core_turn_items() {
    let session = AgentSession::from_model_context(finished_model_context("hello"));

    assert_eq!(session.transcript_store().entries().len(), 3);
    assert!(session.transcript_store().is_turn_boundary());
    assert_eq!(session.model_context().last_turn_id(), TurnId(1));
}

#[test]
fn drive_drains_core_items_into_the_session_context() {
    let mut session = AgentSession::new();
    let assistant = AssistantMessage {
        items: vec![AssistantItem::Text("hi".to_string())],
    };

    session
        .enqueue_input(AgentInput::follow_up("hello"))
        .expect("plain follow-up is valid");
    session.drive();
    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: assistant.clone(),
        })
        .expect("matching model completion is valid");
    session.drive();

    assert_eq!(
        session.model_context().transcript_items(),
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage(UserMessage::text("hello")),
            TranscriptItem::AssistantMessage(assistant),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ]
    );
    // Driving again drains nothing new; the core buffer was already empty.
    session.drive();
    assert_eq!(session.model_context().transcript_items().len(), 4);
}

#[test]
fn live_transcript_keeps_open_turns_open() {
    let mut session = AgentSession::new();

    session
        .enqueue_input(AgentInput::follow_up("hello"))
        .expect("plain follow-up is valid");
    session.drive();

    assert_eq!(
        session.model_context().transcript_items(),
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage(UserMessage::text("hello")),
        ]
    );
}

#[test]
fn rehydrating_an_incomplete_transcript_patches_a_crashed_finish() {
    let model_context = vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(7) },
        TranscriptItem::UserMessage(UserMessage::text("hello")),
    ];

    let session =
        AgentSession::from_model_context(ModelContext::from_transcript_items(model_context));

    assert_eq!(
        session.model_context().transcript_items(),
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(7) },
            TranscriptItem::UserMessage(UserMessage::text("hello")),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(7),
                outcome: TurnOutcome::Crashed,
            },
        ]
    );
    assert_eq!(session.model_context().last_turn_id(), TurnId(7));
}

#[test]
fn from_model_context_closes_open_turn_as_crashed() {
    let mut session = AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(7) },
        TranscriptItem::UserMessage(UserMessage::text("hello")),
    ]));

    assert_eq!(
        session.model_context().transcript_items(),
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(7) },
            TranscriptItem::UserMessage(UserMessage::text("hello")),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(7),
                outcome: TurnOutcome::Crashed,
            },
        ]
    );

    session
        .enqueue_input(AgentInput::follow_up("next"))
        .expect("plain follow-up is valid");
    session.drive();
    assert!(matches!(
        session.model_context().transcript_items().last(),
        Some(TranscriptItem::UserMessage(text)) if text == "next"
    ));
}

#[test]
fn from_transcript_store_repairs_open_tool_turn_to_boundary() {
    let tool_call = ToolCall {
        id: ToolCallId::from_u64(1),
        tool_name: "read".to_string(),
        args_json: "{}".to_string(),
    };
    let store = TranscriptStore::from_model_context(&ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(7) },
        TranscriptItem::UserMessage(UserMessage::text("hello")),
        TranscriptItem::AssistantMessage(AssistantMessage {
            items: vec![AssistantItem::ToolCall(tool_call.clone())],
        }),
        TranscriptItem::ToolCallStarted {
            turn_id: TurnId(7),
            tool_call: tool_call.clone(),
        },
    ]));

    let session = AgentSession::from_transcript_store(store)
        .expect("store restore should repair an open tool turn");

    assert_eq!(
        session.model_context().transcript_items(),
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(7) },
            TranscriptItem::UserMessage(UserMessage::text("hello")),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            }),
            TranscriptItem::ToolCallStarted {
                turn_id: TurnId(7),
                tool_call: tool_call.clone(),
            },
            TranscriptItem::ToolResult(ToolResultMessage::crashed(
                tool_call.id,
                tool_call.tool_name,
            )),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(7),
                outcome: TurnOutcome::Crashed,
            },
        ]
    );
    assert!(session.transcript_store().is_turn_boundary());
    assert!(!session.is_ready_to_continue());
}

#[test]
fn stored_session_interrupt_recovery_repairs_open_tool_turn_and_emits_events() {
    let tool_call = ToolCall {
        id: ToolCallId::from_u64(1),
        tool_name: "read".to_string(),
        args_json: "{}".to_string(),
    };
    let store = TranscriptStore::from_model_context(&ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(7) },
        TranscriptItem::UserMessage(UserMessage::text("hello")),
        TranscriptItem::AssistantMessage(AssistantMessage {
            items: vec![AssistantItem::ToolCall(tool_call.clone())],
        }),
        TranscriptItem::ToolCallStarted {
            turn_id: TurnId(7),
            tool_call: tool_call.clone(),
        },
    ]));
    let mut stored = StoredSession::new("s1");
    stored.active_leaf_id = store.active_leaf_id().map(str::to_string);
    stored.entries = store.entries().into_iter().map(Into::into).collect();
    let mut session = AgentSession::from_stored_session_interrupted(stored)
        .expect("interrupt recovery should close the open tool turn");

    assert_eq!(
        &session.model_context().transcript_items()[4..],
        &[
            TranscriptItem::ToolResult(ToolResultMessage::crashed(
                tool_call.id,
                tool_call.tool_name,
            )),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(7),
                outcome: TurnOutcome::Interrupted,
            },
        ]
    );
    assert!(session.transcript_store().is_turn_boundary());
    assert_eq!(
        session
            .drain_events()
            .into_iter()
            .filter(|event| matches!(event, SessionEvent::TranscriptItemAppended { .. }))
            .count(),
        2,
        "both synthesized durable entries must be persisted by the caller"
    );
}

#[test]
fn stored_session_repairs_compacted_open_tool_turn_to_boundary() {
    let first = ToolCall {
        id: ToolCallId::from_u64(1),
        tool_name: "bash".to_string(),
        args_json: "{}".to_string(),
    };
    let second = ToolCall {
        id: ToolCallId::from_u64(2),
        tool_name: "bash".to_string(),
        args_json: "{}".to_string(),
    };
    let store = TranscriptStore::from_model_context(&ModelContext::from_transcript_items(vec![
        TranscriptItem::CompactionSummary(CompactionSummary::new(
            "session",
            "source",
            "summary",
            None,
            TurnId(58),
        )),
        TranscriptItem::AssistantMessage(AssistantMessage {
            items: vec![
                AssistantItem::ToolCall(first.clone()),
                AssistantItem::ToolCall(second.clone()),
            ],
        }),
        TranscriptItem::ToolCallStarted {
            turn_id: TurnId(58),
            tool_call: first.clone(),
        },
        TranscriptItem::ToolResult(ToolResultMessage {
            tool_call_id: first.id.clone(),
            tool_name: first.tool_name.clone(),
            output: "ok".to_string(),
            status: ToolResultStatus::Success,
        }),
    ]));

    let session = AgentSession::from_transcript_store(store)
        .expect("compacted open turn should be repairable");

    assert_eq!(
        session.model_context().transcript_items().last(),
        Some(&TranscriptItem::TurnFinished {
            turn_id: TurnId(58),
            outcome: TurnOutcome::Crashed,
        })
    );
    assert!(session
        .model_context()
        .transcript_items()
        .iter()
        .any(|item| matches!(
            item,
            TranscriptItem::ToolResult(result)
                if result.tool_call_id == second.id && result.status == ToolResultStatus::Crashed
        )));
    assert!(session.transcript_store().is_turn_boundary());
}

#[test]
fn stored_session_round_trips_the_active_branch() {
    let mut items = Vec::new();
    items.extend(finished_turn(1, "first", "done"));
    items.extend(finished_turn(2, "second", "done"));
    let mut session = AgentSession::from_model_context(ModelContext::from_transcript_items(items));
    let first_turn_leaf_id = session.transcript_store().entries()[3].id.clone();

    session
        .rewind(Some(&first_turn_leaf_id))
        .expect("first turn finish is a valid branch target");
    let stored = session.to_stored_session("s1");
    assert_eq!(stored.active_leaf_id, Some(first_turn_leaf_id.clone()));

    let restored =
        AgentSession::from_stored_session(stored).expect("stored session should rehydrate");
    assert_eq!(restored.model_context(), session.model_context());
    assert_eq!(
        restored.transcript_store().active_leaf_id(),
        Some(first_turn_leaf_id.as_str())
    );
}

#[test]
fn unmatched_tool_completion_before_tool_request_is_ignored() {
    let mut session = AgentSession::new();
    let tool_call = ToolCall {
        id: ToolCallId::from_u64(1),
        tool_name: "bash".to_string(),
        args_json: "{}".to_string(),
    };

    session
        .enqueue_input(AgentInput::follow_up("go"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(1));

    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            },
        })
        .expect("matching model completion is valid");
    session
        .enqueue_input(AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: tool_call.id,
                tool_name: tool_call.tool_name.clone(),
                output: "too early".to_string(),
                status: ToolResultStatus::Success,
            },
        })
        .expect("early tool completion is well formed but not yet matchable");
    session.drive();

    let actions = session.drain_actions();
    assert!(matches!(
        actions.as_slice(),
        [SessionAction::RequestTool {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            ..
        }]
    ));
    assert!(!session
        .model_context()
        .transcript_items()
        .iter()
        .any(|item| matches!(item, TranscriptItem::ToolResult(_))));
}

#[test]
fn completion_event_is_not_emitted_when_interrupt_wins_before_drive() {
    let mut session = AgentSession::new();
    session
        .enqueue_input(AgentInput::follow_up("first"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(1));
    session.drain_events();

    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: empty_assistant(),
        })
        .expect("session model completion should be valid");
    session
        .enqueue_input(AgentInput::Interrupt)
        .expect("interrupt is valid");
    session.drive();

    assert!(session.drain_events().iter().all(|event| {
        !matches!(
            event,
            SessionEvent::ActionCompleted {
                kind: SessionActionKind::Model,
                id,
            } if id == "1"
        )
    }));
    assert_eq!(
        session.model_context().transcript_items().last(),
        Some(&TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Interrupted,
        })
    );
}

#[test]
fn rehydrating_a_graceful_boundary_restores_idle_state() {
    let model_context = vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(2) },
        TranscriptItem::UserMessage(UserMessage::text("hello")),
        TranscriptItem::TurnFinished {
            turn_id: TurnId(2),
            outcome: TurnOutcome::Graceful,
        },
    ];

    let session = AgentSession::from_model_context(ModelContext::from_transcript_items(
        model_context.clone(),
    ));

    assert_eq!(
        session.model_context().transcript_items(),
        model_context.as_slice()
    );
    assert_eq!(session.model_context().last_turn_id(), TurnId(2));
}

#[test]
fn history_operation_can_interrupt_drained_model_action() {
    let mut session = AgentSession::new();
    session
        .enqueue_input(AgentInput::follow_up("hi"))
        .expect("plain follow-up is valid");
    session.drive();
    let actions = session.drain_actions();
    assert!(matches!(
        actions.as_slice(),
        [SessionAction::RequestModel { .. }]
    ));

    session
        .rewind(None)
        .expect("edit should interrupt in-flight model work before applying");
    assert_eq!(
        session.drain_actions(),
        vec![SessionAction::CancelSessionWork]
    );
    assert!(session.model_context().transcript_items().is_empty());

    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        })
        .expect("late model completion is valid but stale");
    session.drive();
}

#[test]
fn tool_crash_result_records_failure_and_continues_turn() {
    let mut session = AgentSession::new();
    session
        .enqueue_input(AgentInput::follow_up("hi"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(1));

    let tool_call = ToolCall {
        id: ToolCallId::from_u64(1),
        tool_name: "bash".to_string(),
        args_json: "{}".to_string(),
    };
    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            },
        })
        .expect("matching model completion is valid");
    session.drive();
    session.drain_actions();

    session
        .enqueue_input(AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: tool_call.id,
                tool_name: tool_call.tool_name,
                output: "tool runner crashed".to_string(),
                status: ToolResultStatus::Crashed,
            },
        })
        .expect("matching tool completion is valid");
    session.drive();

    assert!(session.is_ready_to_continue());
    assert!(matches!(
        session.model_context().transcript_items().last(),
        Some(TranscriptItem::ToolResult(result)) if result.status == ToolResultStatus::Crashed
    ));

    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(3), TurnId(1));
}

#[test]
fn model_failure_marks_turn_crashed_and_unblocks_history_operations() {
    let mut session = AgentSession::new();
    session
        .enqueue_input(AgentInput::follow_up("hi"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(1));

    session
        .enqueue_input(AgentInput::ModelFailed {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            error: "provider failed".to_string(),
        })
        .expect("matching model failure is valid");
    session.drive();

    assert_eq!(
        session.model_context().transcript_items(),
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage(UserMessage::text("hi")),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Crashed,
            },
        ]
    );
    assert!(session.drain_events().iter().any(|event| matches!(
        event,
        SessionEvent::ActionFailed {
            kind: SessionActionKind::Model,
            error,
            ..
        } if error == "provider failed"
    )));
}

#[test]
fn terminal_model_turn_can_resume_from_original_checkpoint_without_duplicate_user_message() {
    let mut session = AgentSession::new();
    session
        .enqueue_input(AgentInput::follow_up("hi"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(1));
    let checkpoint_id = session
        .transcript_store()
        .entries()
        .last()
        .expect("model checkpoint should be the user message")
        .id
        .clone();

    session
        .enqueue_input(AgentInput::ModelFailed {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            error: "provider failed".to_string(),
        })
        .expect("matching model failure is valid");
    session.drive();
    assert!(matches!(
        session.model_context().transcript_items().last(),
        Some(TranscriptItem::TurnFinished {
            outcome: TurnOutcome::Crashed,
            ..
        })
    ));

    session
        .resume_model_turn(&checkpoint_id, TurnId(1), ActionId(1))
        .expect("crashed model turn can be resumed from its checkpoint");
    let resumed_context =
        assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(1));
    assert_eq!(
        resumed_context.transcript_items(),
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage(UserMessage::text("hi")),
        ]
    );
    assert_eq!(
        session.transcript_store().active_leaf_id(),
        Some(checkpoint_id.as_str())
    );

    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage {
                items: vec![AssistantItem::Text("resumed".to_string())],
            },
        })
        .expect("matching resumed model completion is valid");
    session.drive();

    let model_context = session.model_context();
    let items = model_context.transcript_items();
    assert_eq!(
        items,
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage(UserMessage::text("hi")),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("resumed".to_string())],
            }),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ]
    );
    assert_eq!(
        session
            .transcript_store()
            .entries()
            .into_iter()
            .filter(|entry| matches!(
                entry.item,
                TranscriptItem::UserMessage(ref message) if message == &UserMessage::text("hi")
            ))
            .count(),
        1
    );
    assert!(session
        .transcript_store()
        .entries()
        .into_iter()
        .any(|entry| matches!(
            entry.item,
            TranscriptItem::TurnFinished {
                outcome: TurnOutcome::Crashed,
                ..
            }
        )));
}

#[test]
fn max_output_tokens_persists_partial_assistant_then_crashed_boundary() {
    let mut session = AgentSession::new();
    session
        .enqueue_input(AgentInput::follow_up("hi"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(1));

    let provider_replay = vec![ProviderReplayItem::new(
        agent_vocab::ProviderKind::OpenAi,
        &serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "partial" }],
        }),
    )
    .expect("replay serializes")];
    session
        .enqueue_session_input(SessionInput::ModelMaxOutputTokens {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage {
                items: vec![AssistantItem::Text("partial".to_string())],
            },
            provider_replay: provider_replay.clone(),
            error: "provider response hit max_output_tokens".to_string(),
        })
        .expect("max-output completion should be valid");
    session.drive();

    assert!(session.drain_actions().is_empty());
    let context = session.model_context();
    assert_eq!(
        context.transcript_items(),
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage(UserMessage::text("hi")),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("partial".to_string())],
            }),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Crashed,
            },
        ]
    );
    let entries = session.transcript_store().entries();
    let assistant_entry = entries
        .iter()
        .find(|entry| matches!(entry.item, TranscriptItem::AssistantMessage(_)))
        .expect("assistant entry should persist");
    assert_eq!(assistant_entry.provider_replay, provider_replay);
    assert!(session.drain_events().iter().any(|event| matches!(
        event,
        SessionEvent::ActionFailed {
            kind: SessionActionKind::Model,
            id,
            error,
        } if id == "1" && error == "provider response hit max_output_tokens"
    )));
}

#[test]
fn late_action_drain_does_not_leave_completed_request_pending() {
    let mut session = AgentSession::new();
    session
        .enqueue_input(AgentInput::follow_up("hi"))
        .expect("plain follow-up is valid");
    session.drive();

    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        })
        .expect("matching model completion is valid");
    session.drive();

    let late_actions = session.drain_actions();
    assert!(late_actions.is_empty());
}

#[test]
fn stale_completion_after_history_operation_cannot_attach_to_reused_turn_id() {
    let mut session = AgentSession::from_model_context(finished_model_context("first"));
    let turn_one_end_id = session.transcript_store().entries()[2].id.clone();

    session
        .enqueue_input(AgentInput::follow_up("old second"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(2));
    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(2),
            assistant: AssistantMessage {
                items: vec![AssistantItem::Text("old response".to_string())],
            },
        })
        .expect("matching model completion is valid");
    session.drive();

    session
        .rewind(Some(&turn_one_end_id))
        .expect("completed history can rewind to turn one");
    session
        .enqueue_input(AgentInput::follow_up("new second"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(2), TurnId(2));

    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(2),
            assistant: AssistantMessage {
                items: vec![AssistantItem::Text("stale old response".to_string())],
            },
        })
        .expect("well-formed stale completion is valid input");
    session.drive();
    assert_eq!(
        session.model_context().transcript_items(),
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage(UserMessage::text("first")),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptItem::TurnStarted { turn_id: TurnId(2) },
            TranscriptItem::UserMessage(UserMessage::text("new second")),
        ]
    );

    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(2),
            assistant: AssistantMessage {
                items: vec![AssistantItem::Text("new response".to_string())],
            },
        })
        .expect("matching model completion is valid");
    session.drive();
    assert_eq!(
        session.model_context().transcript_items().last(),
        Some(&TranscriptItem::TurnFinished {
            turn_id: TurnId(2),
            outcome: TurnOutcome::Graceful,
        })
    );
}

#[test]
fn history_operation_preserves_undrained_cancel_session_work() {
    let mut session = AgentSession::from_model_context(finished_model_context("first"));

    session
        .enqueue_input(AgentInput::follow_up("old second"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(2));

    session
        .enqueue_input(AgentInput::Interrupt)
        .expect("interrupt is valid");
    session.drive();

    session
        .rewind(None)
        .expect("edit can proceed while cancellation is waiting to be drained");

    assert_eq!(
        session.drain_actions(),
        vec![SessionAction::CancelSessionWork]
    );
    assert!(session.model_context().transcript_items().is_empty());
}

#[test]
fn direct_model_completion_is_rejected_without_mutating_session_state() {
    let mut session = AgentSession::new();
    let result = session.enqueue_input(AgentInput::ModelCompleted {
        action_id: ActionId(1),
        turn_id: TurnId(1),
        assistant: AssistantMessage { items: Vec::new() },
    });

    assert_eq!(
        result,
        Err(SessionInputError::ModelCompletionRequiresSessionInput)
    );
    session.drive();
    assert!(session.model_context().transcript_items().is_empty());
}

#[test]
fn history_operation_interrupts_active_tool_work_before_applying() {
    let mut session = AgentSession::new();
    session
        .enqueue_input(AgentInput::follow_up("go"))
        .expect("plain follow-up is valid");
    session.drive();
    session.drain_actions();

    let tool_call = ToolCall {
        id: ToolCallId::from_u64(1),
        tool_name: "bash".to_string(),
        args_json: "{}".to_string(),
    };
    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            },
        })
        .expect("matching model completion is valid");
    session.drive();
    session.drain_actions();

    session
        .rewind(None)
        .expect("edit should interrupt active tool work before applying");
    assert_eq!(
        session.drain_actions(),
        vec![SessionAction::CancelSessionWork]
    );
    assert!(session.model_context().transcript_items().is_empty());

    session
        .enqueue_input(AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: tool_call.id,
                tool_name: tool_call.tool_name,
                output: "late".to_string(),
                status: ToolResultStatus::Success,
            },
        })
        .expect("late tool completion is valid but stale");
    session.drive();
    assert_eq!(session.model_context().transcript_items(), &[]);
}

#[test]
fn mismatched_tool_completion_does_not_clear_live_tool_work() {
    let mut session = AgentSession::new();
    session
        .enqueue_input(AgentInput::follow_up("go"))
        .expect("plain follow-up is valid");
    session.drive();
    session.drain_actions();

    let tool_call = ToolCall {
        id: ToolCallId::from_u64(1),
        tool_name: "bash".to_string(),
        args_json: "{}".to_string(),
    };
    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            },
        })
        .expect("matching model completion is valid");
    session.drive();
    session.drain_actions();

    session
        .enqueue_input(AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: ToolCallId::from_u64(99),
                tool_name: tool_call.tool_name,
                output: "misrouted".to_string(),
                status: ToolResultStatus::Success,
            },
        })
        .expect("well-formed but mismatched tool completion is valid");
    session.drive();

    session
        .rewind(None)
        .expect("edit should still cancel the real in-flight tool work");
    assert_eq!(
        session.drain_actions(),
        vec![SessionAction::CancelSessionWork]
    );
}

#[test]
fn interrupt_emits_session_work_cancellation_and_unblocks_edits() {
    let mut session = AgentSession::new();
    session
        .enqueue_input(AgentInput::follow_up("hi"))
        .expect("plain follow-up is valid");
    session.drive();
    session.drain_actions();

    session
        .enqueue_input(AgentInput::Interrupt)
        .expect("interrupt is valid");
    session.drive();
    let actions = session.drain_actions();
    assert!(actions
        .iter()
        .any(|a| matches!(a, SessionAction::CancelSessionWork)));

    session
        .enqueue_session_input(SessionInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage {
                items: vec![AssistantItem::Text("late".to_string())],
            },
        })
        .expect("late model completion is still well-formed input");
    session.drive();
    assert!(!session
        .model_context()
        .transcript_items()
        .iter()
        .any(|item| matches!(
            item,
            TranscriptItem::AssistantMessage(message)
                if message.items == vec![AssistantItem::Text("late".to_string())]
        )));
}
