use agent_session::SessionAction;
use agent_store::{ActionKind, EventType, POST_COMPACTION_DISPATCH_LEASE_DURATION};
use serde_json::json;

use crate::state::{AppState, RunningTask, TaskRegistrationId};
use crate::types::DispatchAction;

use super::events::{clear_event_buffer_if_idle, publish_events};
use super::model::run_model_turn;
use super::session_uses_harness;
use super::tasks::{
    is_shutting_down, prune_finished_tasks, register_task, unregister_task,
    TaskRegistrationRejected,
};
use super::tool::run_tool_turn;

pub(super) async fn dispatch_all(
    state: &AppState,
    session_id: &str,
    dispatches: Vec<DispatchAction>,
) {
    for dispatch in dispatches {
        spawn_dispatch(state.clone(), session_id.to_string(), dispatch).await;
    }
}

#[cfg(test)]
fn runner_starts() -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>> {
    static STARTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
        std::sync::OnceLock::new();
    STARTS.get_or_init(Default::default)
}

#[cfg(test)]
fn record_runner_start(session_id: &str, action: &SessionAction) {
    let kind = match action {
        SessionAction::RequestModel { .. } => "model",
        SessionAction::RequestTool { .. } => "tool",
        SessionAction::CancelSessionWork => "cancel",
    };
    *runner_starts()
        .lock()
        .expect("runner start counter lock poisoned")
        .entry(format!("{session_id}/{kind}"))
        .or_default() += 1;
}

#[cfg(test)]
pub(crate) fn runner_start_count(session_id: &str, kind: &str) -> usize {
    runner_starts()
        .lock()
        .expect("runner start counter lock poisoned")
        .get(&format!("{session_id}/{kind}"))
        .copied()
        .unwrap_or_default()
}

async fn spawn_dispatch(state: AppState, session_id: String, dispatch: DispatchAction) {
    if matches!(&dispatch.action, SessionAction::RequestModel { .. }) {
        let _ = spawn_model_dispatch(state, session_id, dispatch, false).await;
    } else {
        let _ = spawn_claimed_dispatch(state, session_id, dispatch);
    }
}

pub(super) async fn spawn_model_dispatch(
    state: AppState,
    session_id: String,
    dispatch: DispatchAction,
    already_claimed: bool,
) -> Result<Option<TaskRegistrationId>, TaskRegistrationRejected> {
    if session_uses_harness(&dispatch.config) {
        return Ok(None);
    }
    if is_shutting_down(&state) {
        return Err(TaskRegistrationRejected);
    }
    if !already_claimed {
        match state
            .repo
            .claim_pending_model_action(&session_id, &dispatch.row_id, &dispatch.attempt_id)
            .await
        {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(error) => {
                eprintln!(
                    "failed to claim model action {session_id}/{}: {error:#}",
                    dispatch.row_id
                );
                return Ok(None);
            }
        }
    }
    spawn_claimed_dispatch(state, session_id, dispatch).map(Some)
}

