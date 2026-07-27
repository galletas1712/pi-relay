//! Reclaiming a terminal read-only subagent's workspace.
//!
//! A read-only child's cwd is a disposable snapshot, so anything it stages under
//! `.pi-handoff/` only exists until that subvolume is destroyed. Copy-out and
//! teardown are therefore ordered here and nowhere else.

use agent_store::SubagentType;

use crate::handoff::{delegation_dir, safe_handoff_path_segment};
use crate::state::AppState;

/// Hand a terminal read-only subagent's staged artifacts to its parent, then
/// reclaim its workspace.
///
/// Full subagents write into the parent's workspace in place, so they are never
/// copied from and never torn down. Never fails the caller: a failed copy is
/// logged and teardown still runs, so a bad artifact tree can neither block a
/// delegation nor leak a subvolume.
pub(crate) async fn reclaim_read_only_subagent(state: &AppState, session_id: &str) {
    match state.repo.session_subagent_type(session_id).await {
        Ok(Some(SubagentType::ReadOnly)) => {}
        Ok(_) => return,
        Err(error) => {
            eprintln!(
                "failed to load subagent type for workspace teardown {session_id}: {error:#}"
            );
            return;
        }
    }
    if let Err(error) = copy_artifacts_to_parent(state, session_id).await {
        eprintln!("failed to hand back read-only subagent artifacts {session_id}: {error:#}");
    }
    if let Err(error) = state
        .runtime_hosts
        .destroy_session_workspaces(session_id)
        .await
    {
        eprintln!("failed to destroy read-only subagent workspace {session_id}: {error:#}");
    }
}

/// Copy `<child cwd>/.pi-handoff/` into
/// `<parent cwd>/.pi-handoff/<delegation_id>/<child>/artifacts/`, writing an
/// `artifacts.json` manifest beside it so the parent can list the handback after
/// the child's snapshot is gone.
async fn copy_artifacts_to_parent(state: &AppState, session_id: &str) -> anyhow::Result<()> {
    let Some(delegation_id) = state.repo.session_delegation_id(session_id).await? else {
        return Ok(());
    };
    let child = state.repo.load_session_config(session_id).await?;
    let parent_session_id = state
        .repo
        .session_parent_id(session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("read-only subagent {session_id} has no parent"))?;
    let parent = state.repo.load_session_config(&parent_session_id).await?;
    let dir = delegation_dir(&segment(&delegation_id, "delegation_id")?);
    let child_segment = segment(session_id, "subagent_id")?;

    let Some(artifacts) = state
        .runtime_hosts
        .copy_workspace_subtree(
            &child.runtime_id,
            &child.workspace_id,
            crate::handoff::HANDOFF_DIR,
            &parent.workspace_id,
            &format!("{dir}/{child_segment}/artifacts"),
        )
        .await?
    else {
        return Ok(());
    };
    if artifacts.files.is_empty() && artifacts.skipped.is_empty() {
        // The child staged nothing; leave no empty manifest behind.
        return Ok(());
    }
    state
        .runtime_hosts
        .write_workspace_file(
            &parent.runtime_id,
            &parent.workspace_id,
            &format!("{dir}/{child_segment}/artifacts.json"),
            &serde_json::to_string_pretty(&artifacts)?,
        )
        .await?;
    Ok(())
}

fn segment(value: &str, field: &str) -> anyhow::Result<String> {
    safe_handoff_path_segment(value, field)
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))
}
