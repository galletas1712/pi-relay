//! Public `workspace.list_dir` / `workspace.read_file` RPC handlers.
//!
//! The browser sends a session ID and cwd-relative paths only. The daemon
//! resolves the session's runtime/workspace authority and forwards confined
//! browse commands to the runtime.

use agent_runtime_protocol::RuntimeCommandResult;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::codec::from_params;
use crate::state::AppState;
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
    max_bytes: Option<u32>,
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
        Err(error) => map_browse_error(error),
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
        Err(error) => map_browse_error(error),
    }
}

fn map_browse_error(error: anyhow::Error) -> Result<Value, RpcError> {
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
    {
        "invalid_params"
    } else if lower.contains("runtime") && lower.contains("unavailable") {
        "runtime_unavailable"
    } else {
        "read_failed"
    };
    Err(RpcError::new(code, message))
}
