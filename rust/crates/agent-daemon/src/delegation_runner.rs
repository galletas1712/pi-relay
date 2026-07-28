//! The delegation barrier completion path: shared by the live lifecycle hook
//! (`SessionDriver::try_delegation_barrier`) and the boot crash sweep.
//!
//! ```text
//! on a subagent of delegation D reaching its once-only terminal idle (or boot):
//!   recover each subagent's tail to a turn boundary          # don't miss a crashed final message
//!   if any subagent of D is not terminal: return            # barrier not met
//!   finish_delegation CAS (running -> done|done_with_failures)    # single-flight status claim
//!   if the CAS won:
//!     render/refresh handoff dir (per-subagent md)
//!     enqueue deterministic parent wakeup observation
//!     drive parent to consume the queued wakeup
//! ```
//!
//! The terminal-status CAS runs before normal handoff rendering so a concurrent
//! cancellation cannot win and then be left with normal completed handoff
//! artifacts. The wakeup observation is enqueued only after the files exist; if
//! the daemon crashes after the status CAS but before publishing
//! files/observation, the boot sweep repairs already-completed delegations by
//! re-rendering the handoff and idempotently enqueueing the deterministic
//! wakeup. The single-flight is the DB
//! `finish_delegation` CAS, NOT the in-process `SessionDriver` mutex — so
//! concurrent terminal children and restart repair wake the parent exactly
//! once. An individual child reaching terminal never wakes the parent: a
//! delegation produces exactly one parent wakeup, at terminal status.

use std::cell::RefCell;
use std::collections::HashSet;

use agent_store::{Delegation, DelegationStatus, QueuedInputStatus};

use crate::delegation_snapshot::{build_delegation_snapshot, completion_wakeup_observation};
use crate::handoff::terminal_subagent_status;
use crate::runtime::SessionDriver;
use crate::state::AppState;
use crate::types::RpcError;

tokio::task_local! {
    // Delegation ids whose barrier is already running on THIS task's stack.
    // `run_delegation_barrier` recovers each sibling tail via `recover_if_needed`,
    // whose terminal-idle hook re-fires the same delegation's barrier. Without
    // this guard that recursion nests one call frame per subagent, so a wide
    // delegation overflows the 2 MB tokio worker stack (each frame is a large
    // async state machine). The outermost barrier is the single authority: a
    // re-entry for a delegation already in flight is a no-op, so the sibling
    // sweep costs O(1) stack instead of O(subagents).
    static IN_FLIGHT_BARRIERS: RefCell<HashSet<String>>;
}

/// Recover durable delegation launch work immediately after the global
/// abandoned-action stale mark. A process can die after a child session and
/// initial action commit but before registering its runner; every persisted
/// child index is reconstructed and driven even when the delegation already has
/// its expected child count.
pub(crate) async fn recover_active_delegations_after_stale_mark(state: &AppState) {
    reconcile_partial_launches_on_boot(state).await;

    let running = match state.repo.list_running_delegations().await {
        Ok(running) => running,
        Err(error) => {
            eprintln!("boot child recovery could not list running delegations: {error:#}");
            return;
        }
    };
    for delegation in running {
        let children = match state.repo.delegation_spawned_indices(&delegation.id).await {
            Ok(children) => children,
            Err(error) => {
                eprintln!(
                    "boot child recovery could not inspect delegation {}: {error:#}",
                    delegation.id
                );
                continue;
            }
        };
        for index in 0..delegation.expected_subagents {
            let Some(session_id) = children.get(&index) else {
                eprintln!(
                    "boot child recovery found missing index {index} after materialization delegation={}",
                    delegation.id
                );
                continue;
            };
            let driver = SessionDriver::acquire(state, session_id).await;
            let recovered = async {
                if state
                    .repo
                    .pending_actions_for_dispatch(session_id)
                    .await?
                    .is_empty()
                {
                    driver.recover_if_needed().await?;
                } else {
                    driver.ensure_active_loaded_preserving_open_turn().await?;
                }
                driver.dispatch_ready_actions().await?;
                Ok::<(), RpcError>(())
            }
            .await;
            if let Err(error) = recovered {
                eprintln!(
                    "boot child recovery failed delegation={} session={session_id}: {}: {}",
                    delegation.id, error.code, error.message
                );
            }
        }
    }
}

