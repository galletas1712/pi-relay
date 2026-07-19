#![forbid(unsafe_code)]

use agent_vocab::{ProviderKind, ToolCall, ToolResultMessage};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const HEARTBEAT_INTERVAL_SECS: u64 = 10;
pub const HEARTBEAT_TIMEOUT_SECS: u64 = 30;
pub const COMMAND_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeHello {
    pub runtime_id: String,
    pub name: String,
}

pub async fn read_frame<T: for<'de> Deserialize<'de>>(
    reader: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<Option<T>> {
    let length = match reader.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    };
    if length > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "runtime protocol frame exceeds limit",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub async fn write_frame<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "runtime protocol frame exceeds limit",
        ));
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeToControl {
    Hello(RuntimeHello),
    Heartbeat,
    Result {
        command_id: String,
        result: Result<RuntimeCommandResult, RuntimeCommandError>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlToRuntime {
    Command {
        command_id: String,
        command: RuntimeCommand,
    },
    Cancel {
        command_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeCommand {
    ValidateProject {
        workspaces: Vec<ProjectWorkspace>,
    },
    MaterializeSession {
        project_id: String,
        workspace_id: String,
        project_workspaces: Vec<ProjectWorkspace>,
        selected_workspaces: Vec<SelectedWorkspace>,
    },
    EnsureSession {
        workspace_id: String,
        workspaces: Vec<SessionWorkspace>,
    },
    ForkSession {
        source_workspace_id: String,
        target_workspace_id: String,
        workspaces: Vec<SessionWorkspace>,
    },
    DestroySession {
        workspace_id: String,
    },
    ReconcileProject {
        project_id: String,
        workspaces: Vec<ProjectWorkspace>,
    },
    RemoveProject {
        project_id: String,
    },
    ExecuteTool {
        workspace_id: String,
        provider: ProviderKind,
        tool_call: ToolCall,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeCommandResult {
    Ack,
    Materialized { workspaces: Vec<SessionWorkspace> },
    Tool { result: ToolResultMessage },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeCommandError {
    pub code: String,
    pub message: String,
}

impl RuntimeCommandError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    #[default]
    Git,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectWorkspace {
    #[serde(default)]
    pub kind: WorkspaceKind,
    pub workspace_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

impl ProjectWorkspace {
    pub fn git(
        workspace_dir: impl Into<String>,
        remote_url: impl Into<String>,
        remote_branch: impl Into<String>,
    ) -> Self {
        Self {
            kind: WorkspaceKind::Git,
            workspace_dir: workspace_dir.into(),
            remote_url: Some(remote_url.into()),
            remote_branch: Some(remote_branch.into()),
            source_path: None,
        }
    }

    pub fn local(workspace_dir: impl Into<String>, source_path: impl Into<String>) -> Self {
        Self {
            kind: WorkspaceKind::Local,
            workspace_dir: workspace_dir.into(),
            remote_url: None,
            remote_branch: None,
            source_path: Some(source_path.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionWorkspace {
    #[serde(default)]
    pub kind: WorkspaceKind,
    pub workspace_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_branch: Option<String>,
}

impl SessionWorkspace {
    pub fn git(
        workspace_dir: impl Into<String>,
        remote_url: impl Into<String>,
        remote_branch: impl Into<String>,
        base_sha: impl Into<String>,
        local_branch: impl Into<String>,
    ) -> Self {
        Self {
            kind: WorkspaceKind::Git,
            workspace_dir: workspace_dir.into(),
            remote_url: Some(remote_url.into()),
            remote_branch: Some(remote_branch.into()),
            source_path: None,
            base_sha: Some(base_sha.into()),
            local_branch: Some(local_branch.into()),
        }
    }

    pub fn local(workspace_dir: impl Into<String>, source_path: impl Into<String>) -> Self {
        Self {
            kind: WorkspaceKind::Local,
            workspace_dir: workspace_dir.into(),
            remote_url: None,
            remote_branch: None,
            source_path: Some(source_path.into()),
            base_sha: None,
            local_branch: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SelectedWorkspace {
    pub workspace: ProjectWorkspace,
    pub branch_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub runtime_id: String,
    pub name: String,
    pub online: bool,
    pub last_seen_at: Option<String>,
}
