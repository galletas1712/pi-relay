#![forbid(unsafe_code)]

mod workspaces;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_mcp::{McpConfig, McpManager};
use agent_mcp_types::{
    McpManagerError, McpOAuthLoginError, McpSessionManifest, McpSessionSnapshot,
    OAuthCredentialStoreError,
};
use agent_runtime_protocol::{
    read_frame, write_frame, ControlToRuntime, ProjectWorkspace, RuntimeCommand,
    RuntimeCommandError, RuntimeCommandResult, RuntimeHello, RuntimeToControl, SelectedWorkspace,
    HEARTBEAT_INTERVAL_SECS,
};
use agent_tools::{ToolContext, ToolRegistry};
use agent_vocab::{ToolCall, ToolResultMessage};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use tokio::time::Duration;
use uuid::Uuid;

use workspaces::{validate_remote_branch, validate_workspace_dir, WorkspaceManager};

/// Carries a pre-shaped RuntimeCommandError through anyhow so the connection
/// loop can put the stable slug on the wire instead of a generic runtime_error.
#[derive(Debug, Clone)]
struct McpWireError(RuntimeCommandError);

impl std::fmt::Display for McpWireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.0.code, self.0.message)
    }
}

impl std::error::Error for McpWireError {}

fn into_runtime_command_error(error: anyhow::Error) -> RuntimeCommandError {
    if let Some(wire) = error.downcast_ref::<McpWireError>() {
        return wire.0.clone();
    }
    RuntimeCommandError::new("runtime_error", format!("{error:#}"))
}

fn mcp_manager_wire_error(error: McpManagerError) -> anyhow::Error {
    anyhow::Error::new(McpWireError(match error {
        McpManagerError::InventoryChanged { current_revision } => RuntimeCommandError::with_data(
            "mcp_inventory_changed",
            "MCP inventory changed; refresh and review the selection",
            json!({ "current_revision": current_revision }),
        ),
        McpManagerError::SelectionInvalid { message } => {
            RuntimeCommandError::new("mcp_selection_invalid", message)
        }
        McpManagerError::Unavailable { server } => RuntimeCommandError::new(
            "mcp_unavailable",
            format!("A selected MCP server is unavailable: {server}"),
        ),
        McpManagerError::CredentialStore(_) => RuntimeCommandError::new(
            "mcp_oauth_credential_store_failed",
            "MCP OAuth credential storage is unavailable",
        ),
        McpManagerError::Catalog(error) => RuntimeCommandError::new(
            "mcp_selection_invalid",
            format!("invalid MCP catalog: {error:#}"),
        ),
    }))
}

fn mcp_catalog_wire_error(error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(McpWireError(RuntimeCommandError::new(
        "mcp_selection_invalid",
        format!("invalid MCP catalog: {error:#}"),
    )))
}

fn mcp_credential_store_wire_error(_error: OAuthCredentialStoreError) -> anyhow::Error {
    anyhow::Error::new(McpWireError(RuntimeCommandError::new(
        "mcp_oauth_credential_store_failed",
        "MCP OAuth credential storage is unavailable",
    )))
}

