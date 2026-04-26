use super::*;
use agent_core::{
    ActionId, AssistantItem, AssistantMessage, InjectedMessage, ToolCall, ToolCallId,
    ToolResultMessage, ToolResultStatus, TurnId, TurnOutcome,
};

fn finished_model_context(input: &str) -> ModelContext {
    ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(1) },
        TranscriptItem::UserMessage(input.to_string()),
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
        TranscriptItem::UserMessage(user.to_string()),
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

fn session_with_compactable_history() -> AgentSession {
    let mut items = Vec::new();
    items.extend(finished_turn(
        1,
        "first user message with enough text to count",
        "first assistant message with enough text to count",
    ));
    items.extend(finished_turn(
        2,
        "second user message with enough text to count",
        "second assistant message with enough text to count",
    ));
    AgentSession::from_model_context(ModelContext::from_transcript_items(items))
        .with_auto_compaction(AutoCompactionSettings::new(1))
}

fn compacted_boundary_context(summary: &str, last_turn_id: u64) -> ModelContext {
    ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted {
            turn_id: TurnId(last_turn_id),
        },
        TranscriptItem::Injected(InjectedMessage::new("compaction", summary)),
        TranscriptItem::TurnFinished {
            turn_id: TurnId(last_turn_id),
            outcome: TurnOutcome::Graceful,
        },
    ])
}

fn compacted_open_context(summary: &str, turn_id: u64, user: &str) -> ModelContext {
    ModelContext::from_transcript_items(vec![
        TranscriptItem::Injected(InjectedMessage::new("compaction", summary)),
        TranscriptItem::TurnStarted {
            turn_id: TurnId(turn_id),
        },
        TranscriptItem::UserMessage(user.to_string()),
    ])
}

fn assert_single_request_compaction(
    actions: Vec<SessionAction>,
) -> (CompactionRequestId, ModelContext) {
    let [SessionAction::RequestCompaction {
        request_id,
        model_context,
    }] = actions.as_slice()
    else {
        panic!("expected one RequestCompaction action, got {actions:?}");
    };
    (*request_id, model_context.clone())
}

#[test]
fn rewind_and_fork_only_accept_turn_finished_entries() {
    let mut session = AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(1) },
        TranscriptItem::UserMessage("first".to_string()),
        TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Graceful,
        },
        TranscriptItem::TurnStarted { turn_id: TurnId(2) },
        TranscriptItem::UserMessage("second".to_string()),
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
    assert_eq!(
        session.fork(Some(&mid_turn_id)).map(|_| ()),
        Err(HistoryOperationError::Store(
            TranscriptStoreError::NotTurnBoundary
        ))
    );

    session
        .rewind(Some(&turn_one_end_id))
        .expect("turn end is a valid rewind point");
    assert_eq!(session.model_context().last_turn_id(), TurnId(1));

    let fork = session
        .fork(Some(&turn_one_end_id))
        .expect("turn end is a valid fork point");
    assert_eq!(fork.model_context().last_turn_id(), TurnId(1));
}

#[test]
fn fork_can_copy_a_boundary_path_while_source_session_is_running() {
    let mut session = AgentSession::from_model_context(finished_model_context("hello"));
    let boundary_id = session
        .transcript_store()
        .entries()
        .last()
        .expect("finished context has a boundary entry")
        .id
        .clone();

    session
        .enqueue_input(AgentInput::follow_up("new work"))
        .expect("plain follow-up is valid");
    session.drive();

    let fork = session
        .fork(Some(&boundary_id))
        .expect("fork only copies the requested boundary path");
    assert_eq!(fork.model_context().last_turn_id(), TurnId(1));
    assert!(matches!(
        fork.model_context().transcript_items().last(),
        Some(TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Graceful,
        })
    ));
}

