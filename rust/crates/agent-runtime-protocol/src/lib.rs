#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::time::Duration;

use agent_mcp_types::{
    McpAuthServerStatus, McpInventory, McpLogoutResult, McpOAuthLoginStart, McpSessionManifest,
    McpSessionSelection, McpToolView,
};
use agent_tools::ProviderTool;
use agent_vocab::{ProviderKind, ToolCall, ToolResultMessage};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const HEARTBEAT_INTERVAL_SECS: u64 = 10;
pub const HEARTBEAT_TIMEOUT_SECS: u64 = 30;

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
    let length = u32::try_from(bytes.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "runtime protocol frame exceeds the u32 framing range",
        )
    })?;
    writer.write_u32(length).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeToControl {
    Hello(RuntimeHello),
    Heartbeat,
    /// Ephemeral mid-command status for long MaterializeSession work. Does not
    /// complete the waiter; [`Self::Result`] still does.
    Progress {
        command_id: String,
        progress: WorkspaceMaterializeProgress,
    },
    /// Interest-filtered filesystem changes under a watched session cwd.
    BrowseFsChanged {
        workspace_id: String,
        directories: Vec<String>,
        files: Vec<String>,
    },
    Result {
        command_id: String,
        result: Result<RuntimeCommandResult, RuntimeCommandError>,
    },
}

/// Per-workspace status while a session's workspaces are being prepared.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceMaterializeProgress {
    pub workspace_dir: String,
    pub phase: WorkspaceMaterializePhase,
    /// 1-based index in the selected workspace set.
    pub index: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMaterializePhase {
    RefreshingBase,
    Copying,
    BranchOverride,
    Done,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// Command wraps RuntimeCommand, whose MCP variants carry a session manifest;
// the frame is matched once off the wire, so the size spread is acceptable.
#[allow(clippy::large_enum_variant)]
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
    /// Write a control-plane-generated file into the session workspace (e.g.
    /// delegation handoff artifacts) so runtime-side tools can read it.
    /// `rel_path` is relative to the session cwd.
    WriteWorkspaceFile {
        workspace_id: String,
        rel_path: String,
        contents: String,
    },
    /// Read a file from the session workspace back to the control plane.
    ReadWorkspaceFile {
        workspace_id: String,
        rel_path: String,
    },
    /// Shallow, paged listing of one directory under the session cwd.
    BrowseListDir {
        workspace_id: String,
        path: String,
        after_name: Option<String>,
        limit: u32,
    },
    /// Bounded range read of a regular file under the session cwd.
    BrowseReadFile {
        workspace_id: String,
        path: String,
        offset: u64,
        max_bytes: u32,
    },
    /// Replace the watched interest set for this workspace cwd. Empty
    /// directories and files clear interest for the caller; the runtime keeps a
    /// watcher only while some interest remains.
    BrowseWatch {
        workspace_id: String,
        directories: Vec<String>,
        files: Vec<String>,
    },
    /// Repo-wide changed-path status for git workspace roots under the session cwd.
    BrowseGitStatus {
        workspace_id: String,
        against: GitAgainst,
        roots: Vec<GitBrowseRoot>,
    },
    /// Unified diff for one cwd-relative path against HEAD or its branch base.
    BrowseGitDiff {
        workspace_id: String,
        path: String,
        against: GitAgainst,
        roots: Vec<GitBrowseRoot>,
    },
    /// Return runtime-owned instructions and skill packages for a session.
    /// `project_key` selects `$HOME/.agents/projects/<project_key>/skills`
    /// when the session belongs to a project; ephemeral sessions pass `None`.
    ReadRuntimeContext {
        workspace_id: String,
        workspace_dirs: Vec<String>,
        project_key: Option<String>,
    },
    /// Enumerate live MCP servers + tools for an MCP picker. The
    /// control-computed first-party toolsets ride along for name-collision and
    /// token-budget checks.
    McpInventory {
        provider: ProviderKind,
        first_party: HashMap<ProviderKind, Vec<ProviderTool>>,
    },
    /// Author a session MCP manifest against live servers.
    McpSelect {
        selection: McpSessionSelection,
        first_party: HashMap<ProviderKind, Vec<ProviderTool>>,
    },
    /// Execute one MCP tool call. Ships the session manifest so the runtime can
    /// resolve the tool by exposed name and re-validate its contract fingerprint;
    /// the tool_call carries the id/args used to build the result.
    ExecuteMcpTool {
        manifest: McpSessionManifest,
        tool_call: ToolCall,
    },
    /// Per-tool live health for tools.list.
    McpToolViews {
        manifest: McpSessionManifest,
    },
    /// Per-server auth status; backs the mcp.status poll.
    McpAuthStatuses {},
    /// Begin an OAuth login. The loopback callback binds on the runtime host,
    /// which (de-dockerized) is the user's machine and browser-reachable.
    McpBeginLogin {
        server: String,
    },
    /// Complete a login from a browser-delivered callback URL (paste-box fallback).
    McpCompleteLogin {
        server: String,
        login_id: String,
        callback_url: String,
    },
    /// Cancel a pending OAuth login.
    McpCancelLogin {
        server: String,
        login_id: String,
    },
    /// Clear a server's stored OAuth credential.
    McpLogout {
        server: String,
    },
}

