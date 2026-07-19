#![forbid(unsafe_code)]

mod workspaces;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_runtime_protocol::{
    read_frame, write_frame, ControlToRuntime, ProjectWorkspace, RuntimeCommand,
    RuntimeCommandError, RuntimeCommandResult, RuntimeHello, RuntimeToControl, SelectedWorkspace,
    HEARTBEAT_INTERVAL_SECS,
};
use agent_tools::{ToolContext, ToolRegistry};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use tokio::time::Duration;
use uuid::Uuid;

use workspaces::{validate_remote_branch, validate_workspace_dir, WorkspaceManager};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    runtime_id: String,
    name: String,
    control_addr: String,
    workspace_root: PathBuf,
}

#[derive(Clone)]
struct Runtime {
    workspaces: WorkspaceManager,
    tools: Arc<ToolRegistry>,
    running: Arc<Mutex<HashMap<String, AbortHandle>>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = load_config()?;
    let runtime = Runtime {
        workspaces: WorkspaceManager::new(config.workspace_root.clone()),
        tools: Arc::new(ToolRegistry::with_builtin_tools()),
        running: Default::default(),
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
                            let result = task_runtime.execute(command).await
                                .map_err(|error| RuntimeCommandError::new("runtime_error", format!("{error:#}")));
                            task_runtime.running.lock().await.remove(&task_id);
                            let _ = sender.send(RuntimeToControl::Result {
                                command_id: task_id,
                                result,
                            }).await;
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
