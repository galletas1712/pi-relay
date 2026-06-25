//! The delegation barrier completion path: shared by the live lifecycle hook
//! (`SessionDriver::try_delegation_barrier`) and the boot crash sweep.
//!
//! ```text
//! on a subagent of delegation D reaching its once-only terminal idle (or boot):
//!   recover each subagent's tail to a turn boundary          # don't miss a crashed final message
//!   once the expected child count has spawned, enqueue at most one
//!     active deterministic parent observation for an unreported
//!     terminal child                                        # parent can steer/cancel remaining children
//!   if any subagent of D is not terminal: return            # barrier not met
//!   finish_delegation CAS (running -> done|done_with_failures)    # single-flight status claim
//!   if the CAS won:
//!     cancel any still-queued stale partial observations
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
//! once.

use agent_store::{Delegation, DelegationStatus, QueuedInputStatus};

use crate::delegation_snapshot::{
    build_delegation_snapshot, completion_wakeup_observation, subagent_wakeup_observation,
};
use crate::handoff::terminal_subagent_status;
use crate::runtime::{publish_events, SessionDriver};
use crate::state::AppState;
use crate::types::RpcError;

/// Complete a delegation iff every subagent is terminal. Called from the live
/// lifecycle hook (a child's terminal idle, with that child's `SessionDriver`
/// lock held up the stack) and from the boot sweep. Tail recovery uses
/// `try_acquire`, so a lock held up the stack is skipped rather than deadlocked.
///
/// Ordering matters:
///   recover each subagent's tail to a boundary  # mid-turn crash either resumes
///                                                # or settles terminal
///   publish at most one active partial wakeup    # parent gets a decision
///                                                # point before fan-out ends
///   if not all terminal: return                 # barrier not met
///   finish_delegation CAS                       # claim terminal status before
///                                                # any normal handoff files
///   if won: cancel stale queued partials        # no running snapshot after
///                                                # terminal completion
///   if won: write handoff files                 # parent wakeup is not queued
///                                                # until these exist
///   enqueue deterministic wakeup observation     # idempotent; boot repair
///                                                # retries after CAS/file gaps
///   drive the parent
pub(crate) async fn complete_delegation_if_ready(
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
    // which the workflow re-runs; a resumable mid-turn subagent keeps the
    // delegation running.
    recover_subagent_tails(state, &delegation).await;

    let all_terminal = state
        .repo
        .delegation_subagents_all_terminal(delegation_id)
        .await?;
    let partial_needs_drive = if all_terminal {
        false
    } else {
        publish_next_terminal_subagent_observation(state, &delegation).await?
    };

    if !all_terminal {
        if partial_needs_drive {
            drive_parent_after_wakeup(state, &delegation.parent_session_id).await;
        }
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

async fn publish_next_terminal_subagent_observation(
    state: &AppState,
    delegation: &Delegation,
) -> std::result::Result<bool, RpcError> {
    if !state
        .repo
        .delegation_spawned_expected_subagents(&delegation.id)
        .await?
    {
        return Ok(false);
    }

    let subagents = state.repo.list_delegation_subagents(&delegation.id).await?;
    let mut terminal_subagent_ids = Vec::new();
    for subagent in &subagents {
        let work_state =
            crate::delegation_tools::load_subagent_work_state(state, &subagent.session_id).await?;
        if work_state.is_completion_terminal() {
            terminal_subagent_ids.push(subagent.session_id.clone());
        }
    }
    if terminal_subagent_ids.is_empty() {
        return Ok(false);
    }

    // Publish only one active partial wakeup per parent decision point. The
    // transactional store insert suppresses ANY queued/consuming partial for
    // this delegation attempt, not only the selected child id, so concurrent
    // terminal children cannot prequeue multiple stale running snapshots.
    for subagent_id in terminal_subagent_ids {
        let client_input_id = delegation_subagent_wakeup_client_input_id(delegation, &subagent_id);
        if let Some(record) = state
            .repo
            .find_client_input(&delegation.parent_session_id, &client_input_id)
            .await?
        {
            if matches!(
                record.status,
                QueuedInputStatus::Queued | QueuedInputStatus::Consuming
            ) {
                return Ok(true);
            }
            continue;
        }
        let snapshot = build_delegation_snapshot(state, delegation).await?;
        let observation = subagent_wakeup_observation(&snapshot, delegation, &subagent_id)?;
        let inserted = state
            .repo
            .enqueue_partial_delegation_observation_if_running(
                &delegation.parent_session_id,
                &delegation.id,
                &delegation.attempt_id,
                &observation,
                &client_input_id,
            )
            .await?;
        return Ok(inserted);
    }
    Ok(false)
}

/// A parent consumed (or otherwise finished acting on) a partial delegation
/// wakeup. If another sibling had already reached terminal while that partial
/// was queued/being handled, publish exactly one next partial now so the parent
/// gets another decision point instead of waiting until final completion or a
/// daemon restart.
pub(crate) async fn publish_next_partial_after_parent_decision(
    state: &AppState,
    parent_session_id: &str,
    consumed_client_input_id: Option<&str>,
) -> std::result::Result<(), RpcError> {
    let Some(client_input_id) = consumed_client_input_id else {
        return Ok(());
    };
    let Some((delegation_id, attempt_id, _subagent_id)) =
        parse_delegation_subagent_wakeup_client_input_id(client_input_id)
    else {
        return Ok(());
    };
    let Some(delegation) = state.repo.get_delegation(delegation_id).await? else {
        return Ok(());
    };
    if delegation.parent_session_id != parent_session_id
        || delegation.attempt_id != attempt_id
        || delegation.status != DelegationStatus::Running
    {
        return Ok(());
    }
    if state
        .repo
        .delegation_subagents_all_terminal(&delegation.id)
        .await?
    {
        return Ok(());
    }
    if publish_next_terminal_subagent_observation(state, &delegation).await? {
        drive_parent_after_wakeup(state, parent_session_id).await;
    }
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
    let events = state
        .repo
        .cancel_queued_partial_delegation_wakeups(
            &delegation.parent_session_id,
            &delegation.id,
            &delegation.attempt_id,
            "delegation_completed",
        )
        .await?;
    publish_events(state, events);

    let wakeup_client_input_id = delegation_wakeup_client_input_id(delegation);
    let mut completed_delegation = delegation.clone();
    completed_delegation.status = status;
    let snapshot = build_delegation_snapshot(state, &completed_delegation).await?;
    let observation = completion_wakeup_observation(&snapshot, &completed_delegation)?;
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

fn parse_delegation_subagent_wakeup_client_input_id(
    client_input_id: &str,
) -> Option<(&str, &str, &str)> {
    let remainder = client_input_id.strip_prefix("delegation-steer:")?;
    let (delegation_id, remainder) = remainder.split_once(':')?;
    let (attempt_id, subagent_id) = remainder.split_once(':')?;
    if delegation_id.is_empty() || attempt_id.is_empty() || subagent_id.is_empty() {
        return None;
    }
    Some((delegation_id, attempt_id, subagent_id))
}

fn delegation_subagent_wakeup_client_input_id(
    delegation: &Delegation,
    subagent_id: &str,
) -> String {
    format!(
        "delegation-steer:{}:{}:{}",
        delegation.id, delegation.attempt_id, subagent_id
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
/// 1. Cancel any active partial wakeups that survived a pre-fix crash after a
///    delegation's status had already reached `cancelled`. This must run before
///    any boot-time path can drive the parent, otherwise a different repaired
///    delegation for the same parent could resume the parent into the stale
///    partial.
/// 2. Complete every `running` delegation whose subagents are all terminal. A
///    crash before the terminal-status claim leaves such a delegation `running`
///    with every subagent idle; the ordinary barrier path handles it.
/// 3. Repair already-completed delegations that may have crashed after the
///    status CAS but before normal handoff files and the parent wakeup
///    observation were published. This repair is idempotent and only covers `done` /
///    `done_with_failures`; cancelled delegations remain transcript-only and
///    are never reactivated.
pub(crate) async fn sweep_running_delegations_on_boot(state: &AppState) {
    repair_cancelled_delegation_partial_wakeups_on_boot(state).await;
    publish_running_delegation_partial_observations_on_boot(state).await;

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

async fn publish_running_delegation_partial_observations_on_boot(state: &AppState) {
    let running = match state.repo.list_running_delegations().await {
        Ok(running) => running,
        Err(error) => {
            eprintln!("boot delegation sweep could not list running delegations: {error:#}");
            return;
        }
    };
    for delegation in running {
        let Ok(all_terminal) = state
            .repo
            .delegation_subagents_all_terminal(&delegation.id)
            .await
        else {
            continue;
        };
        if all_terminal {
            continue;
        }
        match publish_next_terminal_subagent_observation(state, &delegation).await {
            Ok(true) => drive_parent_after_wakeup(state, &delegation.parent_session_id).await,
            Ok(false) => {}
            Err(error) => eprintln!(
                "boot delegation sweep failed to publish partial observation for delegation {}: {}: {}",
                delegation.id, error.code, error.message
            ),
        }
    }
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

async fn repair_cancelled_delegation_partial_wakeups_on_boot(state: &AppState) {
    match state
        .repo
        .repair_cancelled_delegation_partial_wakeups()
        .await
    {
        Ok(events) => publish_events(state, events),
        Err(error) => eprintln!(
            "boot delegation sweep could not repair cancelled delegation partial wakeups: {error:#}"
        ),
    }
}

#[cfg(test)]
#[path = "delegation_runner_tests.rs"]
mod tests;