fn mcp_oauth_wire_error(error: McpOAuthLoginError) -> anyhow::Error {
    let (code, message) = match error {
        McpOAuthLoginError::NotConfigured => (
            "mcp_oauth_not_configured",
            "OAuth login is not configured for this MCP server",
        ),
        McpOAuthLoginError::AlreadyPending => (
            "mcp_oauth_login_already_pending",
            "An OAuth login is already pending for this MCP server",
        ),
        McpOAuthLoginError::NotFound => (
            "mcp_oauth_login_not_found",
            "The MCP OAuth login was not found",
        ),
        McpOAuthLoginError::AlreadyCompleted => (
            "mcp_oauth_login_finished",
            "The MCP OAuth login is no longer pending",
        ),
        McpOAuthLoginError::Cancelled => (
            "mcp_oauth_login_cancelled",
            "The MCP OAuth login was cancelled",
        ),
        McpOAuthLoginError::Expired => ("mcp_oauth_login_expired", "The MCP OAuth login expired"),
        McpOAuthLoginError::CallbackBind => (
            "mcp_oauth_callback_unavailable",
            "The runtime could not start the loopback OAuth callback listener",
        ),
        McpOAuthLoginError::InvalidCallback => (
            "mcp_oauth_callback_invalid",
            "The OAuth callback URL is invalid for this login",
        ),
        McpOAuthLoginError::Provider => (
            "mcp_oauth_provider_error",
            "The authorization server rejected the OAuth login",
        ),
        McpOAuthLoginError::Persistence => (
            "mcp_oauth_credential_store_failed",
            "MCP OAuth credential storage is unavailable",
        ),
        McpOAuthLoginError::Discovery
        | McpOAuthLoginError::Registration
        | McpOAuthLoginError::TokenEndpoint
        | McpOAuthLoginError::Network
        | McpOAuthLoginError::Unavailable
        | McpOAuthLoginError::AuthorizationUrlTooLong => (
            "mcp_oauth_login_failed",
            "The MCP OAuth login could not be completed",
        ),
    };
    anyhow::Error::new(McpWireError(RuntimeCommandError::new(code, message)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    runtime_id: String,
    name: String,
    control_addr: String,
    workspace_root: PathBuf,
    /// Path to this host's MCP server config (mcp.toml). Absent → no MCP.
    #[serde(default)]
    mcp_config: Option<PathBuf>,
}

#[derive(Clone)]
struct Runtime {
    workspaces: WorkspaceManager,
    tools: Arc<ToolRegistry>,
    running: Arc<Mutex<HashMap<String, AbortHandle>>>,
    mcp: Arc<McpManager>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = load_config()?;
    // MCP servers run on the runtime host next to tool execution. The OAuth
    // credential store lives alongside the workspaces (workspace_root), which on
    // a local runtime is the user's ~/.local/state/pi-relay — reusing their
    // existing logins.
    let mcp = match config.mcp_config.as_deref() {
        Some(path) => {
            McpManager::start_with_credential_file(
                McpConfig::from_path(path)?,
                config.workspace_root.join("mcp-oauth-credentials.json"),
            )
            .await?
        }
        None => McpManager::disabled(),
    };
    let runtime = Runtime {
        workspaces: WorkspaceManager::new(config.workspace_root.clone()),
        tools: Arc::new(ToolRegistry::with_builtin_tools()),
        running: Default::default(),
        mcp,
    };
    runtime.workspaces.validate_root().await.with_context(|| {
        format!(
            "workspace_root {} must support btrfs subvolumes",
            config.workspace_root.display()
        )
    })?;
    loop {
        match connect(&config, runtime.clone()).await {
            Ok(()) => eprintln!("control connection closed"),
            Err(error) => eprintln!("control connection failed: {error:#}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn load_config() -> Result<Config> {
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: pi-runtime <config.toml>"))?;
    let config: Config =
        toml::from_str(&std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?)
            .with_context(|| format!("parse {path}"))?;
    if config.runtime_id.trim().is_empty()
        || config.name.trim().is_empty()
        || config.control_addr.trim().is_empty()
        || !config.workspace_root.is_absolute()
    {
        return Err(anyhow!(
            "runtime_id, name, control_addr, and absolute workspace_root are required"
        ));
    }
    Ok(config)
}

async fn connect(config: &Config, runtime: Runtime) -> Result<()> {
    let stream = TcpStream::connect(&config.control_addr).await?;
    let (mut reader, mut writer) = stream.into_split();
    write_frame(
        &mut writer,
        &RuntimeToControl::Hello(RuntimeHello {
            runtime_id: config.runtime_id.clone(),
            name: config.name.clone(),
        }),
    )
    .await?;
    println!(
        "pi-runtime {} connected to {}",
        config.runtime_id, config.control_addr
    );
    let (results_tx, mut results_rx) = mpsc::channel(32);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
    loop {
        tokio::select! {
            _ = heartbeat.tick() => write_frame(&mut writer, &RuntimeToControl::Heartbeat).await?,
            result = results_rx.recv() => {
                let Some(result) = result else { break };
                write_frame(&mut writer, &result).await?;
            }
            frame = read_frame::<ControlToRuntime>(&mut reader) => {
                let Some(frame) = frame? else { break };
                match frame {
                    ControlToRuntime::Command { command_id, command } => {
                        let task_runtime = runtime.clone();
                        let task_id = command_id.clone();
                        let sender = results_tx.clone();
                        // Hold the lock across spawn+insert so the task's own
                        // self-removal can never run before the insert (which
                        // would leak a stale abort handle).
                        let mut running = runtime.running.lock().await;
                        let handle = tokio::spawn(async move {
                            let result = task_runtime
                                .execute(command)
                                .await
                                .map_err(into_runtime_command_error);
                            task_runtime.running.lock().await.remove(&task_id);
                            let _ = sender
                                .send(RuntimeToControl::Result {
                                    command_id: task_id,
                                    result,
                                })
                                .await;
                        });
                        running.insert(command_id, handle.abort_handle());
                    }
                    ControlToRuntime::Cancel { command_id } => {
                        if let Some(handle) = runtime.running.lock().await.remove(&command_id) {
                            handle.abort();
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

impl Runtime {
    async fn execute(&self, command: RuntimeCommand) -> Result<RuntimeCommandResult> {
        match command {
            RuntimeCommand::ValidateProject { workspaces } => {
                validate_project(&workspaces).await?;
                Ok(RuntimeCommandResult::Ack)
            }
            RuntimeCommand::MaterializeSession {
                project_id,
                workspace_id,
                project_workspaces,
                selected_workspaces,
            } => {
                let project_id = Uuid::parse_str(&project_id)?;
                let (_, workspaces) = self
                    .workspaces
                    .materialize_session(
                        project_id,
                        &workspace_id,
                        &project_workspaces,
                        &selected_workspaces
                            .into_iter()
                            .map(Into::into)
                            .collect::<Vec<_>>(),
                    )
                    .await?;
                Ok(RuntimeCommandResult::Materialized { workspaces })
            }
            RuntimeCommand::EnsureSession {
                workspace_id,
                workspaces,
            } => {
                self.workspaces
                    .ensure_session(&workspace_id, &workspaces)
                    .await?;
                Ok(RuntimeCommandResult::Ack)
            }
            RuntimeCommand::ForkSession {
                source_workspace_id,
                target_workspace_id,
                workspaces,
            } => {
                let (_, workspaces) = self
                    .workspaces
                    .fork_session_from_parent(
                        &source_workspace_id,
                        &workspaces,
                        &target_workspace_id,
                    )
                    .await?;
                Ok(RuntimeCommandResult::Materialized { workspaces })
            }
            RuntimeCommand::DestroySession { workspace_id } => {
                self.workspaces
                    .destroy_session_workspaces(&workspace_id)
                    .await?;
                Ok(RuntimeCommandResult::Ack)
            }
            RuntimeCommand::ReconcileProject {
                project_id,
                workspaces,
            } => {
                self.workspaces
                    .reconcile_project_bases(Uuid::parse_str(&project_id)?, &workspaces)
                    .await?;
                Ok(RuntimeCommandResult::Ack)
            }
            RuntimeCommand::RemoveProject { project_id } => {
                self.workspaces
                    .remove_project_bases(Uuid::parse_str(&project_id)?)
                    .await?;
                Ok(RuntimeCommandResult::Ack)
            }
            RuntimeCommand::ExecuteTool {
                workspace_id,
                provider,
                tool_call,
            } => {
                let _guard = self
                    .workspaces
                    .acquire_cwd_mutation_guard(&workspace_id)
                    .await;
                let context = ToolContext::new(self.workspaces.resolve(&workspace_id));
                let result = self.tools.execute(provider, &tool_call, &context).await?;
                Ok(RuntimeCommandResult::Tool { result })
            }
            RuntimeCommand::WriteWorkspaceFile {
                workspace_id,
                rel_path,
                contents,
            } => {
                self.workspaces
                    .write_workspace_file(&workspace_id, &rel_path, &contents)
                    .await?;
                Ok(RuntimeCommandResult::Ack)
            }
            RuntimeCommand::ReadWorkspaceFile {
                workspace_id,
                rel_path,
            } => {
                let contents = self
                    .workspaces
                    .read_workspace_file(&workspace_id, &rel_path)
                    .await?;
                Ok(RuntimeCommandResult::FileContents { contents })
            }
            RuntimeCommand::ReadRuntimeSkills {
                workspace_id,
                workspace_dirs,
            } => {
                let files = self
                    .workspaces
                    .read_runtime_skills(&workspace_id, &workspace_dirs)
                    .await?;
                Ok(RuntimeCommandResult::RuntimeSkills { files })
            }
            RuntimeCommand::McpInventory {
                provider,
                first_party,
            } => Ok(RuntimeCommandResult::McpInventory {
                inventory: self
                    .mcp
                    .inventory(provider, &first_party)
                    .await
                    .map_err(mcp_manager_wire_error)?,
            }),
            RuntimeCommand::McpSelect {
                selection,
                first_party,
            } => {
                let snapshot = self
                    .mcp
                    .select(&selection, &first_party)
                    .await
                    .map_err(mcp_manager_wire_error)?;
                Ok(RuntimeCommandResult::McpManifest {
                    manifest: snapshot.manifest().clone(),
                })
            }
            RuntimeCommand::ExecuteMcpTool {
                manifest,
                tool_call,
            } => Ok(RuntimeCommandResult::Tool {
                result: self.execute_mcp_tool(manifest, tool_call).await,
            }),
            RuntimeCommand::McpToolViews { manifest } => Ok(RuntimeCommandResult::McpToolViews {
                views: self
                    .mcp
                    .tool_views(
                        &McpSessionSnapshot::new(manifest).map_err(mcp_catalog_wire_error)?,
                    )
                    .await,
            }),
            RuntimeCommand::McpAuthStatuses {} => Ok(RuntimeCommandResult::McpAuthStatuses {
                servers: self.mcp.auth_statuses().await,
            }),
            RuntimeCommand::McpBeginLogin { server } => Ok(RuntimeCommandResult::McpLoginStart {
                start: self
                    .mcp
                    .begin_oauth_login(&server)
                    .await
                    .map_err(mcp_oauth_wire_error)?,
            }),
            RuntimeCommand::McpCompleteLogin {
                server,
                login_id,
                callback_url,
            } => {
                self.mcp
                    .complete_oauth_login(&server, &login_id, &callback_url)
                    .await
                    .map_err(mcp_oauth_wire_error)?;
                Ok(RuntimeCommandResult::Ack)
            }
            RuntimeCommand::McpCancelLogin { server, login_id } => {
                self.mcp
                    .cancel_oauth_login(&server, &login_id)
                    .await
                    .map_err(mcp_oauth_wire_error)?;
                Ok(RuntimeCommandResult::Ack)
            }
            RuntimeCommand::McpLogout { server } => Ok(RuntimeCommandResult::McpLogout {
                result: self
                    .mcp
                    .logout_oauth(&server)
                    .await
                    .map_err(mcp_credential_store_wire_error)?,
            }),
        }
    }

    /// Run one MCP tool call and shape it into a ToolResultMessage, mirroring the
    /// former in-process control-plane path (success unless the server reports an
    /// error or dispatch fails).
    async fn execute_mcp_tool(
        &self,
        manifest: McpSessionManifest,
        tool_call: ToolCall,
    ) -> ToolResultMessage {
        let snapshot = match McpSessionSnapshot::new(manifest) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return ToolResultMessage::error(
                    tool_call.id,
                    tool_call.tool_name,
                    format!("invalid MCP manifest: {error:#}"),
                )
            }
        };
        let arguments = serde_json::from_str(&tool_call.args_json).unwrap_or(Value::Null);
        let ToolCall { id, tool_name, .. } = tool_call;
        match self.mcp.call(&snapshot, &tool_name, arguments).await {
            Ok(output) if output.is_error => ToolResultMessage::error(id, tool_name, output.output),
            Ok(output) => ToolResultMessage::success(id, tool_name, output.output),
            Err(error) => ToolResultMessage::error(id, tool_name, error.to_string()),
        }
    }
}

async fn validate_project(workspaces: &[ProjectWorkspace]) -> Result<()> {
    if workspaces.is_empty() {
        return Err(anyhow!("projects require at least one workspace"));
    }
    let mut names = std::collections::BTreeSet::new();
    for workspace in workspaces {
        validate_workspace_dir(&workspace.workspace_dir)?;
        if !names.insert(&workspace.workspace_dir) {
            return Err(anyhow!(
                "duplicate workspace_dir: {}",
                workspace.workspace_dir
            ));
        }
        match workspace.kind {
            agent_runtime_protocol::WorkspaceKind::Git => {
                validate_remote_branch(
                    workspace.remote_url.as_deref().unwrap_or_default(),
                    workspace.remote_branch.as_deref().unwrap_or_default(),
                )
                .await?;
            }
            agent_runtime_protocol::WorkspaceKind::Local => {
                let source = PathBuf::from(
                    workspace
                        .source_path
                        .as_deref()
                        .ok_or_else(|| anyhow!("local workspace source_path is required"))?,
                );
                if !source.is_dir() {
                    return Err(anyhow!(
                        "local workspace source_path is not a directory: {}",
                        source.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

impl From<SelectedWorkspace> for workspaces::SelectedWorkspace {
    fn from(value: SelectedWorkspace) -> Self {
        Self {
            workspace: value.workspace,
            branch_override: value.branch_override,
        }
    }
}
