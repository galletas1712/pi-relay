use agent_session::SessionAction;
use agent_store::{ActionKind, EventType};
use serde_json::json;

use crate::state::{AppState, RunningTask};
use crate::types::DispatchAction;

use super::events::{clear_event_buffer_if_idle, publish_events};
use super::model::run_model_turn;
use super::session_uses_harness;
use super::tasks::{prune_finished_tasks, register_task, unregister_task};
use super::tool::run_tool_turn;

pub(super) fn dispatch_all(state: &AppState, session_id: &str, dispatches: Vec<DispatchAction>) {
    for dispatch in dispatches {
        spawn_dispatch(state.clone(), session_id.to_string(), dispatch);
    }
}

fn spawn_dispatch(state: AppState, session_id: String, dispatch: DispatchAction) {
    if matches!(&dispatch.action, SessionAction::RequestModel { .. }) {
        spawn_model_dispatch(state, session_id, dispatch, false);
    } else {
        spawn_claimed_dispatch(state, session_id, dispatch);
    }
}

pub(super) fn spawn_model_dispatch(
    state: AppState,
    session_id: String,
    dispatch: DispatchAction,
    already_claimed: bool,
) {
    if session_uses_harness(&dispatch.config) {
        return;
    }
    if !already_claimed {
        let state = state.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            let run = state
                .repo
                .claim_pending_model_action(&session_id, &dispatch.row_id, &dispatch.attempt_id)
                .await;
            match run {
                Ok(true) => spawn_model_dispatch(state, session_id, dispatch, true),
                Ok(false) => {}
                Err(error) => eprintln!(
                    "failed to claim model action {session_id}/{}: {error:#}",
                    dispatch.row_id
                ),
            }
        });
        return;
    }
    spawn_claimed_dispatch(state, session_id, dispatch);
}

fn spawn_claimed_dispatch(state: AppState, session_id: String, dispatch: DispatchAction) {
    let event_type = match &dispatch.action {
        SessionAction::RequestModel { .. } => EventType::ModelError,
        SessionAction::RequestTool { .. } => EventType::ToolError,
        SessionAction::CancelSessionWork => return,
    };
    prune_finished_tasks(&state);
    let action_row_id = dispatch.row_id.clone();
    let action_kind = match &dispatch.action {
        SessionAction::RequestModel { .. } => ActionKind::Model,
        SessionAction::RequestTool { .. } => ActionKind::Tool,
        SessionAction::CancelSessionWork => return,
    };
    let task_state = state.clone();
    let task_session_id = session_id.clone();
    let task_action_row_id = action_row_id.clone();
    let handle = tokio::spawn(async move {
        let row_id = dispatch.row_id.clone();
        let result = match dispatch.action.clone() {
            SessionAction::RequestModel { .. } => {
                run_model_turn(task_state.clone(), session_id.clone(), dispatch).await
            }
            SessionAction::RequestTool { .. } => {
                run_tool_turn(task_state.clone(), session_id.clone(), dispatch).await
            }
            SessionAction::CancelSessionWork => Ok(()),
        };
        unregister_task(&task_state, &row_id);
        if let Err(error) = result {
            eprintln!(
                "dispatch task failed {session_id}/{row_id}: {}: {}",
                error.code, error.message
            );
            if let Err(stale_error) = task_state
                .repo
                .mark_action_stale(&session_id, &row_id)
                .await
            {
                eprintln!("failed to mark action stale {session_id}/{row_id}: {stale_error:#}");
            }
            match task_state
                .repo
                .insert_event(
                    &session_id,
                    event_type,
                    json!({
                        "action_row_id": row_id,
                        "error": error.message,
                    }),
                )
                .await
            {
                Ok(event) => {
                    publish_events(&task_state, vec![event]);
                    if let Err(clear_error) =
                        clear_event_buffer_if_idle(&task_state, &session_id).await
                    {
                        eprintln!(
                            "failed to clear idle event buffer {session_id}: {}: {}",
                            clear_error.code, clear_error.message
                        );
                    }
                }
                Err(event_error) => eprintln!(
                    "failed to record dispatch failure event {session_id}/{row_id}: {event_error:#}"
                ),
            }
        }
    });
    register_task(
        &state,
        RunningTask {
            session_id: task_session_id,
            action_row_id: task_action_row_id,
            kind: action_kind,
            handle,
        },
    );
}