fn spawn_claimed_dispatch(
    state: AppState,
    session_id: String,
    dispatch: DispatchAction,
) -> Result<TaskRegistrationId, TaskRegistrationRejected> {
    let event_type = match &dispatch.action {
        SessionAction::RequestModel { .. } => EventType::ModelError,
        SessionAction::RequestTool { .. } => EventType::ToolError,
        SessionAction::CancelSessionWork => unreachable!("cancel work is not dispatched"),
    };
    prune_finished_tasks(&state);
    let action_row_id = dispatch.row_id.clone();
    let action_kind = match &dispatch.action {
        SessionAction::RequestModel { .. } => ActionKind::Model,
        SessionAction::RequestTool { .. } => ActionKind::Tool,
        SessionAction::CancelSessionWork => unreachable!("cancel work is not dispatched"),
    };
    let registration_id = TaskRegistrationId::new();
    let task_registration_id = registration_id.clone();
    let post_compaction_dispatch_lease = dispatch.post_compaction_dispatch_lease.clone();
    let has_post_compaction_lease = post_compaction_dispatch_lease.is_some();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let task_state = state.clone();
    let task_session_id = session_id.clone();
    let task_action_row_id = action_row_id.clone();
    let handle = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        #[cfg(test)]
        record_runner_start(&session_id, &dispatch.action);
        let row_id = dispatch.row_id.clone();
        let attempt_id = dispatch.attempt_id.clone();
        let lease = dispatch.post_compaction_dispatch_lease.clone();
        let heartbeat_interval = post_compaction_heartbeat_interval(&dispatch.config);
        let run = async {
            #[cfg(test)]
            if dispatch
                .config
                .metadata
                .pointer("/fault_injection/pause_model_dispatch_before_provider")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                std::future::pending::<()>().await;
            }
            #[cfg(test)]
            if matches!(&dispatch.action, SessionAction::RequestTool { .. })
                && dispatch
                    .config
                    .metadata
                    .pointer("/fault_injection/pause_tool_dispatch_before_run")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            {
                std::future::pending::<()>().await;
            }
            match dispatch.action.clone() {
                SessionAction::RequestModel { .. } => {
                    run_model_turn(task_state.clone(), session_id.clone(), dispatch).await
                }
                SessionAction::RequestTool { .. } => {
                    run_tool_turn(task_state.clone(), session_id.clone(), dispatch).await
                }
                SessionAction::CancelSessionWork => Ok(()),
            }
        };
        let result = if let Some(lease) = lease.as_ref() {
            let state = task_state.clone();
            let lease = lease.clone();
            let heartbeat_session_id = session_id.clone();
            let heartbeat_row_id = row_id.clone();
            let heartbeat_attempt_id = attempt_id.clone();
            let heartbeat = async move {
                let mut interval = tokio::time::interval(heartbeat_interval);
                interval.tick().await;
                loop {
                    interval.tick().await;
                    match state
                        .repo
                        .renew_post_compaction_dispatch_lease(
                            &heartbeat_session_id,
                            &heartbeat_row_id,
                            &heartbeat_attempt_id,
                            &lease,
                            POST_COMPACTION_DISPATCH_LEASE_DURATION,
                        )
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            state.post_compaction_recovery_notify.notify_one();
                            break;
                        }
                        Err(error) => {
                            eprintln!(
                                "failed to renew post-compaction dispatch lease {heartbeat_session_id}/{heartbeat_row_id}: {error:#}"
                            );
                            state.post_compaction_recovery_notify.notify_one();
                            break;
                        }
                    }
                }
            };
            tokio::pin!(heartbeat);
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => result,
                () = &mut heartbeat => {
                    // A false renewal can mean this same runner just committed
                    // its terminal or reactive-compaction transition. Wake
                    // recovery and stop renewing, but keep awaiting the runner
                    // so it can register the durable follow-up. If ownership
                    // was truly lost, the replacement registration aborts this
                    // task after expiry.
                    run.await
                },
            }
        } else {
            run.await
        };
        if !unregister_task(&task_state, &row_id, &task_registration_id) {
            return;
        }
        if lease.is_some() {
            task_state.post_compaction_recovery_notify.notify_one();
        }
        if let Err(error) = result {
            eprintln!(
                "dispatch task failed {session_id}/{row_id}: {}: {}",
                error.code, error.message
            );
            let marked_stale = match task_state
                .repo
                .mark_action_stale(&session_id, &row_id, &attempt_id, lease.as_ref())
                .await
            {
                Ok(marked_stale) => marked_stale,
                Err(stale_error) => {
                    eprintln!("failed to mark action stale {session_id}/{row_id}: {stale_error:#}");
                    false
                }
            };
            if !marked_stale {
                return;
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
            registration_id: registration_id.clone(),
            post_compaction_dispatch_lease,
            handle,
        },
        start_tx,
    )?;
    if has_post_compaction_lease {
        super::ensure_post_compaction_dispatch_recovery(&state);
    }
    Ok(registration_id)
}

fn post_compaction_heartbeat_interval(_config: &agent_store::SessionConfig) -> std::time::Duration {
    #[cfg(test)]
    if let Some(milliseconds) = _config
        .metadata
        .pointer("/fault_injection/post_compaction_heartbeat_interval_ms")
        .and_then(serde_json::Value::as_u64)
    {
        return std::time::Duration::from_millis(milliseconds.max(1));
    }
    POST_COMPACTION_DISPATCH_LEASE_DURATION / 3
}
