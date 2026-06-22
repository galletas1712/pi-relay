//! The stage barrier completion path: shared by the live lifecycle hook
//! (`SessionDriver::try_stage_barrier`) and the boot crash sweep.
//!
//! ```text
//! on a subagent of stage S reaching its once-only terminal idle (or boot):
//!   recover each subagent's tail to a turn boundary          # don't miss a crashed final message
//!   if any subagent of S is not terminal: return            # barrier not met
//!   render/refresh handoff dir (index.json + per-subagent md)
//!   finish_stage CAS (running -> done|done_with_failures)    # single-flight stage status + steer
//!   if the CAS won: drive parent to consume the queued steer
//! ```
//!
//! Handoff rendering is an idempotent pure function of durable transcripts and
//! may safely happen before/around the terminal CAS. The single-flight and
//! exactly-once guarantee for terminal stage status and parent steer enqueue is
//! the DB `finish_stage` CAS (stage-row `for update` lock +
//! `status='running'` fence), NOT the in-process `SessionDriver` mutex — so
//! concurrent terminal children and a restart sweep steer the parent exactly
//! once.

use agent_store::{Stage, StageStatus};
use agent_vocab::TurnOutcome;

use crate::handoff::{steer_message, subagent_outcome, write_stage_handoff};
use crate::runtime::SessionDriver;
use crate::state::AppState;
use crate::types::RpcError;

/// Complete a stage iff every subagent is terminal. Called from the live
/// lifecycle hook (a child's terminal idle, with that child's `SessionDriver`
/// lock held up the stack) and from the boot sweep. Tail recovery uses
/// `try_acquire`, so a lock held up the stack is skipped rather than deadlocked.
///
/// Ordering matters:
///   recover each subagent's tail to a boundary  # mid-turn crash either resumes
///                                                # or settles terminal
///   if not all terminal: return                 # barrier not met
///   write handoff files                         # pure fn of the durable
///                                                # transcript; safe before/again
///   finish_stage CAS + steer-enqueue (one tx)   # commit => steer durably queued
///   if won: drive the parent                    # idempotent; durable steer is
///                                                # the crash backstop
pub(crate) async fn complete_stage_if_ready(
    state: &AppState,
    stage_id: &str,
) -> std::result::Result<(), RpcError> {
    let Some(stage) = state.repo.get_stage(stage_id).await? else {
        return Ok(());
    };
    if stage.status != StageStatus::Running {
        return Ok(());
    }

    // Recover each subagent to a turn boundary BEFORE the terminality gate. A
    // subagent that crashed mid-turn (its action stale-marked at boot, no queued
    // input) is NON-terminal by transcript boundary; recovery either drives it
    // forward (re-establishing live work, keeping the stage running) or advances
    // it to a genuine boundary. v1 semantics: a subagent that genuinely cannot
    // continue ends at a boundary classified Crashed -> done_with_failures, which
    // the workflow re-runs; a resumable mid-turn subagent keeps the stage running.
    recover_subagent_tails(state, &stage).await;

    if !state.repo.stage_subagents_all_terminal(stage_id).await? {
        return Ok(());
    }

    let (won_status, ok, failed, failed_ids) = classify_subagents(state, &stage).await?;

    // Render the handoff BEFORE the CAS: it is a pure function of the durable
    // transcript, so a re-run (replay/sweep) safely rewrites identical files.
    let handoff_dir = write_stage_handoff(state, &stage, won_status).await?;
    let message = steer_message(&stage, &handoff_dir, ok, failed, &failed_ids);
    // Deterministic key: a replay or boot sweep re-running finish_stage with the
    // same stage+attempt cannot enqueue a second steer (unique-index no-op).
    let steer_client_input_id = format!("stage-steer:{}:{}", stage.id, stage.attempt_id);

    let won = state
        .repo
        .finish_stage(
            &stage.id,
            &stage.attempt_id,
            won_status,
            &stage.parent_session_id,
            &message,
            &steer_client_input_id,
        )
        .await?;
    if !won {
        // Another terminal child or the boot sweep already won the CAS; that
        // winner enqueued the single steer in its own tx. Nothing more to do.
        return Ok(());
    }

    // The steer is durably queued (committed in the CAS tx). Drive the parent so
    // it consumes the steer promptly; driving is idempotent and replayable, and
    // the durable queued_input is the crash backstop if this drive never runs.
    drive_parent_after_steer(state, &stage.parent_session_id).await;
    Ok(())
}

