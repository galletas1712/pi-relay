use agent_store::{EventFrame, SessionActivity};

use crate::state::AppState;
use crate::types::RpcError;

pub(crate) fn publish_events(state: &AppState, events: Vec<EventFrame>) {
    for event in events {
        let _ = state.events.send(event);
    }
}

pub(crate) async fn clear_event_buffer_if_idle(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<(), RpcError> {
    let activity = state.repo.activity(session_id).await?;
    if activity == SessionActivity::Idle {
        state.repo.clear_session_events(session_id).await?;
    }
    Ok(())
}

pub(crate) async fn clear_event_buffer_after_commit(
    state: &AppState,
    session_id: &str,
    operation: &str,
) {
    if let Err(error) = clear_event_buffer_if_idle(state, session_id).await {
        eprintln!(
            "failed to clear event buffer after committed {operation} for session {session_id}: {}: {}",
            error.code, error.message
        );
    }
}
