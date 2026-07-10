use std::collections::HashSet;
use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, ProtocolVersion,
    ServerNotification, ServerResult, Tool,
};
use rmcp::service::{
    NotificationContext, PeerRequestOptions, RequestHandle, RoleClient, RunningService,
    RxJsonRpcMessage, TxJsonRpcMessage,
};
use rmcp::transport::{which_command, Transport};
use rmcp::ClientHandler;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Mutex, Notify, RwLock, Semaphore};
use tokio::time::{timeout, timeout_at, Instant};

use crate::config::McpServerConfig;

const MAX_INBOUND_FRAME_BYTES: usize = 2 * 1024 * 1024;
const MAX_LIST_PAGES: usize = 64;
const MAX_LIST_TOOLS: usize = 512;
const MAX_LIST_BYTES: usize = 2 * 1024 * 1024;
const MAX_CALL_ARGUMENT_BYTES: usize = 256 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const WRITER_CLOSE_TIMEOUT: Duration = Duration::from_millis(100);
const CANCELLATION_DELIVERY_TIMEOUT: Duration = Duration::from_millis(50);

type Service = RunningService<RoleClient, Arc<ClientNotifications>>;

#[derive(Debug, Error)]
pub(crate) enum McpClientCallError {
    #[error("MCP tools/call timed out")]
    Timeout,
    #[error("MCP tool catalog changed before call admission")]
    ToolsChanged,
    #[error("{0}")]
    Protocol(String),
}

struct ClientNotifications {
    tools_revision: AtomicU64,
    tools_uncertain: AtomicBool,
    accepts_tools_changed: AtomicBool,
    admission_barrier: RwLock<()>,
}

impl Default for ClientNotifications {
    fn default() -> Self {
        Self {
            tools_revision: AtomicU64::new(0),
            tools_uncertain: AtomicBool::new(false),
            accepts_tools_changed: AtomicBool::new(false),
            admission_barrier: RwLock::new(()),
        }
    }
}

impl ClientNotifications {
    fn mark_tools_changed_received(&self) -> bool {
        let accepted = self.accepts_tools_changed.load(Ordering::Acquire);
        if accepted {
            self.tools_uncertain.store(true, Ordering::Release);
            self.tools_revision.fetch_add(1, Ordering::AcqRel);
        }
        accepted
    }
}

impl ClientHandler for ClientNotifications {
    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let accepted = self.accepts_tools_changed.load(Ordering::Acquire);
        async move {
            if !accepted {
                return;
            }
            // Taking the write side orders notification handling against call
            // admission. A call that already owns the read side is admitted
            // before this notification; every later admission observes the
            // receipt-time revision and refreshes first.
            let _barrier = self.admission_barrier.write().await;
            self.tools_uncertain.store(false, Ordering::Release);
        }
    }
}

#[derive(Default)]
struct ClientLiveness {
    closed: AtomicBool,
    #[cfg(test)]
    closed_notify: Notify,
}