/// Drive the parent so it picks up the just-queued completion steer. The steer
/// is already durable; this is a best-effort prompt so the parent does not wait
/// for its next external poke. A held-up-the-stack lock is skipped (the parent
/// is not the firing child, but be defensive) and any drive error is logged, not
/// propagated — the durable steer is delivered by the parent's normal recovery.
///
/// Lock ordering note: this acquires the PARENT driver lock while a child driver
/// lock may be held up the stack (the firing child). `try_acquire` makes that a
/// skip rather than a blocking acquire, so the child->parent ordering here cannot
/// deadlock against the parent->child ordering of a spawn.
async fn drive_parent_after_steer(state: &AppState, parent_session_id: &str) {
    let Some(driver) = SessionDriver::try_acquire(state, parent_session_id).await else {
        return;
    };
    if let Err(error) = driver.recover_if_needed().await {
        eprintln!(
            "stage barrier could not recover parent {parent_session_id} after steer: {}: {}",
            error.code, error.message
        );
        return;
    }
    // recover_if_needed only drives a parent that should_continue; an idle parent
    // with the freshly queued steer needs an explicit drive to consume it.
    if let Err(error) = driver.drive_until_blocked().await {
        eprintln!(
            "stage barrier could not drive parent {parent_session_id} after steer: {}: {}",
            error.code, error.message
        );
    }
}

/// Recover each subagent's tail to a turn boundary before the barrier reads its
/// transcript, so a crashed tail does not miss its final assistant message.
/// `try_acquire` skips any subagent whose driver lock is held up the stack (the
/// firing child, already at a boundary) rather than deadlocking on it.
async fn recover_subagent_tails(state: &AppState, stage: &Stage) {
    let subagents = match state.repo.list_stage_subagents(&stage.id).await {
        Ok(subagents) => subagents,
        Err(error) => {
            eprintln!(
                "stage barrier could not list subagents of {}: {error:#}",
                stage.id
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
                "stage barrier could not recover subagent {} of {}: {}: {}",
                subagent.session_id, stage.id, error.code, error.message
            );
        }
    }
}

/// Read every subagent's terminal outcome from its durable transcript and fold
/// it into the stage's won status (`done` vs `done_with_failures`), plus the
/// ok/failed counts and the failed ids for the steer.
async fn classify_subagents(
    state: &AppState,
    stage: &Stage,
) -> std::result::Result<(StageStatus, usize, usize, Vec<String>), RpcError> {
    let subagents = state.repo.list_stage_subagents(&stage.id).await?;
    let mut ok = 0usize;
    let mut failed_ids = Vec::new();
    for subagent in &subagents {
        let history = state.repo.active_branch(&subagent.session_id).await?;
        match subagent_outcome(&history) {
            TurnOutcome::Graceful => ok += 1,
            TurnOutcome::Interrupted | TurnOutcome::Crashed => {
                failed_ids.push(subagent.session_id.clone())
            }
        }
    }
    let failed = failed_ids.len();
    let status = if failed == 0 {
        StageStatus::Done
    } else {
        StageStatus::DoneWithFailures
    };
    Ok((status, ok, failed, failed_ids))
}

/// Boot crash sweep: complete every `running` stage whose subagents are all
/// terminal. A crash mid-barrier leaves such a stage `running` with every
/// subagent idle; idempotent handoff rendering plus the `finish_stage` CAS
/// makes terminal status + steer enqueue happen exactly once even if it raced a
/// live terminal child. This COMPLETES stages — it never stale-marks them.
pub(crate) async fn sweep_running_stages_on_boot(state: &AppState) {
    let ready = match state.repo.sweep_running_stages().await {
        Ok(ready) => ready,
        Err(error) => {
            eprintln!("boot stage sweep could not list running stages: {error:#}");
            return;
        }
    };
    if ready.is_empty() {
        return;
    }
    eprintln!("boot stage sweep completing {} ready stage(s)", ready.len());
    for stage in ready {
        if let Err(error) = complete_stage_if_ready(state, &stage.id).await {
            eprintln!(
                "boot stage sweep failed to complete stage {}: {}: {}",
                stage.id, error.code, error.message
            );
        }
    }
}

#[cfg(test)]
#[path = "stage_runner_tests.rs"]
mod tests;
