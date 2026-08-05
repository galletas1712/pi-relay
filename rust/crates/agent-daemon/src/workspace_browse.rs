//! Public `workspace.list_dir` / `workspace.read_file` / `workspace.watch` /
//! `workspace.git_status` / `workspace.git_diff` RPCs.
//!
//! The browser sends a session ID and cwd-relative paths only. The daemon
//! resolves the session's runtime/workspace authority and forwards confined
//! browse commands to the runtime. Watch interest is session-scoped; the
//! runtime receives the union of interests for a shared workspace.

use std::collections::BTreeSet;

use agent_runtime_protocol::{GitAgainst, GitBrowseRoot, RuntimeCommandResult, WorkspaceKind};
use agent_store::{EventFrame, EventType};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::codec::from_params;
use crate::runtime::publish_events;
use crate::state::{AppState, BrowseWatchInterest};
use crate::types::RpcError;

#[derive(Debug, Deserialize)]
struct ListDirParams {
    session_id: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    after_name: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ReadFileParams {
    session_id: String,
    path: String,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    max_bytes: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct WatchParams {
    session_id: String,
    #[serde(default)]
    directories: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GitStatusParams {
    session_id: String,
    against: GitAgainst,
}

#[derive(Debug, Deserialize)]
struct GitDiffParams {
    session_id: String,
    path: String,
    against: GitAgainst,
}

pub(crate) async fn list_dir(state: &AppState, params: Value) -> Result<Value, RpcError> {
    let params: ListDirParams = from_params(params)?;
    let config = state.repo.load_session_config(&params.session_id).await?;
    let limit = params.limit.unwrap_or(0);
    match state
        .runtime_hosts
        .browse_list_dir(
            &config.runtime_id,
            &config.workspace_id,
            &params.path,
            params.after_name.as_deref(),
            limit,
        )
        .await
    {
        Ok(RuntimeCommandResult::DirListing {
            path,
            entries,
            next_after_name,
        }) => {
            let mut body = json!({
                "path": path,
                "entries": entries,
            });
            if let Some(next) = next_after_name {
                body["next_after_name"] = Value::String(next);
            }
            Ok(body)
        }
        Ok(_) => Err(RpcError::new(
            "internal_error",
            "runtime returned the wrong list_dir result",
        )),
        Err(error) => Err(map_browse_error(error)),
    }
}

pub(crate) async fn read_file(state: &AppState, params: Value) -> Result<Value, RpcError> {
    let params: ReadFileParams = from_params(params)?;
    if params.path.is_empty() {
        return Err(RpcError::new(
            "invalid_params",
            "path must name a regular file",
        ));
    }
    let config = state.repo.load_session_config(&params.session_id).await?;
    let max_bytes = params.max_bytes.unwrap_or(0);
    match state
        .runtime_hosts
        .browse_read_file(
            &config.runtime_id,
            &config.workspace_id,
            &params.path,
            params.offset,
            max_bytes,
        )
        .await
    {
        Ok(RuntimeCommandResult::FilePrefix {
            path,
            content_base64,
            byte_len,
            total_size,
            eof,
            mtime_ms,
        }) => {
            let mut body = json!({
                "path": path,
                "content_base64": content_base64,
                "byte_len": byte_len,
                "total_size": total_size,
                "eof": eof,
            });
            if let Some(mtime_ms) = mtime_ms {
                body["mtime_ms"] = json!(mtime_ms);
            }
            Ok(body)
        }
        Ok(_) => Err(RpcError::new(
            "internal_error",
            "runtime returned the wrong read_file result",
        )),
        Err(error) => Err(map_browse_error(error)),
    }
}

pub(crate) async fn watch(state: &AppState, params: Value) -> Result<Value, RpcError> {
    let params: WatchParams = from_params(params)?;
    let config = state.repo.load_session_config(&params.session_id).await?;
    let directories = params.directories.into_iter().collect::<BTreeSet<_>>();
    let files = params.files.into_iter().collect::<BTreeSet<_>>();
    {
        let mut watches = state.browse_watches.lock().await;
        if directories.is_empty() && files.is_empty() {
            watches.remove(&params.session_id);
        } else {
            watches.insert(
                params.session_id.clone(),
                BrowseWatchInterest {
                    workspace_id: config.workspace_id.clone(),
                    directories: directories.clone(),
                    files: files.clone(),
                },
            );
        }
    }
    let (union_dirs, union_files) = union_interest_for_workspace(state, &config.workspace_id).await;
    state
        .runtime_hosts
        .browse_watch(
            &config.runtime_id,
            &config.workspace_id,
            union_dirs,
            union_files,
        )
        .await
        .map_err(map_browse_error)?;
    Ok(json!({ "ok": true }))
}

pub(crate) async fn git_status(state: &AppState, params: Value) -> Result<Value, RpcError> {
    let params: GitStatusParams = from_params(params)?;
    let config = state.repo.load_session_config(&params.session_id).await?;
    let roots = git_browse_roots(&config.workspaces);
    match state
        .runtime_hosts
        .browse_git_status(
            &config.runtime_id,
            &config.workspace_id,
            params.against,
            roots,
        )
        .await
    {
        Ok(RuntimeCommandResult::GitStatus { against, roots }) => Ok(json!({
            "against": against,
            "roots": roots,
        })),
        Ok(_) => Err(RpcError::new(
            "internal_error",
            "runtime returned the wrong git_status result",
        )),
        Err(error) => Err(map_browse_error(error)),
    }
}

pub(crate) async fn git_diff(state: &AppState, params: Value) -> Result<Value, RpcError> {
    let params: GitDiffParams = from_params(params)?;
    if params.path.is_empty() {
        return Err(RpcError::new(
            "invalid_params",
            "path must name a file under a git workspace",
        ));
    }
    let config = state.repo.load_session_config(&params.session_id).await?;
    let roots = git_browse_roots(&config.workspaces);
    match state
        .runtime_hosts
        .browse_git_diff(
            &config.runtime_id,
            &config.workspace_id,
            &params.path,
            params.against,
            roots,
        )
        .await
    {
        Ok(RuntimeCommandResult::GitDiff {
            path,
            against,
            base_oid,
            status,
            unified,
            binary,
            truncated,
        }) => {
            let mut body = json!({
                "path": path,
                "against": against,
                "unified": unified,
                "binary": binary,
                "truncated": truncated,
            });
            if let Some(base_oid) = base_oid {
                body["base_oid"] = Value::String(base_oid);
            }
            if let Some(status) = status {
                body["status"] = json!(status);
            }
            Ok(body)
        }
        Ok(_) => Err(RpcError::new(
            "internal_error",
            "runtime returned the wrong git_diff result",
        )),
        Err(error) => Err(map_browse_error(error)),
    }
}

fn git_browse_roots(workspaces: &[agent_store::SessionWorkspace]) -> Vec<GitBrowseRoot> {
    workspaces
        .iter()
        .filter(|workspace| workspace.kind == WorkspaceKind::Git)
        .filter_map(|workspace| {
            let remote_branch = workspace.remote_branch.as_deref()?.trim();
            if remote_branch.is_empty() {
                return None;
            }
            Some(GitBrowseRoot {
                workspace_dir: workspace.workspace_dir.clone(),
                remote_branch: remote_branch.to_string(),
            })
        })
        .collect()
}

pub(crate) fn publish_browse_fs_changed(
    state: &AppState,
    workspace_id: String,
    directories: Vec<String>,
    files: Vec<String>,
) {
    let state = state.clone();
    tokio::spawn(async move {
        let watches = state.browse_watches.lock().await.clone();
        let mut frames = Vec::new();
        for (session_id, interest) in watches {
            if interest.workspace_id != workspace_id {
                continue;
            }
            let matched_dirs: Vec<_> = directories
                .iter()
                .filter(|path| interest.directories.contains(path.as_str()))
                .cloned()
                .collect();
            let matched_files: Vec<_> = files
                .iter()
                .filter(|path| interest.files.contains(path.as_str()))
                .cloned()
                .collect();
            if matched_dirs.is_empty() && matched_files.is_empty() {
                continue;
            }
            frames.push(EventFrame {
                // Ephemeral: not persisted; clients must not advance high-water on this.
                event_id: 0,
                event: EventType::WorkspaceFsChanged,
                session_id,
                data: json!({
                    "directories": matched_dirs,
                    "files": matched_files,
                }),
            });
        }
        if !frames.is_empty() {
            publish_events(&state, frames);
        }
    });
}

async fn union_interest_for_workspace(
    state: &AppState,
    workspace_id: &str,
) -> (Vec<String>, Vec<String>) {
    let watches = state.browse_watches.lock().await;
    let mut directories = BTreeSet::new();
    let mut files = BTreeSet::new();
    for interest in watches.values() {
        if interest.workspace_id != workspace_id {
            continue;
        }
        directories.extend(interest.directories.iter().cloned());
        files.extend(interest.files.iter().cloned());
    }
    (
        directories.into_iter().collect(),
        files.into_iter().collect(),
    )
}

fn map_browse_error(error: anyhow::Error) -> RpcError {
    let message = format!("{error:#}");
    let lower = message.to_ascii_lowercase();
    let code = if lower.contains("not found") || lower.contains("no such file") {
        "not_found"
    } else if lower.contains("permission denied") {
        "permission_denied"
    } else if lower.contains("invalid")
        || lower.contains("must be")
        || lower.contains("illegal")
        || lower.contains("refusing")
        || lower.contains("reserved")
        || lower.contains("not a regular file")
        || lower.contains("not a directory")
        || lower.contains("more than")
        || lower.contains("not under a git")
    {
        "invalid_params"
    } else if lower.contains("runtime") && lower.contains("unavailable") {
        "runtime_unavailable"
    } else {
        "read_failed"
    };
    RpcError::new(code, message)
}
