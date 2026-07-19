use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agent_runtime_protocol::{
    read_frame, write_frame, ControlToRuntime, RuntimeCommand, RuntimeCommandError,
    RuntimeCommandResult, RuntimeHello, RuntimeRecord, RuntimeToControl, SelectedWorkspace,
    COMMAND_TIMEOUT_SECS, HEARTBEAT_TIMEOUT_SECS,
};
use agent_store::PostgresAgentStore;
use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration, Instant};
use uuid::Uuid;

/// Monotonic per-process connection generation. A runtime that drops and
/// reconnects gets a fresh id, so a stale connection's teardown can never evict
/// or fail the work of a newer connection for the same runtime_id.
static CONNECTION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct RuntimeRegistry {
    repo: Arc<PostgresAgentStore>,
    connections: Arc<Mutex<HashMap<String, RuntimeConnection>>>,
    waiters: Arc<Mutex<HashMap<String, Waiter>>>,
}

#[derive(Clone)]
struct RuntimeConnection {
    connection_id: u64,
    name: String,
    sender: mpsc::Sender<ControlToRuntime>,
    last_heartbeat: Instant,
}

struct Waiter {
    connection_id: u64,
    sender: oneshot::Sender<Result<RuntimeCommandResult, RuntimeCommandError>>,
}

impl RuntimeRegistry {
    pub(crate) fn new(repo: Arc<PostgresAgentStore>) -> Self {
        Self {
            repo,
            connections: Default::default(),
            waiters: Default::default(),
        }
    }

    pub(crate) async fn listen(self, bind: String) -> Result<()> {
        let listener = TcpListener::bind(&bind).await?;
        println!("pi-agentd runtime listener on tcp://{bind}");
        loop {
            let (stream, _) = listener.accept().await?;
            let registry = self.clone();
            tokio::spawn(async move {
                if let Err(error) = registry.handle_connection(stream).await {
                    eprintln!("runtime connection failed: {error:#}");
                }
            });
        }
    }

    /// Serve one runtime connection. Generic over the transport so tests can
    /// drive an in-process runtime over an in-memory duplex pipe; production
    /// passes an accepted `TcpStream`.
    pub(crate) async fn handle_connection<S>(&self, stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let hello = timeout(
            Duration::from_secs(10),
            read_frame::<RuntimeToControl>(&mut reader),
        )
        .await
        .context("runtime hello timeout")??
        .ok_or_else(|| anyhow!("runtime disconnected before hello"))?;
        let RuntimeToControl::Hello(RuntimeHello { runtime_id, name }) = hello else {
            return Err(anyhow!("first runtime frame must be hello"));
        };
        if runtime_id.trim().is_empty() || name.trim().is_empty() {
            return Err(anyhow!("runtime id and name must not be blank"));
        }
        let connection_id = CONNECTION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.repo.register_runtime(&runtime_id, &name).await?;
        let (sender, mut receiver) = mpsc::channel(32);
        self.connections.lock().await.insert(
            runtime_id.clone(),
            RuntimeConnection {
                connection_id,
                name,
                sender,
                last_heartbeat: Instant::now(),
            },
        );

        let served = self
            .serve_connection(
                connection_id,
                &runtime_id,
                &mut reader,
                &mut writer,
                &mut receiver,
            )
            .await;
        self.teardown_connection(connection_id, &runtime_id).await;
        served
    }