impl RuntimeCommand {
    /// How long the daemon waits for this command's result. Materializing a
    /// session clones or fetches every selected repo, so it gets a far larger
    /// budget than the interactive commands sharing the same connection; a
    /// single global budget would let a slow bash tool call hold a turn for as
    /// long as a multi-repo checkout. Matching exhaustively keeps a new variant
    /// a compile error rather than a silent inheritance of the default.
    pub fn timeout(&self) -> Duration {
        match self {
            Self::MaterializeSession { .. } => Duration::from_secs(300),
            Self::ValidateProject { .. }
            | Self::EnsureSession { .. }
            | Self::ForkSession { .. }
            | Self::DestroySession { .. }
            | Self::ReconcileProject { .. }
            | Self::RemoveProject { .. }
            | Self::ExecuteTool { .. }
            | Self::WriteWorkspaceFile { .. }
            | Self::ReadWorkspaceFile { .. }
            | Self::BrowseListDir { .. }
            | Self::BrowseReadFile { .. }
            | Self::BrowseWatch { .. }
            | Self::BrowseGitStatus { .. }
            | Self::BrowseGitDiff { .. }
            | Self::ReadRuntimeContext { .. }
            | Self::McpInventory { .. }
            | Self::McpSelect { .. }
            | Self::ExecuteMcpTool { .. }
            | Self::McpToolViews { .. }
            | Self::McpAuthStatuses {}
            | Self::McpBeginLogin { .. }
            | Self::McpCompleteLogin { .. }
            | Self::McpCancelLogin { .. }
            | Self::McpLogout { .. } => Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeCommandResult {
    Ack,
    Materialized {
        workspaces: Vec<SessionWorkspace>,
    },
    Tool {
        result: ToolResultMessage,
    },
    FileContents {
        contents: Option<String>,
    },
    DirListing {
        path: String,
        entries: Vec<WorkspaceDirEntry>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_name: Option<String>,
    },
    FilePrefix {
        path: String,
        content_base64: String,
        byte_len: u64,
        total_size: u64,
        eof: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        mtime_ms: Option<u64>,
    },
    GitStatus {
        against: GitAgainst,
        roots: Vec<GitStatusRoot>,
    },
    GitDiff {
        path: String,
        against: GitAgainst,
        #[serde(skip_serializing_if = "Option::is_none")]
        comparison: Option<GitComparison>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<GitFileStatus>,
        unified: String,
        binary: bool,
        truncated: bool,
    },
    RuntimeContext {
        context: RuntimeContext,
    },
    McpInventory {
        inventory: McpInventory,
    },
    McpManifest {
        manifest: McpSessionManifest,
    },
    McpToolViews {
        views: Vec<McpToolView>,
    },
    McpAuthStatuses {
        servers: Vec<McpAuthServerStatus>,
    },
    McpLoginStart {
        start: McpOAuthLoginStart,
    },
    McpLogout {
        result: McpLogoutResult,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceDirEntry {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<u64>,
}

/// Comparison target for browse git status / diff.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitAgainst {
    /// Working tree (+ index) vs `HEAD`.
    WorkingTree,
    /// Working tree vs the current branch or PR's merge base.
    Branch,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitBrowseRoot {
    pub workspace_dir: String,
    pub remote_url: String,
    pub remote_branch: String,
    pub local_branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitPullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitComparisonRef {
    pub branch: String,
    pub oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<GitPullRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitComparison {
    pub base: GitComparisonRef,
    pub tip: GitComparisonRef,
    pub merge_base_oid: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitFileStatus {
    Modified,
    Added,
    Deleted,
    Untracked,
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitStatusEntry {
    /// Cwd-relative path (includes `workspace_dir/` prefix).
    pub path: String,
    pub status: GitFileStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitStatusRoot {
    pub workspace_dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<GitComparison>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub entries: Vec<GitStatusEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillKind {
    Skill,
    SubagentRole,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillOrigin {
    HomeGlobal,
    RuntimeWorkflow,
    HomeProject,
    WorkspaceProject,
    RuntimeRole,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum InstructionScope {
    #[default]
    Global,
    Project,
    Workspace,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawInstructionFile {
    pub workspace: Option<String>,
    pub path: String,
    pub contents: String,
    /// Controls prompt heading: global (none), `### Project: …`, or `#### <ws>`.
    #[serde(default)]
    pub scope: InstructionScope,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeContext {
    pub instructions: Vec<RawInstructionFile>,
    pub skills: Vec<RawSkillFile>,
}

/// A raw `SKILL.md` found on the session's runtime. `LoadSkill` returns `path`
/// as an absolute runtime-host path and `contents` as the payload's `content`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawSkillFile {
    pub kind: SkillKind,
    pub origin: SkillOrigin,
    pub workspace: Option<String>,
    pub package_name: String,
    pub path: String,
    pub contents: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeCommandError {
    pub code: String,
    pub message: String,
    #[serde(
        default = "empty_error_data",
        skip_serializing_if = "is_empty_error_data"
    )]
    pub data: serde_json::Value,
}

fn empty_error_data() -> serde_json::Value {
    serde_json::json!({})
}

fn is_empty_error_data(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|object| object.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{
        read_frame, write_frame, InstructionScope, RawInstructionFile, RawSkillFile,
        RuntimeCommand, RuntimeContext, SkillKind, SkillOrigin,
    };
    use std::time::Duration;
    use tokio::io::duplex;

    #[test]
    fn materialize_gets_the_long_timeout_and_ordinary_commands_the_default() {
        assert_eq!(
            RuntimeCommand::MaterializeSession {
                project_id: "project".to_string(),
                workspace_id: "workspace".to_string(),
                project_workspaces: Vec::new(),
                selected_workspaces: Vec::new(),
            }
            .timeout(),
            Duration::from_secs(300)
        );
        assert_eq!(
            RuntimeCommand::ExecuteTool {
                workspace_id: "workspace".to_string(),
                provider: agent_vocab::ProviderKind::Claude,
                tool_call: agent_vocab::ToolCall {
                    id: agent_vocab::ToolCallId::new("call-1"),
                    tool_name: "Bash".to_string(),
                    args_json: "{}".to_string(),
                },
            }
            .timeout(),
            Duration::from_secs(120)
        );
    }

    #[tokio::test]
    async fn round_trips_frames_larger_than_eight_megabytes() {
        let payload = "x".repeat(8 * 1024 * 1024 + 1);
        let (mut writer, mut reader) = duplex(payload.len() + 1024);
        let write_task = tokio::spawn(async move { write_frame(&mut writer, &payload).await });

        let received = read_frame::<String>(&mut reader)
            .await
            .expect("large frame should be readable")
            .expect("writer should send a frame");
        write_task
            .await
            .expect("writer task should finish")
            .expect("large frame should be writable");

        assert_eq!(received.len(), 8 * 1024 * 1024 + 1);
    }

    #[test]
    fn runtime_context_round_trips_typed_skill_origins() {
        let context = RuntimeContext {
            instructions: vec![RawInstructionFile {
                workspace: None,
                path: "/home/test/.agents/AGENTS.md".to_string(),
                contents: "instructions".to_string(),
                scope: InstructionScope::Global,
            }],
            skills: vec![RawSkillFile {
                kind: SkillKind::SubagentRole,
                origin: SkillOrigin::RuntimeRole,
                workspace: None,
                package_name: "reviewer".to_string(),
                path: "/config/runtime/subagent-roles/reviewer/SKILL.md".to_string(),
                contents: "---\nname: reviewer\ndescription: review\n---\n".to_string(),
            }],
        };

        let encoded = serde_json::to_string(&context).expect("serialize");
        let decoded: RuntimeContext = serde_json::from_str(&encoded).expect("deserialize");

        assert_eq!(decoded, context);
    }
}

impl RuntimeCommandError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: empty_error_data(),
        }
    }

    pub fn with_data(
        code: impl Into<String>,
        message: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data,
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
