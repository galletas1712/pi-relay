//! Session-authorized, read-only workspace artifact RPCs.

use agent_runtime_protocol::SessionWorkspace as RuntimeWorkspace;
use serde::Deserialize;
use serde_json::Value;

use crate::{state::AppState, types::RpcError};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceParams {
    session_id: String,
    workspace_dir: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileParams {
    session_id: String,
    workspace_dir: String,
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffParams {
    session_id: String,
    workspace_dir: String,
    path: Option<String>,
}

pub(crate) async fn snapshot(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: WorkspaceParams = serde_json::from_value(params)
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))?;
    let config = state.repo.load_session_config(&params.session_id).await?;
    let workspace = workspace(&config, &params.workspace_dir)?;
    let snapshot = state
        .runtime_hosts
        .artifacts_snapshot(&config.runtime_id, &config.workspace_id, workspace)
        .await?;
    Ok(serde_json::to_value(snapshot).map_err(anyhow::Error::from)?)
}

pub(crate) async fn read_file(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: FileParams = serde_json::from_value(params)
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))?;
    let config = state.repo.load_session_config(&params.session_id).await?;
    let workspace = workspace(&config, &params.workspace_dir)?;
    let file = state
        .runtime_hosts
        .artifacts_file(
            &config.runtime_id,
            &config.workspace_id,
            &workspace.workspace_dir,
            &params.path,
        )
        .await?;
    Ok(serde_json::to_value(file).map_err(anyhow::Error::from)?)
}

pub(crate) async fn diff(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let params: DiffParams = serde_json::from_value(params)
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))?;
    let config = state.repo.load_session_config(&params.session_id).await?;
    let workspace = workspace(&config, &params.workspace_dir)?;
    let diff = state
        .runtime_hosts
        .artifacts_diff(
            &config.runtime_id,
            &config.workspace_id,
            workspace,
            params.path,
        )
        .await?;
    Ok(serde_json::to_value(diff).map_err(anyhow::Error::from)?)
}

fn workspace(
    config: &agent_store::SessionConfig,
    workspace_dir: &str,
) -> std::result::Result<RuntimeWorkspace, RpcError> {
    let persisted = config
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_dir == workspace_dir)
        .ok_or_else(|| {
            RpcError::new(
                "workspace_not_found",
                "workspace is not a member of the session",
            )
        })?;
    serde_json::from_value(
        serde_json::to_value(persisted)
            .map_err(|error| RpcError::new("internal_error", error.to_string()))?,
    )
    .map_err(|error| {
        RpcError::new(
            "invalid_session",
            format!("invalid workspace metadata: {error}"),
        )
    })
}