async fn reconcile_partial_launches_on_boot(state: &AppState) {
    let running = match state.repo.list_running_delegations().await {
        Ok(running) => running,
        Err(error) => {
            eprintln!("boot launch sweep could not list running delegations: {error:#}");
            return;
        }
    };
    for delegation in running {
        let spawned = match state.repo.list_delegation_subagents(&delegation.id).await {
            Ok(spawned) => spawned.len(),
            Err(error) => {
                eprintln!(
                    "boot launch sweep could not inspect delegation {}: {error:#}",
                    delegation.id
                );
                continue;
            }
        };
        if spawned >= delegation.expected_subagents as usize {
            continue;
        }
        if let Err(error) =
            crate::delegation_tools::materialize_delegation_launch(state, &delegation).await
        {
            eprintln!(
                "boot launch recovery failed delegation={}: {}: {}",
                delegation.id, error.code, error.message
            );
        }
    }
}

/// Barrier entry point. Establishes the per-task re-entrancy set on the
/// outermost call, then runs the guarded barrier; nested calls (a sibling tail
/// recovery re-firing this delegation's barrier) reuse the existing set.
pub(crate) async fn complete_delegation_if_ready(
    state: &AppState,
    delegation_id: &str,
) -> std::result::Result<(), RpcError> {
    if IN_FLIGHT_BARRIERS.try_with(|_| ()).is_ok() {
        guard_delegation_barrier(state, delegation_id).await
    } else {
        IN_FLIGHT_BARRIERS
            .scope(
                RefCell::new(HashSet::new()),
                guard_delegation_barrier(state, delegation_id),
            )
            .await
    }
}

/// Deduplicate a barrier for a delegation already being completed up the stack.
/// The outermost call runs the terminality gate after every sibling tail is
/// recovered, so a nested re-entry only needs to return.
async fn guard_delegation_barrier(
    state: &AppState,
    delegation_id: &str,
) -> std::result::Result<(), RpcError> {
    let newly_entered = IN_FLIGHT_BARRIERS
        .with(|in_flight| in_flight.borrow_mut().insert(delegation_id.to_string()));
    if !newly_entered {
        return Ok(());
    }
    let result = run_delegation_barrier(state, delegation_id).await;
    IN_FLIGHT_BARRIERS.with(|in_flight| {
        in_flight.borrow_mut().remove(delegation_id);
    });
    result
}

/// Complete a delegation iff every subagent is terminal. Called from the live
/// lifecycle hook (a child's terminal idle, with that child's `SessionDriver`
/// lock held up the stack) and from the boot sweep. Tail recovery uses
/// `try_acquire`, so a lock held up the stack is skipped rather than deadlocked.
///
/// Ordering matters:
///   recover each subagent's tail to a boundary  # mid-turn crash either resumes
///                                                # or settles terminal
///   if not all terminal: return                 # barrier not met
///   finish_delegation CAS                       # claim terminal status before
///                                                # any normal handoff files
///   if won: write handoff files                 # parent wakeup is not queued
///                                                # until these exist
///   enqueue deterministic wakeup observation     # idempotent; boot repair
///                                                # retries after CAS/file gaps
///   drive the parent
async fn run_delegation_barrier(
    state: &AppState,
    delegation_id: &str,
) -> std::result::Result<(), RpcError> {
    let Some(delegation) = state.repo.get_delegation(delegation_id).await? else {
        return Ok(());
    };
    if delegation.status != DelegationStatus::Running {
        return Ok(());
    }

    // Recover each subagent to a turn boundary BEFORE the terminality gate. A
    // subagent that crashed mid-turn (its action stale-marked at boot, no queued
    // input) is NON-terminal by transcript boundary; recovery either drives it
    // forward (re-establishing live work, keeping the delegation running) or advances
    // it to a genuine boundary. v1 semantics: a subagent that genuinely cannot
    // continue ends at a boundary classified Crashed -> done_with_failures,
    // after which the workflow may choose fresh work; a resumable mid-turn
    // subagent keeps the delegation running.
    recover_subagent_tails(state, &delegation).await;

    if !state
        .repo
        .delegation_subagents_all_terminal(delegation_id)
        .await?
    {
        return Ok(());
    }

    let won_status = classify_subagents(state, &delegation).await?;

    let won = try_claim_and_publish_completed_delegation(state, &delegation, won_status).await?;
    if !won {
        // Another terminal child, the boot sweep, or cancellation already won
        // the status CAS. Only the winner may publish normal handoff files; a
        // cancellation winner must remain transcript-only.
        return Ok(());
    }

    // The wakeup observation is durably queued. Drive the parent so it consumes
    // the wakeup promptly; driving is idempotent and replayable, and the
    // durable queued_input is the crash backstop if this drive never runs.
    drive_parent_after_wakeup(state, &delegation.parent_session_id).await;
    Ok(())
}