#[test]
fn invalid_rewind_target_does_not_cancel_live_work() {
    let mut session = AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(1) },
        TranscriptItem::UserMessage("first".to_string()),
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
        .enqueue_input(AgentInput::ModelCompleted {
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
        .enqueue_input(AgentInput::ModelCompleted {
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
fn drive_absorbs_core_items_into_the_session_context() {
    let mut session = AgentSession::new();
    let assistant = AssistantMessage {
        items: vec![AssistantItem::Text("hi".to_string())],
    };

    session
        .enqueue_input(AgentInput::follow_up("hello"))
        .expect("plain follow-up is valid");
    session.drive();
    session
        .enqueue_input(AgentInput::ModelCompleted {
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
            TranscriptItem::UserMessage("hello".to_string()),
            TranscriptItem::AssistantMessage(assistant),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ]
    );
    // Driving again absorbs nothing new; the core buffer was drained.
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
            TranscriptItem::UserMessage("hello".to_string()),
        ]
    );
}

#[test]
fn rehydrating_an_incomplete_transcript_patches_a_crashed_finish() {
    let model_context = vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(7) },
        TranscriptItem::UserMessage("hello".to_string()),
    ];

    let session = AgentSession::from_transcript_items(model_context);

    assert_eq!(
        session.model_context().transcript_items(),
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(7) },
            TranscriptItem::UserMessage("hello".to_string()),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(7),
                outcome: TurnOutcome::Crashed,
            },
        ]
    );
    assert_eq!(session.last_turn_id(), TurnId(7));
}

#[test]
fn from_model_context_recovers_an_open_tail_as_crashed() {
    let mut session = AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(7) },
        TranscriptItem::UserMessage("hello".to_string()),
    ]));

    assert_eq!(
        session.model_context().transcript_items(),
        &[
            TranscriptItem::TurnStarted { turn_id: TurnId(7) },
            TranscriptItem::UserMessage("hello".to_string()),
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
fn restore_remains_quiescent_until_new_input_reaches_a_model_barrier() {
    let mut items = Vec::new();
    items.extend(finished_turn(
        1,
        "first user message with enough text to count",
        "first assistant message with enough text to count",
    ));
    items.extend(finished_turn(
        2,
        "second user message with enough text to count",
        "second assistant message with enough text to count",
    ));
    items.push(TranscriptItem::TurnStarted { turn_id: TurnId(3) });
    items.push(TranscriptItem::UserMessage(
        "open turn before process death".to_string(),
    ));

    let mut session = AgentSession::from_transcript_items(items)
        .with_auto_compaction(AutoCompactionSettings::new(1));

    assert!(session.drain_actions().is_empty());
    assert!(matches!(
        session.model_context().transcript_items().last(),
        Some(TranscriptItem::TurnFinished {
            turn_id: TurnId(3),
            outcome: TurnOutcome::Crashed,
        })
    ));

    session
        .enqueue_input(AgentInput::follow_up("after restore"))
        .expect("plain follow-up is valid");
    session.drive();
    assert!(matches!(
        session.drain_actions().as_slice(),
        [SessionAction::RequestCompaction { .. }]
    ));
}

#[test]
fn rehydrating_a_graceful_boundary_restores_idle_state() {
    let model_context = vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(2) },
        TranscriptItem::UserMessage("hello".to_string()),
        TranscriptItem::TurnFinished {
            turn_id: TurnId(2),
            outcome: TurnOutcome::Graceful,
        },
    ];

    let session = AgentSession::from_transcript_items(model_context.clone());

    assert_eq!(
        session.model_context().transcript_items(),
        model_context.as_slice()
    );
    assert_eq!(session.last_turn_id(), TurnId(2));
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
        .enqueue_input(AgentInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        })
        .expect("late model completion is valid but stale");
    session.drive();
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
            TranscriptItem::UserMessage("hi".to_string()),
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
fn auto_compaction_requests_remote_compaction_before_releasing_model_request() {
    let mut session = session_with_compactable_history();
    session
        .enqueue_input(AgentInput::follow_up("third user message"))
        .expect("plain follow-up is valid");
    session.drive();

    let (request_id, compaction_context) =
        assert_single_request_compaction(session.drain_actions());
    assert!(compaction_context.transcript_items().iter().any(
        |item| matches!(item, TranscriptItem::UserMessage(text) if text.contains("first user message"))
    ));
    assert!(matches!(
        compaction_context.transcript_items().last(),
        Some(TranscriptItem::UserMessage(text)) if text == "third user message"
    ));

    let events = session.drain_events();
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::TranscriptItemAppended {
            item: TranscriptItem::TurnStarted { turn_id: TurnId(3) },
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ActionRequested {
            action: SessionAction::RequestCompaction { .. }
        }
    )));

    let replacement = compacted_open_context("summary text", 3, "third user message");
    session
        .enqueue_session_input(SessionInput::CompactionCompleted {
            request_id,
            replacement: replacement.clone(),
        })
        .expect("compaction completion should be accepted");

    let request_model_context =
        assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(3));
    assert_eq!(request_model_context, replacement);
    assert_eq!(request_model_context, session.model_context());

    let events = session.drain_events();
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ActionCompleted {
            kind: SessionActionKind::Compaction,
            ..
        }
    )));
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::HistoryCompacted)));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ActionRequested {
            action: SessionAction::RequestModel {
                action_id: ActionId(1),
                turn_id: TurnId(3),
                ..
            }
        }
    )));
}

