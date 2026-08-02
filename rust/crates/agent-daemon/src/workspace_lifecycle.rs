use agent_runtime_protocol::WorkspaceMaterializeProgress;
use agent_store::{
    PreparedWorkspace, ProjectWorkspace, SessionConfig, WorkspaceCleanupMode, WorkspaceOwnerKind,
};
use anyhow::{anyhow, Result};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::state::AppState;
use crate::workspace_selection::SelectedWorkspace;

// MaterializeSession has the longest runtime command timeout at 300 seconds.
// Keep the durable reservation through that timeout plus fixed transport grace.
const PROVISIONING_LEASE_SECS: i64 = 310;
const RECONCILE_INTERVAL_SECS: u64 = 2;

async fn reserve(
    state: &AppState,
    owner_session_id: &str,
    runtime_id: &str,
    owner_kind: WorkspaceOwnerKind,
) -> Result<(String, String)> {
    let workspace_id = format!("workspace_{}", Uuid::new_v4());
    let generation = format!("workspace_generation_{}", Uuid::new_v4());
    state
        .repo
        .begin_workspace_provisioning(
            owner_session_id,
            runtime_id,
            &workspace_id,
            &generation,
            owner_kind,
            PROVISIONING_LEASE_SECS,
        )
        .await?;
    Ok((workspace_id, generation))
}

async fn mark_abandoned(
    state: &AppState,
    owner_session_id: &str,
    runtime_id: &str,
    workspace_id: &str,
    generation: &str,
) {
    if let Err(error) = state
        .repo
        .request_workspace_cleanup_exact(
            owner_session_id,
            runtime_id,
            workspace_id,
            generation,
            WorkspaceCleanupMode::DeleteSession,
        )
        .await
    {
        eprintln!("failed to persist cleanup for abandoned workspace {workspace_id}: {error:#}");
    }
}

pub(crate) async fn prepare_materialized(
    state: &AppState,
    owner_session_id: &str,
    runtime_id: &str,
    project_id: Uuid,
    project_workspaces: &[ProjectWorkspace],
    selected: &[SelectedWorkspace],
    on_progress: Option<mpsc::Sender<WorkspaceMaterializeProgress>>,
) -> Result<PreparedWorkspace> {
    let (workspace_id, generation) = reserve(
        state,
        owner_session_id,
        runtime_id,
        WorkspaceOwnerKind::Root,
    )
    .await?;
    let result = state
        .runtime_hosts
        .materialize_session_at(
            runtime_id,
            project_id,
            project_workspaces,
            selected,
            &workspace_id,
            on_progress,
        )
        .await;
    let workspaces = match result {
        Ok(workspaces) => workspaces,
        // A disconnect/timeout does not prove whether the runtime effect took
        // place. Keep the provisioning lease so a late create cannot race an
        // immediate destroy; periodic reconciliation destroys the exact id
        // after the bounded lease.
        Err(error) => return Err(error),
    };
    match state
        .repo
        .finish_workspace_provisioning(
            owner_session_id,
            runtime_id,
            &workspace_id,
            &generation,
            &workspaces,
        )
        .await
    {
        Ok(prepared) => Ok(prepared),
        Err(error) => {
            mark_abandoned(
                state,
                owner_session_id,
                runtime_id,
                &workspace_id,
                &generation,
            )
            .await;
            Err(error)
        }
    }
}

pub(crate) async fn prepare_snapshot(
    state: &AppState,
    owner_session_id: &str,
    owner_kind: WorkspaceOwnerKind,
    source: &SessionConfig,
) -> Result<PreparedWorkspace> {
    if !matches!(
        owner_kind,
        WorkspaceOwnerKind::HistoryFork | WorkspaceOwnerKind::ReadOnly
    ) {
        return Err(anyhow!("workspace snapshot requires a snapshot owner kind"));
    }
    let (workspace_id, generation) =
        reserve(state, owner_session_id, &source.runtime_id, owner_kind).await?;
    let result = state
        .runtime_hosts
        .fork_workspace(
            &source.runtime_id,
            &source.workspace_id,
            &source.workspaces,
            &workspace_id,
        )
        .await;
    let workspaces = match result {
        Ok(workspaces) => workspaces,
        // As above, an ambiguous transport failure retains the bounded lease.
        Err(error) => return Err(error),
    };
    match state
        .repo
        .finish_workspace_provisioning(
            owner_session_id,
            &source.runtime_id,
            &workspace_id,
            &generation,
            &workspaces,
        )
        .await
    {
        Ok(prepared) => Ok(prepared),
        Err(error) => {
            mark_abandoned(
                state,
                owner_session_id,
                &source.runtime_id,
                &workspace_id,
                &generation,
            )
            .await;
            Err(error)
        }
    }
}

pub(crate) async fn abandon_prepared(state: &AppState, workspace: &PreparedWorkspace) {
    if let Err(error) = state.repo.abandon_prepared_workspace(workspace).await {
        eprintln!(
            "failed to persist prepared-workspace cleanup {}: {error:#}",
            workspace.workspace_id
        );
    }
}

/// Persist logical deletion first. The periodic worker owns physical cleanup,
/// so offline runtimes never block cancellation, startup, or an RPC response.
pub(crate) async fn request_session_cleanup(
    state: &AppState,
    session_id: &str,
    mode: WorkspaceCleanupMode,
) -> Result<bool> {
    state
        .repo
        .request_session_workspace_cleanup(session_id, mode)
        .await
}

pub(crate) async fn reconcile(state: &AppState, runtime_id: Option<&str>) -> Result<()> {
    state.repo.reconcile_workspace_resources().await?;
    for resource in state.repo.claim_due_workspace_deletions(runtime_id).await? {
        if let Err(error) = state
            .runtime_hosts
            .destroy_workspace(&resource.runtime_id, &resource.workspace_id)
            .await
        {
            if let Err(record_error) = state
                .repo
                .record_workspace_cleanup_failure(&resource, &format!("{error:#}"))
                .await
            {
                eprintln!(
                    "failed to record workspace cleanup failure {}: {record_error:#}",
                    resource.workspace_id
                );
            }
            continue;
        }
        match state.repo.complete_workspace_cleanup(&resource).await {
            Ok(true) => {}
            Ok(false) => {
                eprintln!(
                    "workspace cleanup generation changed before completion: {}",
                    resource.workspace_id
                );
            }
            Err(error) => {
                eprintln!(
                    "workspace cleanup completion remains pending {}: {error:#}",
                    resource.workspace_id
                );
                if let Err(record_error) = state
                    .repo
                    .record_workspace_cleanup_failure(&resource, &format!("{error:#}"))
                    .await
                {
                    eprintln!(
                        "failed to record workspace completion failure {}: {record_error:#}",
                        resource.workspace_id
                    );
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn spawn_reconciler(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(RECONCILE_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = reconcile(&state, None).await {
                eprintln!("private workspace reconciliation failed: {error:#}");
            }
        }
    });
}