impl ClientLiveness {
    fn mark_closed(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            #[cfg(test)]
            self.closed_notify.notify_waiters();
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    #[cfg(test)]
    async fn wait_for_closed(&self) {
        loop {
            let closed = self.closed_notify.notified();
            tokio::pin!(closed);
            closed.as_mut().enable();
            if self.is_closed() {
                return;
            }
            closed.await;
        }
    }
}

struct BoundedChildTransport {
    child: Option<Box<dyn ChildWrapper>>,
    reader: BufReader<ChildStdout>,
    writer: Arc<Mutex<Option<ChildStdin>>>,
    frame: Vec<u8>,
    liveness: Arc<ClientLiveness>,
    notifications: Arc<ClientNotifications>,
}

impl BoundedChildTransport {
    fn spawn(
        config: &McpServerConfig,
        liveness: Arc<ClientLiveness>,
        notifications: Arc<ClientNotifications>,
    ) -> Result<Self> {
        let command = which_command(&config.command).context("resolve MCP executable")?;
        let mut command = CommandWrap::from(command);
        command.command_mut().args(&config.args);
        command.command_mut().env_clear();
        for name in [
            "PATH",
            "HOME",
            "USERPROFILE",
            "SYSTEMROOT",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
            "TMPDIR",
            "TMP",
            "TEMP",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.command_mut().env(name, value);
            }
        }
        if let Some(cwd) = &config.cwd {
            command.command_mut().current_dir(cwd);
        }
        command.command_mut().envs(&config.env);
        for name in &config.inherit_env {
            let value = std::env::var_os(name)
                .ok_or_else(|| anyhow!("inherited environment variable {name} is not set"))?;
            command.command_mut().env(name, value);
        }
        command
            .command_mut()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);
        command.wrap(KillOnDrop);
        let mut child = command.spawn().context("spawn MCP stdio server")?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| anyhow!("MCP child stdout was not piped"))?;
        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| anyhow!("MCP child stdin was not piped"))?;
        if let Some(mut stderr) = child.stderr().take() {
            tokio::spawn(async move {
                let mut buffer = [0_u8; 8192];
                while stderr.read(&mut buffer).await.unwrap_or(0) != 0 {}
            });
        }
        Ok(Self {
            child: Some(child),
            reader: BufReader::with_capacity(8192, stdout),
            writer: Arc::new(Mutex::new(Some(stdin))),
            frame: Vec::with_capacity(8192),
            liveness,
            notifications,
        })
    }

    async fn receive_frame(&mut self) -> io::Result<Option<&[u8]>> {
        self.frame.clear();
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                self.liveness.mark_closed();
                return Ok(None);
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            let content_len = newline.unwrap_or(available.len());
            if self.frame.len().saturating_add(content_len) > MAX_INBOUND_FRAME_BYTES {
                self.liveness.mark_closed();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("MCP inbound frame exceeds {MAX_INBOUND_FRAME_BYTES} bytes"),
                ));
            }
            self.frame.extend_from_slice(&available[..content_len]);
            self.reader.consume(consumed);
            if newline.is_some() {
                if self.frame.last() == Some(&b'\r') {
                    self.frame.pop();
                }
                if !self.frame.is_empty() {
                    return Ok(Some(&self.frame));
                }
            }
        }
    }
}

impl Drop for BoundedChildTransport {
    fn drop(&mut self) {
        self.liveness.mark_closed();
        if let Some(child) = self.child.as_mut() {
            // This is deliberately synchronous and precedes dropping any
            // potentially blocked stdin writer. ProcessGroup/JobObject
            // implements start_kill for the complete process tree.
            let _ = child.start_kill();
        }
    }
}

impl Transport<RoleClient> for BoundedChildTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let writer = self.writer.clone();
        let liveness = self.liveness.clone();
        async move {
            let mut bytes = serde_json::to_vec(&item).map_err(io::Error::other)?;
            bytes.push(b'\n');
            let mut writer = writer.lock().await;
            let writer = writer.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "MCP transport is closed")
            })?;
            if let Err(error) = writer.write_all(&bytes).await {
                liveness.mark_closed();
                return Err(error);
            }
            if let Err(error) = writer.flush().await {
                liveness.mark_closed();
                return Err(error);
            }
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let frame = match self.receive_frame().await {
            Ok(Some(frame)) => frame,
            Ok(None) => return None,
            Err(error) => {
                eprintln!("closing MCP transport after invalid inbound frame: {error}");
                return None;
            }
        };
        match serde_json::from_slice(frame) {
            Ok(message) => {
                if matches!(
                    &message,
                    rmcp::model::JsonRpcMessage::Notification(notification)
                        if matches!(
                            notification.notification,
                            ServerNotification::ToolListChangedNotification(_)
                        )
                ) {
                    self.notifications.mark_tools_changed_received();
                }
                Some(message)
            }
            Err(error) => {
                self.liveness.mark_closed();
                eprintln!("closing MCP transport after invalid JSON-RPC frame: {error}");
                None
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.liveness.mark_closed();
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        if let Ok(mut writer) = timeout(WRITER_CLOSE_TIMEOUT, self.writer.lock()).await {
            writer.take();
        }
        if let Some(mut child) = self.child.take() {
            let kill = Box::into_pin(child.kill());
            let _ = timeout(SHUTDOWN_TIMEOUT, kill).await;
        }
        Ok(())
    }
}

pub(crate) struct McpClient {
    service: Mutex<Service>,
    calls: Semaphore,
    active_calls: AtomicUsize,
    calls_idle: Notify,
    notifications: Arc<ClientNotifications>,
    liveness: Arc<ClientLiveness>,
}

