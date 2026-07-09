use agent_store::ActionKind;
use std::collections::HashMap;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::state::{AppState, RunningTask, TaskRegistrationId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskRegistrationRejected;

pub(crate) fn abort_session_tasks(state: &AppState, session_id: &str) -> Vec<ActionKind> {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    abort_matching_tasks(&mut tasks, session_id)
}

fn abort_matching_tasks(
    tasks: &mut HashMap<String, RunningTask>,
    session_id: &str,
) -> Vec<ActionKind> {
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

pub(crate) fn register_auxiliary_task(
    state: &AppState,
    handle: JoinHandle<()>,
    start: oneshot::Sender<()>,
) -> Result<(), TaskRegistrationRejected> {
    let _registration_guard = state
        .task_registration_lock
        .lock()
        .expect("task registration lock poisoned");
    if state
        .shutting_down
        .load(std::sync::atomic::Ordering::Acquire)
    {
        handle.abort();
        return Err(TaskRegistrationRejected);
    }
    let mut tasks = state
        .auxiliary_tasks
        .lock()
        .expect("auxiliary task registry lock poisoned");
    tasks.retain(|task| !task.is_finished());
    tasks.push(handle);
    let _ = start.send(());
    Ok(())
}

pub(super) fn register_recovery_task(
    state: &AppState,
    handle: JoinHandle<()>,
    start: oneshot::Sender<()>,
) -> Result<(), TaskRegistrationRejected> {
    let _registration_guard = state
        .task_registration_lock
        .lock()
        .expect("task registration lock poisoned");
    if state
        .shutting_down
        .load(std::sync::atomic::Ordering::Acquire)
    {
        handle.abort();
        return Err(TaskRegistrationRejected);
    }
    *state
        .post_compaction_recovery_task
        .lock()
        .expect("recovery task lock poisoned") = Some(handle);
    let _ = start.send(());
    Ok(())
}

pub(crate) fn take_tasks(state: &AppState) -> Vec<JoinHandle<()>> {
    let _registration_guard = state
        .task_registration_lock
        .lock()
        .expect("task registration lock poisoned");
    state
        .shutting_down
        .store(true, std::sync::atomic::Ordering::Release);
    state.post_compaction_recovery_notify.notify_waiters();
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    let mut handles = tasks
        .drain()
        .map(|(_, task)| task.handle)
        .collect::<Vec<_>>();
    handles.extend(
        state
            .auxiliary_tasks
            .lock()
            .expect("auxiliary task registry lock poisoned")
            .drain(..),
    );
    if let Some(handle) = state
        .post_compaction_recovery_task
        .lock()
        .expect("recovery task lock poisoned")
        .take()
    {
        handles.push(handle);
    }
    handles
}

pub(super) fn is_shutting_down(state: &AppState) -> bool {
    let _registration_guard = state
        .task_registration_lock
        .lock()
        .expect("task registration lock poisoned");
    state
        .shutting_down
        .load(std::sync::atomic::Ordering::Acquire)
}

pub(crate) fn session_has_live_tasks(state: &AppState, session_id: &str) -> bool {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    tasks.retain(|_, task| !task.handle.is_finished());
    tasks.values().any(|task| task.session_id == session_id)
}

pub(crate) fn action_has_live_task_for_lease(
    state: &AppState,
    action_row_id: &str,
    lease: &agent_store::PostCompactionDispatchLease,
) -> bool {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    tasks.retain(|_, task| !task.handle.is_finished());
    tasks
        .get(action_row_id)
        .is_some_and(|task| task_owns_lease(task, lease))
}

pub(crate) fn task_registration_is_live(
    state: &AppState,
    action_row_id: &str,
    registration_id: &TaskRegistrationId,
) -> bool {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    tasks.retain(|_, task| !task.handle.is_finished());
    tasks
        .get(action_row_id)
        .is_some_and(|task| task.registration_id == *registration_id)
}

pub(super) fn register_task(
    state: &AppState,
    task: RunningTask,
    start: oneshot::Sender<()>,
) -> Result<(), TaskRegistrationRejected> {
    let _registration_guard = state
        .task_registration_lock
        .lock()
        .expect("task registration lock poisoned");
    if state
        .shutting_down
        .load(std::sync::atomic::Ordering::Acquire)
    {
        task.handle.abort();
        return Err(TaskRegistrationRejected);
    }
    let replaced = state
        .tasks
        .lock()
        .expect("task registry lock poisoned")
        .insert(task.action_row_id.clone(), task);
    if let Some(replaced) = replaced {
        // Only a superseded post-compaction runner (fenced by lease/generation)
        // is safe to abort here. A non-leased replaced task is an ordinary
        // tool/model runner hit by a re-dispatch race; aborting it mid-run
        // strands its action, because the abort drops the future before the
        // post-`run.await` mark_action_stale in spawn_claimed_dispatch. Drop its
        // handle instead — the task detaches and runs to completion, and the
        // duplicate runner no-ops (its action is already running). This restores
        // the pre-#221 behavior for ordinary dispatch.
        if replaced.post_compaction_dispatch_lease.is_some() {
            replaced.handle.abort();
        }
    }
    let _ = start.send(());
    Ok(())
}

pub(super) fn unregister_task(
    state: &AppState,
    action_row_id: &str,
    registration_id: &TaskRegistrationId,
) -> bool {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    remove_task_if_owner(&mut tasks, action_row_id, registration_id).is_some()
}

pub(super) fn prune_finished_tasks(state: &AppState) {
    let mut tasks = state.tasks.lock().expect("task registry lock poisoned");
    tasks.retain(|_, task| !task.handle.is_finished());
}

fn remove_task_if_owner(
    tasks: &mut HashMap<String, RunningTask>,
    action_row_id: &str,
    registration_id: &TaskRegistrationId,
) -> Option<RunningTask> {
    if tasks
        .get(action_row_id)
        .is_some_and(|task| task.registration_id == *registration_id)
    {
        tasks.remove(action_row_id)
    } else {
        None
    }
}

fn task_owns_lease(task: &RunningTask, lease: &agent_store::PostCompactionDispatchLease) -> bool {
    task.post_compaction_dispatch_lease.as_ref() == Some(lease)
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use super::*;

    #[tokio::test]
    async fn abort_matching_tasks_is_exact_session_scoped() {
        let mut tasks = HashMap::new();
        for (row_id, session_id, kind) in [
            ("parent-action", "parent", ActionKind::Model),
            ("child-a-action", "child-a", ActionKind::Tool),
            ("child-b-action", "child-b", ActionKind::Compaction),
        ] {
            tasks.insert(
                row_id.to_string(),
                RunningTask {
                    session_id: session_id.to_string(),
                    action_row_id: row_id.to_string(),
                    registration_id: TaskRegistrationId::new(),
                    post_compaction_dispatch_lease: None,
                    kind,
                    handle: tokio::spawn(pending()),
                },
            );
        }

        assert_eq!(
            abort_matching_tasks(&mut tasks, "child-a"),
            vec![ActionKind::Tool]
        );
        assert!(!tasks.contains_key("child-a-action"));
        assert!(tasks.contains_key("parent-action"));
        assert!(tasks.contains_key("child-b-action"));

        for (_, task) in tasks {
            task.handle.abort();
        }
    }

    #[tokio::test]
    async fn old_registration_cannot_remove_or_match_replacement() {
        let old_registration = TaskRegistrationId::new();
        let new_registration = TaskRegistrationId::new();
        let old_lease = agent_store::PostCompactionDispatchLease {
            owner_id: "owner-1".to_string(),
            generation: 1,
            context_leaf_id: "leaf".to_string(),
        };
        let new_lease = agent_store::PostCompactionDispatchLease {
            owner_id: "owner-2".to_string(),
            generation: 2,
            context_leaf_id: "leaf".to_string(),
        };
        let old_handle = tokio::spawn(std::future::pending::<()>());
        let new_handle = tokio::spawn(std::future::pending::<()>());
        let mut tasks = HashMap::new();
        tasks.insert(
            "action".to_string(),
            RunningTask {
                session_id: "session".to_string(),
                action_row_id: "action".to_string(),
                registration_id: new_registration.clone(),
                post_compaction_dispatch_lease: Some(new_lease.clone()),
                kind: ActionKind::Model,
                handle: new_handle,
            },
        );

        assert!(remove_task_if_owner(&mut tasks, "action", &old_registration).is_none());
        assert_eq!(
            tasks.get("action").map(|task| &task.registration_id),
            Some(&new_registration)
        );
        assert!(!task_owns_lease(
            tasks.get("action").expect("new registration remains"),
            &old_lease
        ));
        assert!(task_owns_lease(
            tasks.get("action").expect("new registration remains"),
            &new_lease
        ));

        old_handle.abort();
        tasks
            .remove("action")
            .expect("new registration remains")
            .handle
            .abort();
    }
}