#[test]
fn requested_compaction_can_run_while_idle() {
    let mut session = session_with_compactable_history();
    session.set_auto_compaction(None);

    session.compact();

    let (request_id, _) = assert_single_request_compaction(session.drain_actions());
    let replacement = compacted_boundary_context("manual summary", 2);
    session
        .enqueue_session_input(SessionInput::CompactionCompleted {
            request_id,
            replacement: replacement.clone(),
        })
        .expect("compaction completion should be accepted");

    assert!(session.drain_actions().is_empty());
    assert_eq!(session.model_context(), replacement);
}

#[test]
fn requested_compaction_request_can_start_behind_undrained_cancel_session_work() {
    let mut session = session_with_compactable_history();
    session.set_auto_compaction(None);

    session
        .enqueue_input(AgentInput::follow_up("third user message"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(3));

    session
        .enqueue_input(AgentInput::Interrupt)
        .expect("interrupt is valid");
    session.drive();

    session.compact();

    let actions = session.drain_actions();
    assert!(matches!(
        actions.as_slice(),
        [
            SessionAction::CancelSessionWork,
            SessionAction::RequestCompaction { .. }
        ]
    ));
}

#[test]
fn pending_idle_compaction_request_blocks_new_turns_until_it_completes() {
    let mut session = session_with_compactable_history();
    session.set_auto_compaction(None);

    session.compact();
    let (request_id, _) = assert_single_request_compaction(session.drain_actions());

    session
        .enqueue_input(AgentInput::follow_up("third user message"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_eq!(session.model_context().last_turn_id(), TurnId(2));
    assert!(session.drain_actions().is_empty());

    session
        .enqueue_session_input(SessionInput::CompactionCompleted {
            request_id,
            replacement: compacted_boundary_context("manual summary", 2),
        })
        .expect("compaction completion should be accepted");
    session.drive();

    let request_context =
        assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(3));
    assert!(matches!(
        request_context.transcript_items().last(),
        Some(TranscriptItem::UserMessage(text)) if text == "third user message"
    ));
}

#[test]
fn queued_steer_and_follow_up_survive_idle_compaction_request() {
    let mut session = session_with_compactable_history();
    session.set_auto_compaction(None);

    session.compact();
    let (request_id, _) = assert_single_request_compaction(session.drain_actions());

    session
        .enqueue_input(AgentInput::follow_up("normal queued work"))
        .expect("plain follow-up is valid");
    session
        .enqueue_input(AgentInput::steer("urgent queued work"))
        .expect("plain steer is valid");
    session.drive();
    assert_eq!(session.model_context().last_turn_id(), TurnId(2));
    assert!(session.drain_actions().is_empty());

    session
        .enqueue_session_input(SessionInput::CompactionCompleted {
            request_id,
            replacement: compacted_boundary_context("compaction request summary", 2),
        })
        .expect("compaction completion should be accepted");
    session.drive();

    let first_request_context =
        assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(3));
    assert!(matches!(
        first_request_context.transcript_items().last(),
        Some(TranscriptItem::UserMessage(text)) if text == "urgent queued work"
    ));

    session
        .enqueue_input(AgentInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(3),
            assistant: AssistantMessage { items: Vec::new() },
        })
        .expect("matching model completion is valid");
    session.drive();

    let second_request_context =
        assert_single_request_model(session.drain_actions(), ActionId(2), TurnId(4));
    assert!(matches!(
        second_request_context.transcript_items().last(),
        Some(TranscriptItem::UserMessage(text)) if text == "normal queued work"
    ));
}