impl McpClient {
    pub(crate) async fn start(config: &McpServerConfig) -> Result<(Arc<Self>, Vec<Tool>)> {
        let liveness = Arc::new(ClientLiveness::default());
        let notifications = Arc::new(ClientNotifications::default());
        let transport =
            BoundedChildTransport::spawn(config, liveness.clone(), notifications.clone())?;
        let service = timeout(
            config.startup_timeout(),
            rmcp::serve_client(notifications.clone(), transport),
        )
        .await
        .map_err(|_| anyhow!("MCP initialize timed out"))?
        .context("initialize MCP client")?;
        let peer_info = service
            .peer()
            .peer_info()
            .ok_or_else(|| anyhow!("MCP initialize returned no server information"))?;
        if !ProtocolVersion::KNOWN_VERSIONS.contains(&peer_info.protocol_version) {
            bail!(
                "MCP server negotiated unsupported protocol version {}",
                peer_info.protocol_version
            );
        }
        let tools_capability = peer_info
            .capabilities
            .tools
            .as_ref()
            .ok_or_else(|| anyhow!("MCP server did not advertise the tools capability"))?;
        notifications.accepts_tools_changed.store(
            tools_capability.list_changed == Some(true),
            Ordering::Release,
        );
        let client = Arc::new(Self {
            service: Mutex::new(service),
            calls: Semaphore::new(config.parallel_calls),
            active_calls: AtomicUsize::new(0),
            calls_idle: Notify::new(),
            notifications,
            liveness,
        });
        let (tools, _) = client.refresh_tools(config).await?;
        Ok((client, tools))
    }

    async fn list_tools_bounded(&self) -> Result<Vec<Tool>> {
        let service = self.service.lock().await.peer().clone();
        let mut tools = Vec::new();
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut bytes = 0_usize;
        for _ in 0..MAX_LIST_PAGES {
            let mut params = rmcp::model::PaginatedRequestParams::default();
            params.cursor = cursor.clone();
            let result = service
                .list_tools(Some(params))
                .await
                .context("list MCP tools")?;
            bytes = bytes.saturating_add(serde_json::to_vec(&result)?.len());
            if bytes > MAX_LIST_BYTES {
                bail!("MCP tools/list exceeds {MAX_LIST_BYTES} bytes");
            }
            tools.extend(result.tools);
            if tools.len() > MAX_LIST_TOOLS {
                bail!("MCP tools/list has more than {MAX_LIST_TOOLS} tools");
            }
            let Some(next_cursor) = result.next_cursor else {
                return Ok(tools);
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                bail!("MCP tools/list repeated a cursor");
            }
            cursor = Some(next_cursor);
        }
        bail!("MCP tools/list has more than {MAX_LIST_PAGES} pages")
    }

    pub(crate) async fn refresh_tools(&self, config: &McpServerConfig) -> Result<(Vec<Tool>, u64)> {
        let deadline = Instant::now() + config.startup_timeout();
        let _admission = timeout_at(deadline, self.notifications.admission_barrier.write())
            .await
            .map_err(|_| anyhow!("MCP tools/list refresh barrier timed out"))?;
        loop {
            let revision = self.tools_revision();
            let tools = timeout_at(deadline, self.list_tools_bounded())
                .await
                .map_err(|_| anyhow!("MCP tools/list refresh timed out"))??;
            let refreshed_revision = self.tools_revision();
            if revision == refreshed_revision {
                return Ok((tools, refreshed_revision));
            }
        }
    }

    pub(crate) fn tools_revision(&self) -> u64 {
        self.notifications.tools_revision.load(Ordering::Acquire)
    }

    pub(crate) fn tools_uncertain(&self) -> bool {
        self.notifications.tools_uncertain.load(Ordering::Acquire)
    }