pub(crate) async fn try_claim_and_publish_completed_delegation(
    state: &AppState,
    delegation: &Delegation,
    status: DelegationStatus,
) -> std::result::Result<bool, RpcError> {
    let won = state
        .repo
        .finish_delegation(&delegation.id, &delegation.attempt_id, status)
        .await?;
    if !won {
        return Ok(false);
    }
    // Publish normal handoff only after winning the terminal status claim; this
    // keeps a cancelled delegation from getting completed artifacts. If the
    // daemon crashes in this gap, boot repair re-renders completed delegations
    // and enqueues the same deterministic wakeup observation.
    publish_completed_delegation(state, delegation, status).await?;
    Ok(true)
}

/// Render a completed delegation's normal handoff files and then enqueue the
/// deterministic typed parent wakeup observation. This helper is intentionally
/// replayable:
/// `build_delegation_snapshot` refreshes the same handoff artifacts from
/// durable transcripts, and the queued-input client id is keyed by
/// delegation-id+attempt-id so repair cannot double-enqueue.
async fn publish_completed_delegation(
    state: &AppState,
    delegation: &Delegation,
    status: DelegationStatus,
) -> std::result::Result<(), RpcError> {
    let wakeup_client_input_id = delegation_wakeup_client_input_id(delegation);
    let mut completed_delegation = delegation.clone();
    completed_delegation.status = status;
    let snapshot = build_delegation_snapshot(state, &completed_delegation).await?;
    let observation = completion_wakeup_observation(&snapshot)?;
    state
        .repo
        .enqueue_delegation_observation(
            &delegation.parent_session_id,
            &observation,
            &wakeup_client_input_id,
        )
        .await?;
    Ok(())
}

fn delegation_wakeup_client_input_id(delegation: &Delegation) -> String {
    format!(
        "delegation-steer:{}:{}",
        delegation.id, delegation.attempt_id
    )
}

async fn completion_wakeup_needs_drive(
    state: &AppState,
    delegation: &Delegation,
) -> std::result::Result<bool, RpcError> {
    let key = delegation_wakeup_client_input_id(delegation);
    Ok(state
        .repo
        .find_client_input(&delegation.parent_session_id, &key)
        .await?
        .is_some_and(|record| {
            matches!(
                record.status,
                QueuedInputStatus::Queued | QueuedInputStatus::Consuming
            )
        }))
}

/// Drive the parent so it picks up the just-queued completion observation. The observation
/// is already durable; this is a best-effort prompt so the parent does not wait
/// for its next external poke. A held-up-the-stack lock is skipped (the parent
/// is not the firing child, but be defensive) and any drive error is logged, not
/// propagated — the durable observation is delivered by the parent's normal recovery.
///
/// Lock ordering note: this acquires the PARENT driver lock while a child driver
/// lock may be held up the stack (the firing child). `try_acquire` makes that a
/// skip rather than a blocking acquire, so the child->parent ordering here cannot
/// deadlock against the parent->child ordering of a spawn.
async fn drive_parent_after_wakeup(state: &AppState, parent_session_id: &str) {
    let Some(driver) = SessionDriver::try_acquire(state, parent_session_id).await else {
        return;
    };
    if let Err(error) = driver.recover_if_needed().await {
        eprintln!(
            "delegation barrier could not recover parent {parent_session_id} after wakeup: {}: {}",
            error.code, error.message
        );
        return;
    }
    // recover_if_needed only drives a parent that should_continue; an idle parent
    // with the freshly queued wakeup needs an explicit drive to consume it.
    if let Err(error) = driver.drive_until_blocked().await {
        eprintln!(
            "delegation barrier could not drive parent {parent_session_id} after wakeup: {}: {}",
            error.code, error.message
        );
    }
}