#[test]
fn requested_compaction_waits_for_next_model_context_barrier() {
    let mut session = session_with_compactable_history();
    session.set_auto_compaction(None);
    let tool_call = ToolCall {
        id: ToolCallId(1),
        tool_name: "read".to_string(),
        args_json: "{}".to_string(),
    };

    session
        .enqueue_input(AgentInput::follow_up("third user message"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(3));

    session.compact();
    assert!(session.drain_actions().is_empty());

    session
        .enqueue_input(AgentInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(3),
            assistant: AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            },
        })
        .expect("matching model completion is valid");
    session.drive();
    let actions = session.drain_actions();
    let [SessionAction::RequestTool {
        action_id: tool_action_id,
        turn_id,
        ..
    }] = actions.as_slice()
    else {
        panic!("expected one RequestTool action");
    };
    assert_eq!((*tool_action_id, *turn_id), (ActionId(2), TurnId(3)));

    session
        .enqueue_input(AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(3),
            result: ToolResultMessage {
                tool_call_id: tool_call.id,
                tool_name: tool_call.tool_name.clone(),
                output: "ok".to_string(),
                status: ToolResultStatus::Success,
            },
        })
        .expect("matching tool completion is valid");
    session.drive();

    let (request_id, _) = assert_single_request_compaction(session.drain_actions());
    let replacement = compacted_open_context("barrier summary", 3, "third user message");
    session
        .enqueue_session_input(SessionInput::CompactionCompleted {
            request_id,
            replacement: replacement.clone(),
        })
        .expect("compaction completion should be accepted");

    let request_model_context =
        assert_single_request_model(session.drain_actions(), ActionId(3), TurnId(3));
    assert_eq!(request_model_context, replacement);
}

#[test]
fn requested_model_barrier_compaction_request_rechecks_auto_compaction_before_releasing_model() {
    let mut session = session_with_compactable_history();
    let auto_compaction = session.auto_compaction();
    session.set_auto_compaction(None);
    let tool_call = ToolCall {
        id: ToolCallId(1),
        tool_name: "read".to_string(),
        args_json: "{}".to_string(),
    };

    session
        .enqueue_input(AgentInput::follow_up("third user message"))
        .expect("plain follow-up is valid");
    session.drive();
    assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(3));

    session.compact();
    session.set_auto_compaction(auto_compaction);
    session
        .enqueue_input(AgentInput::ModelCompleted {
            action_id: ActionId(1),
            turn_id: TurnId(3),
            assistant: AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            },
        })
        .expect("matching model completion is valid");
    session.drive();
    assert!(matches!(
        session.drain_actions().as_slice(),
        [SessionAction::RequestTool { .. }]
    ));
    session
        .enqueue_input(AgentInput::ToolCompleted {
            action_id: ActionId(2),
            turn_id: TurnId(3),
            result: ToolResultMessage {
                tool_call_id: tool_call.id,
                tool_name: tool_call.tool_name.clone(),
                output: "ok".to_string(),
                status: ToolResultStatus::Success,
            },
        })
        .expect("matching tool completion is valid");
    session.drive();

    let (request_id, _) = assert_single_request_compaction(session.drain_actions());
    session
        .enqueue_session_input(SessionInput::CompactionCompleted {
            request_id,
            replacement: compacted_open_context(
                "manual summary that is still long enough to trigger auto",
                3,
                "third user message",
            ),
        })
        .expect("requested compaction completion should be accepted");

    let (auto_request_id, _) = assert_single_request_compaction(session.drain_actions());
    let replacement = compacted_open_context("auto summary", 3, "third user message");
    session
        .enqueue_session_input(SessionInput::CompactionCompleted {
            request_id: auto_request_id,
            replacement: replacement.clone(),
        })
        .expect("auto compaction completion should be accepted");

    let request_model_context =
        assert_single_request_model(session.drain_actions(), ActionId(3), TurnId(3));
    assert_eq!(request_model_context, replacement);
}

#[test]
fn failed_compaction_releases_model_request_without_editing_context() {
    let mut session = session_with_compactable_history();
    session
        .enqueue_input(AgentInput::follow_up("third user message"))
        .expect("plain follow-up is valid");
    session.drive();
    let (request_id, before_failure) = assert_single_request_compaction(session.drain_actions());

    session
        .enqueue_session_input(SessionInput::CompactionFailed {
            request_id,
            error: "compact failed".to_string(),
        })
        .expect("compaction failure should be accepted");

    let request_model_context =
        assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(3));
    assert_eq!(request_model_context, before_failure);
    assert!(session.drain_events().iter().any(|event| matches!(
        event,
        SessionEvent::ActionFailed {
            kind: SessionActionKind::Compaction,
            error,
            ..
        } if error == "compact failed"
    )));
}