    pub(crate) async fn call(
        self: &Arc<Self>,
        _pin: McpClientPin,
        deadline: Instant,
        expected_tools_revision: u64,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<CallToolResult, McpClientCallError> {
        if serde_json::to_vec(&arguments)
            .map_err(|error| McpClientCallError::Protocol(error.to_string()))?
            .len()
            > MAX_CALL_ARGUMENT_BYTES
        {
            return Err(McpClientCallError::Protocol(format!(
                "MCP tool arguments exceed {MAX_CALL_ARGUMENT_BYTES} bytes"
            )));
        }
        let _permit = timeout_at(deadline, self.calls.acquire())
            .await
            .map_err(|_| McpClientCallError::Timeout)?
            .map_err(|_| McpClientCallError::Protocol("MCP client is shutting down".to_string()))?;
        let admission = timeout_at(deadline, self.notifications.admission_barrier.read())
            .await
            .map_err(|_| McpClientCallError::Timeout)?;
        if self.tools_uncertain() || self.tools_revision() != expected_tools_revision {
            return Err(McpClientCallError::ToolsChanged);
        }
        let service = self.service.lock().await.peer().clone();
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            CallToolRequestParams::new(name.to_string()).with_arguments(arguments),
        ));
        let handle = timeout_at(
            deadline,
            service.send_cancellable_request(request, PeerRequestOptions::no_options()),
        )
        .await
        .map_err(|_| McpClientCallError::Timeout)?
        .map_err(|error| McpClientCallError::Protocol(error.to_string()))?;
        drop(admission);
        let response = CancellableRequest::new(handle, self.clone())
            .wait(deadline)
            .await?;
        match response {
            ServerResult::CallToolResult(result) => Ok(result),
            _ => Err(McpClientCallError::Protocol(
                "MCP tools/call returned an unexpected response".to_string(),
            )),
        }
    }

    pub(crate) async fn shutdown(&self) {
        let mut service = self.service.lock().await;
        let _ = service.close_with_timeout(SHUTDOWN_TIMEOUT).await;
        self.liveness.mark_closed();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.liveness.is_closed()
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_closed(&self) {
        self.liveness.wait_for_closed().await;
    }

    pub(crate) async fn wait_for_calls_idle(&self) {
        loop {
            let idle = self.calls_idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.active_calls.load(Ordering::Acquire) == 0 {
                return;
            }
            idle.await;
        }
    }

    pub(crate) fn pin(self: &Arc<Self>) -> McpClientPin {
        self.active_calls.fetch_add(1, Ordering::AcqRel);
        McpClientPin {
            client: self.clone(),
        }
    }
}

pub(crate) struct McpClientPin {
    client: Arc<McpClient>,
}

impl Drop for McpClientPin {
    fn drop(&mut self) {
        if self.client.active_calls.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.client.calls_idle.notify_waiters();
        }
    }
}

struct CancellableRequest {
    handle: Option<RequestHandle<RoleClient>>,
    client: Arc<McpClient>,
}

impl CancellableRequest {
    fn new(handle: RequestHandle<RoleClient>, client: Arc<McpClient>) -> Self {
        Self {
            handle: Some(handle),
            client,
        }
    }

    async fn wait(mut self, deadline: Instant) -> Result<ServerResult, McpClientCallError> {
        let response = {
            let handle = self.handle.as_mut().expect("request handle is present");
            timeout_at(deadline, &mut handle.rx).await
        };
        match response {
            Ok(Ok(Ok(result))) => {
                self.handle.take();
                Ok(result)
            }
            Ok(Ok(Err(error))) => {
                self.handle.take();
                Err(McpClientCallError::Protocol(error.to_string()))
            }
            Ok(Err(_)) => {
                self.handle.take();
                Err(McpClientCallError::Protocol(
                    "MCP transport closed during tools/call".to_string(),
                ))
            }
            Err(_) => {
                if let Some(handle) = self.handle.take() {
                    spawn_bounded_cancellation(
                        handle,
                        self.client.clone(),
                        "MCP tools/call deadline exceeded",
                    );
                }
                Err(McpClientCallError::Timeout)
            }
        }
    }
}

impl Drop for CancellableRequest {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            spawn_bounded_cancellation(
                handle,
                self.client.clone(),
                "MCP tools/call task cancelled",
            );
        }
    }
}

fn spawn_bounded_cancellation(
    handle: RequestHandle<RoleClient>,
    client: Arc<McpClient>,
    reason: &'static str,
) {
    // Cancellation delivery shares the stdio writer with requests and can be
    // backpressured forever. Mark this connection unusable immediately and
    // deliver cancellation only as a separately bounded best effort.
    client.liveness.mark_closed();
    tokio::spawn(async move {
        let _ = timeout(
            CANCELLATION_DELIVERY_TIMEOUT,
            handle.cancel(Some(reason.to_string())),
        )
        .await;
        drop(client);
    });
}