/// Recover each subagent's tail to a turn boundary before the barrier reads its
/// transcript, so a crashed tail does not miss its final assistant message.
/// `try_acquire` skips any subagent whose driver lock is held up the stack (the
/// firing child, already at a boundary) rather than deadlocking on it.
async fn recover_subagent_tails(state: &AppState, delegation: &Delegation) {
    let subagents = match state.repo.list_delegation_subagents(&delegation.id).await {
        Ok(subagents) => subagents,
        Err(error) => {
            eprintln!(
                "delegation barrier could not list subagents of {}: {error:#}",
                delegation.id
            );
            return;
        }
    };
    for subagent in subagents {
        let Some(driver) = SessionDriver::try_acquire(state, &subagent.session_id).await else {
            continue;
        };
        if let Err(error) = driver.recover_if_needed().await {
            eprintln!(
                "delegation barrier could not recover subagent {} of {}: {}: {}",
                subagent.session_id, delegation.id, error.code, error.message
            );
        }
    }
}

/// Read every subagent's terminal status from its durable Postgres transcript
/// and fold it into the delegation's won status (`done` vs
/// `done_with_failures`).
async fn classify_subagents(
    state: &AppState,
    delegation: &Delegation,
) -> std::result::Result<DelegationStatus, RpcError> {
    let subagents = state.repo.list_delegation_subagents(&delegation.id).await?;
    for subagent in &subagents {
        let history = state.repo.active_branch(&subagent.session_id).await?;
        if terminal_subagent_status(&history) == Some("failed") {
            return Ok(DelegationStatus::DoneWithFailures);
        }
    }
    Ok(DelegationStatus::Done)
}

/// Boot crash sweep:
///
/// 1. Complete every `running` delegation whose subagents are all terminal. A
///    crash before the terminal-status claim leaves such a delegation `running`
///    with every subagent idle; the ordinary barrier path handles it.
/// 2. Repair already-completed delegations that may have crashed after the
///    status CAS but before normal handoff files and the parent wakeup
///    observation were published. This repair is idempotent and only covers `done` /
///    `done_with_failures`; cancelled delegations remain transcript-only and
///    are never reactivated.
pub(crate) async fn sweep_running_delegations_on_boot(state: &AppState) {
    let ready = match state.repo.sweep_running_delegations().await {
        Ok(ready) => ready,
        Err(error) => {
            eprintln!("boot delegation sweep could not list running delegations: {error:#}");
            Vec::new()
        }
    };
    if !ready.is_empty() {
        eprintln!(
            "boot delegation sweep completing {} ready delegation(s)",
            ready.len()
        );
    }
    for delegation in ready {
        if let Err(error) = complete_delegation_if_ready(state, &delegation.id).await {
            eprintln!(
                "boot delegation sweep failed to complete delegation {}: {}: {}",
                delegation.id, error.code, error.message
            );
        }
    }

    repair_completed_delegation_publications_on_boot(state).await;
}

async fn repair_completed_delegation_publications_on_boot(state: &AppState) {
    let completed = match state.repo.list_completed_delegations_for_repair().await {
        Ok(completed) => completed,
        Err(error) => {
            eprintln!("boot delegation sweep could not list completed delegations: {error:#}");
            return;
        }
    };
    if completed.is_empty() {
        return;
    }
    eprintln!(
        "boot delegation sweep repairing {} completed delegation publication(s)",
        completed.len()
    );
    for delegation in completed {
        let status = delegation.status;
        let Err(error) = repair_completed_delegation_publication(state, &delegation, status).await
        else {
            continue;
        };
        eprintln!(
            "boot delegation sweep failed to repair completed delegation {}: {}: {}",
            delegation.id, error.code, error.message
        );
    }
}

async fn repair_completed_delegation_publication(
    state: &AppState,
    delegation: &Delegation,
    status: DelegationStatus,
) -> std::result::Result<(), RpcError> {
    if !matches!(
        status,
        DelegationStatus::Done | DelegationStatus::DoneWithFailures
    ) {
        return Ok(());
    }
    let classified_status = classify_subagents(state, delegation).await?;
    let status = if classified_status == status {
        status
    } else {
        // The committed DB status is authoritative after the CAS. This mismatch
        // should not happen unless transcripts were manually edited, but repair
        // still publishes an observation payload that matches the terminal row
        // the parent will observe.
        eprintln!(
            "completed delegation {} repair classified {:?} but row is {:?}; using row status",
            delegation.id, classified_status, status
        );
        status
    };
    publish_completed_delegation(state, delegation, status).await?;
    if completion_wakeup_needs_drive(state, delegation).await? {
        drive_parent_after_wakeup(state, &delegation.parent_session_id).await;
    }
    Ok(())
}

#[cfg(test)]
#[path = "delegation_runner_tests.rs"]
mod tests;
