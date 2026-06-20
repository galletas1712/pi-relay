use agent_store::ActionKind;
use tokio::task::JoinHandle;

use crate::state::{AppState, RunningTask};

pub(crate) fn abort_session_tasks(state: &AppState, session_id: &str) -> Vec<ActionKind> {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    tasks.retain(|_, task| !task.handle.is_finished());
    let action_row_ids = tasks
        .iter()
        .filter(|(_, task)| task.session_id == session_id)
        .map(|(action_row_id, _)| action_row_id.clone())
        .collect::<Vec<_>>();
    let mut aborted = Vec::new();
    for action_row_id in action_row_ids {
        if let Some(task) = tasks.remove(&action_row_id) {
            aborted.push(task.kind);
            task.handle.abort();
        }
    }
    aborted
}

pub(crate) fn take_tasks(state: &AppState) -> Vec<JoinHandle<()>> {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    tasks.drain().map(|(_, task)| task.handle).collect()
}

pub(crate) fn session_has_live_tasks(state: &AppState, session_id: &str) -> bool {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    tasks.retain(|_, task| !task.handle.is_finished());
    tasks.values().any(|task| task.session_id == session_id)
}

pub(super) fn register_task(state: &AppState, task: RunningTask) {
    state
        .tasks
        .lock()
        .expect("task registry lock poisoned")
        .insert(task.action_row_id.clone(), task);
}

pub(super) fn unregister_task(state: &AppState, action_row_id: &str) {
    state
        .tasks
        .lock()
        .expect("task registry lock poisoned")
        .remove(action_row_id);
}

pub(super) fn prune_finished_tasks(state: &AppState) {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    tasks.retain(|_, task| !task.handle.is_finished());
}