#[test]
fn stale_compaction_completion_does_not_unblock_pending_compaction() {
    let mut session = session_with_compactable_history();
    session
        .enqueue_input(AgentInput::follow_up("third user message"))
        .expect("plain follow-up is valid");
    session.drive();
    let (request_id, _) = assert_single_request_compaction(session.drain_actions());

    session
        .enqueue_session_input(SessionInput::CompactionCompleted {
            request_id: CompactionRequestId(99),
            replacement: compacted_open_context("wrong", 3, "third user message"),
        })
        .expect("stale compaction completion should be accepted and ignored");
    assert!(session.drain_actions().is_empty());

    let replacement = compacted_open_context("right", 3, "third user message");
    session
        .enqueue_session_input(SessionInput::CompactionCompleted {
            request_id,
            replacement: replacement.clone(),
        })
        .expect("matching compaction completion should be accepted");
    let request_model_context =
        assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(3));
    assert_eq!(request_model_context, replacement);
}

#[test]
fn interrupt_during_compaction_request_cancels_session_work_and_ignores_late_completion() {
    let mut session = session_with_compactable_history();
    session
        .enqueue_input(AgentInput::follow_up("third user message"))
        .expect("plain follow-up is valid");
    session.drive();
    let (request_id, _) = assert_single_request_compaction(session.drain_actions());

    session
        .enqueue_input(AgentInput::Interrupt)
        .expect("interrupt is valid");
    session.drive();

    let actions = session.drain_actions();
    assert_eq!(actions, vec![SessionAction::CancelSessionWork]);
    assert!(session.drain_events().iter().any(|event| matches!(
        event,
        SessionEvent::ActionFailed {
            kind: SessionActionKind::Compaction,
            error,
            ..
        } if error == "interrupted"
    )));

    session
        .enqueue_session_input(SessionInput::CompactionCompleted {
            request_id,
            replacement: compacted_open_context("late summary", 3, "third user message"),
        })
        .expect("late compaction completion should be accepted and ignored");
    assert!(session.drain_actions().is_empty());
}

#[test]
fn late_action_drain_does_not_leave_completed_request_pending() {
    let mut session = AgentSession::new();
    session
        .enqueue_input(AgentInput::follow_up("hi"))
        .expect("plain follow-up is valid");
    session.drive();

    session
        .enqueue_input(AgentInput::ModelCompleted {
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
        .enqueue_input(AgentInput::ModelCompleted {
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
        .enqueue_input(AgentInput::ModelCompleted {
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
            TranscriptItem::UserMessage("first".to_string()),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptItem::TurnStarted { turn_id: TurnId(2) },
            TranscriptItem::UserMessage("new second".to_string()),
        ]
    );

    session
        .enqueue_input(AgentInput::ModelCompleted {
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
fn invalid_origin_tags_are_rejected_without_mutating_session_state() {
    let mut session = AgentSession::new();
    let result = session.enqueue_input(AgentInput::Steer {
        from: Some("parent".to_string()),
        kind: None,
        content: "half tagged".to_string(),
    });

    assert_eq!(result, Err(AgentInputError::UnpairedOriginTags));
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
        id: ToolCallId(1),
        tool_name: "bash".to_string(),
        args_json: "{}".to_string(),
    };
    session
        .enqueue_input(AgentInput::ModelCompleted {
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
fn rewind_accepts_injected_tail_at_turn_boundary() {
    // A context whose leaf is a run of back-to-back injected entries after
    // a TurnFinished is still at a turn boundary.
    let mut session = AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted { turn_id: TurnId(1) },
        TranscriptItem::UserMessage("hi".to_string()),
        TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Graceful,
        },
    ]));
    session
        .transcript_store
        .append_injected(InjectedMessage::new("note", "a"));
    session
        .transcript_store
        .append_injected(InjectedMessage::new("compaction", "b"));
    session
        .transcript_store
        .append_injected(InjectedMessage::new("note", "c"));

    assert!(session.transcript_store().is_turn_boundary());
    session
        .rewind(None)
        .expect("injected tail after a boundary can be edited");
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
        .enqueue_input(AgentInput::ModelCompleted {
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
