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
use agent_tools::ToolRegistry;
use agent_vocab::{InlineToolResultMessage, ToolCall};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use tokio::time::Duration;
use uuid::Uuid;

use workspaces::{
    validate_remote_branch, validate_workspace_dir, MaterializeProgressSink, WorkspaceManager,
};

const PRODUCT_CONFIG_DIR: &str = "pi-relay";
const RUNTIME_CONFIG_DIR: &str = "runtime";
const RUNTIME_CONFIG_FILE: &str = "config.toml";
const MCP_CONFIG_FILE: &str = "mcp.toml";

/// Carries a pre-shaped RuntimeCommandError through anyhow so the connection
/// loop can put the stable slug on the wire instead of a generic runtime_error.
#[derive(Debug, Clone)]
struct RuntimeWireError(RuntimeCommandError);

impl std::fmt::Display for RuntimeWireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.0.code, self.0.message)
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use agent_vocab::{
        decode_base64_bounded, InlineContentBlock, ProviderKind, ToolCall, ToolCallId,
    };
    use std::fs;

    fn test_runtime(root: &std::path::Path) -> Runtime {
        Runtime {
            workspaces: WorkspaceManager::new(
                root.to_path_buf(),
                root.join("config"),
                root.join("home"),
            )
            .expect("pin disposable workspace root"),
            tools: Arc::new(ToolRegistry::with_builtin_tools()),
            running: Default::default(),
            mcp: McpManager::disabled(),
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pi-runtime-capability-{label}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create disposable runtime root");
        root
    }

    fn read_image_command(workspace_id: &str, path: &str) -> RuntimeCommand {
        RuntimeCommand::ExecuteTool {
            workspace_id: workspace_id.to_string(),
            provider: ProviderKind::OpenAi,
            tool_call: ToolCall {
                id: ToolCallId::new("call_read_image"),
                tool_name: "ReadImage".to_string(),
                args_json: json!({ "path": path }).to_string(),
            },
        }
    }

    fn tiny_png(marker: u8) -> Vec<u8> {
        let mut bytes = vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        bytes.push(marker);
        bytes
    }

    #[test]
    fn missing_workspace_capability_maps_to_a_typed_runtime_error() {
        let root = temp_root("missing");
        let runtime = test_runtime(&root);

        let error = runtime
            .workspaces
            .tool_context("missing-session")
            .map_err(workspace_capability_wire_error)
            .expect_err("missing session cwd must fail capability acquisition");
        let wire = into_runtime_command_error(error);

        assert_eq!(wire.code, "workspace_capability_unavailable");
        assert!(wire.message.contains("session workspace is unavailable"));
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn execute_tool_reads_from_the_verified_managed_cwd() {
        let root = temp_root("normal");
        let runtime = test_runtime(&root);
        let cwd = root.join("sessions/session-normal/cwd");
        fs::create_dir_all(&cwd).expect("create managed cwd");
        let expected = tiny_png(1);
        fs::write(cwd.join("pixel.png"), &expected).expect("write managed image");

        let result = runtime
            .execute(read_image_command("session-normal", "pixel.png"), None)
            .await
            .expect("execute ReadImage");
        let RuntimeCommandResult::Tool { result } = result else {
            panic!("expected tool result");
        };
        let InlineContentBlock::Image { data, .. } = &result.content[1] else {
            panic!("expected image content");
        };
        assert_eq!(
            decode_base64_bounded(data).expect("decode returned image"),
            expected
        );

        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_tool_rejects_replaced_cwd_outside_the_pinned_root() {
        let root = temp_root("outside");
        let runtime = test_runtime(&root);
        let session = root.join("sessions/session-outside");
        let cwd = session.join("cwd");
        fs::create_dir_all(&cwd).expect("create managed cwd");
        fs::rename(&cwd, session.join("displaced-cwd")).expect("displace managed cwd");
        let outside = temp_root("external");
        let external = tiny_png(2);
        fs::write(outside.join("external.png"), &external).expect("write external image");
        std::os::unix::fs::symlink(&outside, &cwd).expect("replace cwd with external symlink");

        let error = runtime
            .execute(read_image_command("session-outside", "external.png"), None)
            .await
            .expect_err("replaced cwd must fail capability acquisition");
        let wire = into_runtime_command_error(error);
        assert_eq!(wire.code, "workspace_capability_unavailable");
        assert!(!wire
            .message
            .contains(&agent_vocab::encode_base64(&external)));

        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(outside).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_tool_rejects_replaced_cwd_inside_the_pinned_root() {
        let root = temp_root("inside");
        let runtime = test_runtime(&root);
        let victim = root.join("sessions/session-victim");
        let cwd = victim.join("cwd");
        fs::create_dir_all(&cwd).expect("create victim cwd");
        fs::rename(&cwd, victim.join("displaced-cwd")).expect("displace victim cwd");
        let other = root.join("sessions/session-other/cwd");
        fs::create_dir_all(&other).expect("create other managed cwd");
        let other_bytes = tiny_png(3);
        fs::write(other.join("other.png"), &other_bytes).expect("write other image");
        std::os::unix::fs::symlink("../session-other/cwd", &cwd)
            .expect("replace cwd with in-root symlink");

        let error = runtime
            .execute(read_image_command("session-victim", "other.png"), None)
            .await
            .expect_err("in-root cwd substitution must fail capability acquisition");
        let wire = into_runtime_command_error(error);
        assert_eq!(wire.code, "workspace_capability_unavailable");
        assert!(!wire
            .message
            .contains(&agent_vocab::encode_base64(&other_bytes)));

        fs::remove_dir_all(root).ok();
    }
}

impl std::error::Error for RuntimeWireError {}

fn workspace_capability_wire_error(error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(RuntimeWireError(RuntimeCommandError::new(
        "workspace_capability_unavailable",
        format!("session workspace is unavailable: {error:#}"),
    )))
}

fn into_runtime_command_error(error: anyhow::Error) -> RuntimeCommandError {
    if let Some(wire) = error.downcast_ref::<RuntimeWireError>() {
        return wire.0.clone();
    }
    RuntimeCommandError::new("runtime_error", format!("{error:#}"))
}

fn mcp_manager_wire_error(error: McpManagerError) -> anyhow::Error {
    anyhow::Error::new(RuntimeWireError(match error {
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
    anyhow::Error::new(RuntimeWireError(RuntimeCommandError::new(
        "mcp_selection_invalid",
        format!("invalid MCP catalog: {error:#}"),
    )))
}

fn mcp_credential_store_wire_error(_error: OAuthCredentialStoreError) -> anyhow::Error {
    anyhow::Error::new(RuntimeWireError(RuntimeCommandError::new(
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
    anyhow::Error::new(RuntimeWireError(RuntimeCommandError::new(code, message)))
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
    let workspaces = WorkspaceManager::new(
        config.workspace_root.clone(),
        config.config_root.clone(),
        config.home_dir.clone(),
    )?;
    workspaces.validate_root().await.with_context(|| {
        format!(
            "workspace_root {} must support btrfs subvolumes",
            config.workspace_root.display()
        )
    })?;
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
        workspaces,
        tools: Arc::new(ToolRegistry::with_builtin_tools()),
        running: Default::default(),
        mcp,
    };
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
                            let (progress_tx, progress_forward) = if matches!(
                                &command,
                                RuntimeCommand::MaterializeSession { .. }
                            ) {
                                let (progress_tx, mut progress_rx) = mpsc::channel(32);
                                let sender = sender.clone();
                                let command_id = task_id.clone();
                                let forward = tokio::spawn(async move {
                                    while let Some(progress) = progress_rx.recv().await {
                                        let _ = sender
                                            .send(RuntimeToControl::Progress {
                                                command_id: command_id.clone(),
                                                progress,
                                            })
                                            .await;
                                    }
                                });
                                (Some(progress_tx), Some(forward))
                            } else {
                                (None, None)
                            };
                            let result = task_runtime
                                .execute(command, progress_tx)
                                .await
                                .map_err(into_runtime_command_error);
                            // Progress sender drops inside execute; await the
                            // forwarder so Progress frames flush before Result.
                            if let Some(forward) = progress_forward {
                                let _ = forward.await;
                            }
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
    async fn execute(
        &self,
        command: RuntimeCommand,
        progress: Option<MaterializeProgressSink>,
    ) -> Result<RuntimeCommandResult> {
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
                        progress,
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
                let _guard = self
                    .workspaces
                    .acquire_cwd_mutation_guard(&source_workspace_id)
                    .await;
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
                let context = self
                    .workspaces
                    .tool_context(&workspace_id)
                    .map_err(workspace_capability_wire_error)?;
                let result = self.tools.execute(provider, &tool_call, &context).await?;
                Ok(RuntimeCommandResult::Tool { result })
            }
            RuntimeCommand::WriteWorkspaceFile {
                workspace_id,
                rel_path,
                contents,
            } => {
                let _guard = self
                    .workspaces
                    .acquire_cwd_mutation_guard(&workspace_id)
                    .await;
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
                project_key,
            } => {
                let context = self
                    .workspaces
                    .read_runtime_context(&workspace_id, &workspace_dirs, project_key.as_deref())
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
            } => {
                let result = self.execute_mcp_tool(manifest, tool_call).await;
                Ok(RuntimeCommandResult::Tool { result })
            }
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

    /// Run one MCP tool call and shape it into a transient inline result, mirroring the
    /// former in-process control-plane path (success unless the server reports an
    /// error or dispatch fails).
    async fn execute_mcp_tool(
        &self,
        manifest: McpSessionManifest,
        tool_call: ToolCall,
    ) -> InlineToolResultMessage {
        let snapshot = match McpSessionSnapshot::new(manifest) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return InlineToolResultMessage::error(
                    tool_call.id,
                    tool_call.tool_name,
                    format!("invalid MCP manifest: {error:#}"),
                )
            }
        };
        let arguments = serde_json::from_str(&tool_call.args_json).unwrap_or(Value::Null);
        let ToolCall { id, tool_name, .. } = tool_call;
        match self.mcp.call(&snapshot, &tool_name, arguments).await {
            Ok(output) if output.is_error => {
                InlineToolResultMessage::error_content(id, tool_name, output.content)
            }
            Ok(output) => InlineToolResultMessage::success_content(id, tool_name, output.content),
            Err(error) => InlineToolResultMessage::error(id, tool_name, error.to_string()),
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