    async fn serve_connection<R, W>(
        &self,
        connection_id: u64,
        runtime_id: &str,
        reader: &mut R,
        writer: &mut W,
        receiver: &mut mpsc::Receiver<ControlToRuntime>,
    ) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        loop {
            tokio::select! {
                outgoing = receiver.recv() => {
                    let Some(outgoing) = outgoing else { break };
                    write_frame(writer, &outgoing).await?;
                }
                incoming = read_frame::<RuntimeToControl>(reader) => {
                    let Some(incoming) = incoming? else { break };
                    match incoming {
                        RuntimeToControl::Heartbeat => {
                            let mut connections = self.connections.lock().await;
                            if let Some(connection) = connections.get_mut(runtime_id) {
                                if connection.connection_id == connection_id {
                                    connection.last_heartbeat = Instant::now();
                                }
                            }
                            drop(connections);
                            self.repo.runtime_heartbeat(runtime_id).await?;
                        }
                        RuntimeToControl::Result { command_id, result } => {
                            if let Some(waiter) = self.waiters.lock().await.remove(&command_id) {
                                let _ = waiter.sender.send(result);
                            }
                        }
                        RuntimeToControl::Hello(_) => return Err(anyhow!("duplicate runtime hello")),
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(HEARTBEAT_TIMEOUT_SECS)) => {
                    let stale = self.connections.lock().await
                        .get(runtime_id)
                        .map(|connection| connection.connection_id == connection_id
                            && connection.last_heartbeat.elapsed() >= Duration::from_secs(HEARTBEAT_TIMEOUT_SECS))
                        .unwrap_or(true);
                    if stale {
                        return Err(anyhow!("runtime heartbeat timed out"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Remove this connection only if it is still the registered one (a newer
    /// reconnect must survive), and fail every command that was waiting on this
    /// exact connection so callers fail fast instead of blocking to timeout.
    async fn teardown_connection(&self, connection_id: u64, runtime_id: &str) {
        {
            let mut connections = self.connections.lock().await;
            if connections
                .get(runtime_id)
                .is_some_and(|connection| connection.connection_id == connection_id)
            {
                connections.remove(runtime_id);
            }
        }
        let mut waiters = self.waiters.lock().await;
        let orphaned: Vec<String> = waiters
            .iter()
            .filter(|(_, waiter)| waiter.connection_id == connection_id)
            .map(|(command_id, _)| command_id.clone())
            .collect();
        for command_id in orphaned {
            if let Some(waiter) = waiters.remove(&command_id) {
                let _ = waiter.sender.send(Err(RuntimeCommandError::new(
                    "runtime_disconnected",
                    "runtime disconnected while command was running",
                )));
            }
        }
    }

    /// Send a command to an online runtime and await its result. Commands are
    /// request-scoped: an offline runtime is an explicit error rather than a
    /// persisted-and-replayed command (replay double-applies non-idempotent
    /// tool side effects).
    pub(crate) async fn execute(
        &self,
        runtime_id: &str,
        command: RuntimeCommand,
    ) -> Result<RuntimeCommandResult> {
        let command_id = format!("runtime_command_{}", Uuid::new_v4());
        let (tx, rx) = oneshot::channel();
        let (connection_id, sender) = {
            let connections = self.connections.lock().await;
            let connection = connections
                .get(runtime_id)
                .ok_or_else(|| anyhow!("runtime unavailable: {runtime_id}"))?;
            (connection.connection_id, connection.sender.clone())
        };
        self.waiters.lock().await.insert(
            command_id.clone(),
            Waiter {
                connection_id,
                sender: tx,
            },
        );
        if sender
            .send(ControlToRuntime::Command {
                command_id: command_id.clone(),
                command,
            })
            .await
            .is_err()
        {
            self.waiters.lock().await.remove(&command_id);
            return Err(anyhow!("runtime unavailable: {runtime_id}"));
        }
        match timeout(Duration::from_secs(COMMAND_TIMEOUT_SECS), rx).await {
            Ok(Ok(result)) => result.map_err(|error| anyhow!("{}: {}", error.code, error.message)),
            Ok(Err(_)) => Err(anyhow!("runtime disconnected while command was running")),
            Err(_) => {
                self.waiters.lock().await.remove(&command_id);
                Err(anyhow!("runtime command timed out"))
            }
        }
    }

    pub(crate) async fn list(&self) -> Result<Vec<RuntimeRecord>> {
        let online = self.connections.lock().await.clone();
        let mut records = self.repo.list_runtimes().await?;
        for record in &mut records {
            if let Some(connection) = online.get(&record.runtime_id) {
                record.online = true;
                record.name.clone_from(&connection.name);
            }
        }
        Ok(records)
    }

    pub(crate) async fn require_available(&self, runtime_id: &str) -> Result<()> {
        if self.connections.lock().await.contains_key(runtime_id) {
            Ok(())
        } else {
            Err(anyhow!("runtime unavailable: {runtime_id}"))
        }
    }

    /// Materialize a project session's workspaces on `runtime_id`, returning the
    /// generated workspace id and the runtime's resolved workspace list. Shared
    /// by `session_start` and history-fork tests.
    pub(crate) async fn materialize_session(
        &self,
        runtime_id: &str,
        project_id: Uuid,
        project_workspaces: &[agent_store::ProjectWorkspace],
        selected: &[crate::workspace_selection::SelectedWorkspace],
    ) -> Result<(String, Vec<agent_store::SessionWorkspace>)> {
        let workspace_id = format!("workspace_{}", Uuid::new_v4());
        let result = self
            .execute(
                runtime_id,
                RuntimeCommand::MaterializeSession {
                    project_id: project_id.to_string(),
                    workspace_id: workspace_id.clone(),
                    project_workspaces: project_workspaces.to_vec(),
                    selected_workspaces: selected
                        .iter()
                        .map(|selected| SelectedWorkspace {
                            workspace: selected.workspace.clone(),
                            branch_override: selected.branch_override.clone(),
                        })
                        .collect(),
                },
            )
            .await?;
        let RuntimeCommandResult::Materialized { workspaces } = result else {
            return Err(anyhow!("runtime returned the wrong materialization result"));
        };
        Ok((workspace_id, workspaces))
    }

    pub(crate) async fn ensure_for_runtime(
        &self,
        runtime_id: &str,
        workspace_id: &str,
        workspaces: &[agent_store::SessionWorkspace],
    ) -> Result<()> {
        self.execute(
            runtime_id,
            RuntimeCommand::EnsureSession {
                workspace_id: workspace_id.to_string(),
                workspaces: workspaces.to_vec(),
            },
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn ensure_session(
        &self,
        session_id: &str,
        workspace_id: &str,
        workspaces: &[agent_store::SessionWorkspace],
    ) -> Result<()> {
        let runtime_id = self.repo.session_runtime_id(session_id).await?;
        self.ensure_for_runtime(&runtime_id, workspace_id, workspaces)
            .await
    }

    pub(crate) async fn fork_session_from_parent(
        &self,
        parent_session_id: &str,
        source_workspace_id: &str,
        workspaces: &[agent_store::SessionWorkspace],
        target_workspace_id: &str,
    ) -> Result<(String, Vec<agent_store::SessionWorkspace>)> {
        let runtime_id = self.repo.session_runtime_id(parent_session_id).await?;
        let result = self
            .execute(
                &runtime_id,
                RuntimeCommand::ForkSession {
                    source_workspace_id: source_workspace_id.to_string(),
                    target_workspace_id: target_workspace_id.to_string(),
                    workspaces: workspaces.to_vec(),
                },
            )
            .await?;
        let RuntimeCommandResult::Materialized { workspaces } = result else {
            return Err(anyhow!("runtime returned wrong fork result"));
        };
        Ok((target_workspace_id.to_string(), workspaces))
    }

    pub(crate) async fn destroy_session_workspaces(&self, session_id: &str) -> Result<()> {
        let config = self.repo.load_session_config(session_id).await?;
        self.execute(
            &config.runtime_id,
            RuntimeCommand::DestroySession {
                workspace_id: config.workspace_id,
            },
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn reconcile_project_bases(
        &self,
        project_id: uuid::Uuid,
        workspaces: &[agent_store::ProjectWorkspace],
    ) -> Result<()> {
        let project = self.repo.get_project(project_id).await?;
        self.execute(
            &project.runtime_id,
            RuntimeCommand::ReconcileProject {
                project_id: project_id.to_string(),
                workspaces: workspaces.to_vec(),
            },
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn remove_project_bases(
        &self,
        runtime_id: &str,
        project_id: uuid::Uuid,
    ) -> Result<()> {
        self.execute(
            runtime_id,
            RuntimeCommand::RemoveProject {
                project_id: project_id.to_string(),
            },
        )
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use agent_runtime_protocol::{ProjectWorkspace, SessionWorkspace, WorkspaceKind};
    use agent_tools::{ToolContext, ToolRegistry};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    pub(crate) const TEST_RUNTIME_ID: &str = "runtime-test";

    /// Connect an in-process fake runtime to `registry` over an in-memory pipe
    /// and wait until it is registered. It answers workspace commands with
    /// canned results (no btrfs) and runs the real `ToolRegistry` in a plain
    /// per-workspace temp dir for `ExecuteTool`, so daemon orchestration/fork
    /// tests keep real coverage without a runtime host.
    pub(crate) async fn connect_test_runtime(registry: &RuntimeRegistry, runtime_id: &str) {
        let (control_io, runtime_io) = tokio::io::duplex(1 << 20);
        let control = registry.clone();
        tokio::spawn(async move {
            let _ = control.handle_connection(control_io).await;
        });
        let id = runtime_id.to_string();
        tokio::spawn(async move {
            let _ = fake_runtime_loop(runtime_io, id).await;
        });
        for _ in 0..200 {
            if registry.require_available(runtime_id).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("test runtime {runtime_id} never registered");
    }

    async fn fake_runtime_loop<S>(stream: S, runtime_id: String) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_frame(
            &mut writer,
            &RuntimeToControl::Hello(RuntimeHello {
                runtime_id,
                name: "fake test runtime".to_string(),
            }),
        )
        .await?;
        let tools = ToolRegistry::with_builtin_tools();
        let dirs: Arc<TokioMutex<HashMap<String, std::path::PathBuf>>> = Default::default();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = heartbeat.tick() => write_frame(&mut writer, &RuntimeToControl::Heartbeat).await?,
                frame = read_frame::<ControlToRuntime>(&mut reader) => {
                    let Some(frame) = frame? else { break };
                    let ControlToRuntime::Command { command_id, command } = frame else { continue };
                    let result = handle_fake_command(&tools, &dirs, command).await;
                    write_frame(&mut writer, &RuntimeToControl::Result { command_id, result }).await?;
                }
            }
        }
        Ok(())
    }

    async fn handle_fake_command(
        tools: &ToolRegistry,
        dirs: &Arc<TokioMutex<HashMap<String, std::path::PathBuf>>>,
        command: RuntimeCommand,
    ) -> std::result::Result<RuntimeCommandResult, RuntimeCommandError> {
        match command {
            RuntimeCommand::ValidateProject { .. }
            | RuntimeCommand::EnsureSession { .. }
            | RuntimeCommand::DestroySession { .. }
            | RuntimeCommand::ReconcileProject { .. }
            | RuntimeCommand::RemoveProject { .. } => Ok(RuntimeCommandResult::Ack),
            RuntimeCommand::MaterializeSession {
                selected_workspaces,
                ..
            } => Ok(RuntimeCommandResult::Materialized {
                workspaces: selected_workspaces
                    .into_iter()
                    .map(|selected| session_workspace_from(selected.workspace))
                    .collect(),
            }),
            RuntimeCommand::ForkSession { workspaces, .. } => {
                Ok(RuntimeCommandResult::Materialized { workspaces })
            }
            RuntimeCommand::ExecuteTool {
                workspace_id,
                provider,
                tool_call,
            } => {
                let dir = {
                    let mut dirs = dirs.lock().await;
                    dirs.entry(workspace_id)
                        .or_insert_with(|| {
                            let path =
                                std::env::temp_dir().join(format!("pi-fake-ws-{}", Uuid::new_v4()));
                            std::fs::create_dir_all(&path).expect("fake workspace dir");
                            path
                        })
                        .clone()
                };
                let context = ToolContext::new(dir);
                match tools.execute(provider, &tool_call, &context).await {
                    Ok(result) => Ok(RuntimeCommandResult::Tool { result }),
                    Err(error) => Err(RuntimeCommandError::new("tool_error", format!("{error:#}"))),
                }
            }
        }
    }

    fn session_workspace_from(workspace: ProjectWorkspace) -> SessionWorkspace {
        match workspace.kind {
            WorkspaceKind::Git => SessionWorkspace::git(
                workspace.workspace_dir,
                workspace.remote_url.unwrap_or_default(),
                workspace.remote_branch.unwrap_or_default(),
                "0".repeat(40),
                "pi/test".to_string(),
            ),
            WorkspaceKind::Local => SessionWorkspace::local(
                workspace.workspace_dir,
                workspace.source_path.unwrap_or_default(),
            ),
        }
    }
}
