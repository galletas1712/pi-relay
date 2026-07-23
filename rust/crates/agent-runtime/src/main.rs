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

const PRODUCT_CONFIG_DIR: &str = "pi-relay";
const RUNTIME_CONFIG_DIR: &str = "runtime";
const RUNTIME_CONFIG_FILE: &str = "config.toml";
const MCP_CONFIG_FILE: &str = "mcp.toml";

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
    #[serde(skip)]
    config_root: PathBuf,
    #[serde(skip)]
    home_dir: PathBuf,
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
    // credential store lives alongside the managed workspaces under the
    // configured workspace_root.
    let mcp_path = config.config_root.join(MCP_CONFIG_FILE);
    let mcp = match std::fs::metadata(&mcp_path) {
        Ok(_) => {
            McpManager::start_with_credential_file(
                McpConfig::from_path(&mcp_path)?,
                config.workspace_root.join("mcp-oauth-credentials.json"),
            )
            .await?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => McpManager::disabled(),
        Err(error) => {
            return Err(error).with_context(|| format!("read MCP config {}", mcp_path.display()))
        }
    };
    let runtime = Runtime {
        workspaces: WorkspaceManager::new(
            config.workspace_root.clone(),
            config.config_root.clone(),
            config.home_dir.clone(),
        ),
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
    load_config_from_values(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
        std::env::args().skip(1).collect(),
    )
}

fn load_config_from_values(
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    args: Vec<String>,
) -> Result<Config> {
    if let Some(argument) = args.first() {
        return Err(anyhow!(
            "pi-runtime accepts no arguments; configure it in {RUNTIME_CONFIG_FILE} (unknown argument: {argument})"
        ));
    }
    let config_root = config_root_from_env(xdg_config_home.as_deref(), home.as_deref())?;
    let path = config_root.join(RUNTIME_CONFIG_FILE);
    let mut config: Config = toml::from_str(
        &std::fs::read_to_string(&path)
            .with_context(|| format!("read runtime config {}", path.display()))?,
    )
    .with_context(|| format!("parse runtime config {}", path.display()))?;
    if config.runtime_id.trim().is_empty()
        || config.name.trim().is_empty()
        || config.control_addr.trim().is_empty()
        || !config.workspace_root.is_absolute()
    {
        return Err(anyhow!(
            "runtime_id, name, control_addr, and absolute workspace_root are required"
        ));
    }
    let home_dir = home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| anyhow!("HOME must be an absolute path"))?;
    config.config_root = config_root;
    config.home_dir = home_dir;
    Ok(config)
}

fn config_root_from_env(
    xdg_config_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    if let Some(xdg_config_home) = xdg_config_home.filter(|value| !value.is_empty()) {
        let config_home = PathBuf::from(xdg_config_home);
        if !config_home.is_absolute() {
            return Err(anyhow!("XDG_CONFIG_HOME must be an absolute path"));
        }
        return Ok(config_home
            .join(PRODUCT_CONFIG_DIR)
            .join(RUNTIME_CONFIG_DIR));
    }
    let home = home
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("HOME is required when XDG_CONFIG_HOME is unset"))?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(anyhow!("HOME must be an absolute path"));
    }
    Ok(home
        .join(".config")
        .join(PRODUCT_CONFIG_DIR)
        .join(RUNTIME_CONFIG_DIR))
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
    let (incoming_tx, mut incoming_rx) = mpsc::channel(32);
    let reader_task = tokio::spawn(async move {
        loop {
            match read_frame::<ControlToRuntime>(&mut reader).await {
                Ok(Some(frame)) => {
                    if incoming_tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = incoming_tx.send(Err(error)).await;
                    break;
                }
            }
        }
    });
    let (results_tx, mut results_rx) = mpsc::channel(32);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
    let connected = async {
        loop {
        tokio::select! {
            _ = heartbeat.tick() => write_frame(&mut writer, &RuntimeToControl::Heartbeat).await?,
            result = results_rx.recv() => {
                let Some(result) = result else { break };
                write_frame(&mut writer, &result).await?;
            }
            frame = incoming_rx.recv() => {
                let Some(frame) = frame else { break };
                let frame = frame?;
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
                            let _ = results_tx
                                .send(RuntimeToControl::Result {
                                    command_id,
                                    result: Err(RuntimeCommandError::new(
                                        "runtime_cancelled",
                                        "runtime command cancelled",
                                    )),
                                })
                                .await;
                        }
                    }
                }
            }
        }
        }
        Ok(())
    }
    .await;
    reader_task.abort();
    connected
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
            RuntimeCommand::ReadRuntimeContext {
                workspace_id,
                workspace_dirs,
            } => {
                let context = self
                    .workspaces
                    .read_runtime_context(&workspace_id, &workspace_dirs)
                    .await?;
                Ok(RuntimeCommandResult::RuntimeContext { context })
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
                    .tool_views(&McpSessionSnapshot::new(manifest).map_err(mcp_catalog_wire_error)?)
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

#[cfg(test)]
mod config_tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn loads_strict_runtime_config_from_its_xdg_root() {
        let xdg = make_temp_dir("xdg");
        let config_root = xdg.join(PRODUCT_CONFIG_DIR).join(RUNTIME_CONFIG_DIR);
        fs::create_dir_all(&config_root).expect("config root");
        fs::write(
            config_root.join(RUNTIME_CONFIG_FILE),
            r#"
runtime_id = "runtime-test"
name = "Test runtime"
control_addr = "127.0.0.1:8786"
workspace_root = "/tmp/pi-runtime-test"
"#,
        )
        .expect("runtime config");

        let config = load_config_from_values(
            Some(xdg.as_os_str().to_owned()),
            Some(xdg.as_os_str().to_owned()),
            Vec::new(),
        )
        .expect("load runtime config");
        assert_eq!(config.runtime_id, "runtime-test");
        assert_eq!(config.config_root, config_root);

        fs::write(
            config.config_root.join(RUNTIME_CONFIG_FILE),
            r#"
runtime_id = "runtime-test"
name = "Test runtime"
control_addr = "127.0.0.1:8786"
workspace_root = "/tmp/pi-runtime-test"
mcp_config = "/tmp/mcp.toml"
"#,
        )
        .expect("runtime config with removed field");
        let error = load_config_from_values(
            Some(xdg.as_os_str().to_owned()),
            Some(xdg.as_os_str().to_owned()),
            Vec::new(),
        )
        .expect_err("mcp_config is no longer part of runtime config");
        assert!(format!("{error:#}").contains("unknown field"));

        fs::remove_dir_all(xdg).ok();
    }

    #[test]
    fn runtime_config_root_falls_back_to_home_and_rejects_arguments() {
        assert_eq!(
            config_root_from_env(None, Some("/home/test".as_ref())).expect("config root"),
            PathBuf::from("/home/test/.config/pi-relay/runtime")
        );
        let error = load_config_from_values(
            None,
            Some("/home/test".into()),
            vec!["old-config.toml".to_string()],
        )
        .expect_err("runtime rejects configuration arguments");
        assert!(format!("{error:#}").contains("pi-runtime accepts no arguments"));
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "pi-runtime-config-{prefix}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp directory");
        path
    }
}
