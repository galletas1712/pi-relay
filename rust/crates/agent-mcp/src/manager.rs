use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_tools::ProviderTool;
use agent_vocab::ProviderKind;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, Notify, RwLock};

use crate::catalog::{
    build_inventory_catalog, declaration_token_estimate, select_manifest, DiscoveredTool, MAX_TOOLS,
};
use crate::client::{McpClient, McpClientCallError, McpClientStart};
use crate::config::{McpConfig, McpServerConfig};
use crate::result::normalize_call_result;
use crate::{McpSessionManifest, McpSessionSnapshot};

const MAX_SELECTED_SERVERS: usize = 64;
const MAX_REVISION_BYTES: usize = 128;
const MAX_SERVER_ID_BYTES: usize = 128;
const MAX_RAW_TOOL_NAME_BYTES: usize = 256;
const MAX_MCP_PROMPT_SUMMARY_BYTES: usize = 16 * 1024;
const MAX_PROVIDER_TOOLSET_BYTES: usize = 1024 * 1024;

fn bounded_error_message(mut message: String) -> String {
    const MAX_ERROR_BYTES: usize = 16 * 1024;
    if message.len() <= MAX_ERROR_BYTES {
        return message;
    }
    let mut end = MAX_ERROR_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str(" [truncated]");
    message
}

fn discovered_tools(
    server_id: &str,
    config: &McpServerConfig,
    tools: Vec<rmcp::model::Tool>,
) -> anyhow::Result<Vec<DiscoveredTool>> {
    let tools = tools
        .into_iter()
        .filter(|tool| config.tool_enabled(tool.name.as_ref()))
        .map(|tool| DiscoveredTool {
            server_id: server_id.to_string(),
            server_config_fingerprint: config.semantic_fingerprint(),
            raw_name: tool.name.into_owned(),
            description: tool
                .description
                .map(|value| value.into_owned())
                .unwrap_or_default(),
            input_schema: Value::Object((*tool.input_schema).clone()),
        })
        .collect::<Vec<_>>();
    let config_fingerprints =
        BTreeMap::from([(server_id.to_string(), config.semantic_fingerprint())]);
    build_inventory_catalog(&config_fingerprints, tools.clone(), &BTreeSet::new())?;
    Ok(tools)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpHealth {
    Healthy,
    Unavailable,
    Revoked,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthStatus {
    NonOauth,
    Unsupported,
    Unknown,
    Bearer,
    LoginRequired,
    ReauthenticationRequired,
    OauthReady,
    AuthorizationPending,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthKind {
    None,
    Bearer,
    Oauth,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthFailure {
    CredentialStoreUnavailable,
    DiscoveryFailed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct McpAuthServerStatus {
    pub server: String,
    pub auth_kind: McpAuthKind,
    pub auth_state: McpAuthStatus,
    pub can_login: bool,
    pub can_logout: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<McpAuthFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpLogoutResult {
    Removed,
    NotFound,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct McpInventoryTool {
    pub raw_name: String,
    pub description: String,
    pub context_token_estimate: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct McpInventoryServer {
    pub server: String,
    pub revision: String,
    pub health: McpHealth,
    pub tools: Vec<McpInventoryTool>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct McpInventory {
    pub revision: String,
    pub servers: Vec<McpInventoryServer>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpSessionSelection {
    pub inventory_revision: String,
    pub servers: Vec<McpServerSelection>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpServerSelection {
    pub server: String,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct McpToolView {
    pub server: String,
    pub raw_name: String,
    pub exposed_name: String,
    pub contract_fingerprint: String,
    pub health: McpHealth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallOutput {
    pub output: String,
    pub is_error: bool,
}

#[derive(Debug, Error)]
pub enum McpManagerError {
    #[error("mcp_inventory_changed: MCP inventory changed")]
    InventoryChanged { current_revision: String },
    #[error("mcp_selection_invalid: {message}")]
    SelectionInvalid { message: String },
    #[error("mcp_unavailable: selected MCP server {server} is unavailable")]
    Unavailable { server: String },
    #[error("mcp_oauth_credential_store_failed")]
    CredentialStore(#[from] crate::OAuthCredentialStoreError),
    #[error("invalid MCP catalog: {0}")]
    Catalog(#[from] anyhow::Error),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum McpCallError {
    #[error("mcp_tool_revoked: MCP tool {tool} is no longer allowed")]
    Revoked { tool: String },
    #[error("mcp_server_unavailable: MCP server {server} is unavailable")]
    ServerUnavailable { server: String },
    #[error("mcp_tool_contract_changed: MCP tool {tool} no longer has the advertised contract")]
    ContractChanged { tool: String },
    #[error("mcp_call_timeout: MCP tool {tool} exceeded its total call deadline")]
    Timeout { tool: String },
    #[error("mcp_protocol_error: {message}")]
    Protocol { message: String },
}

struct ServerState {
    config: McpServerConfig,
    client: Option<Arc<McpClient>>,
    /// The last fully validated catalog is retained for inventory diagnostics
    /// and existing session contract checks even while the route is down.
    tools: Vec<DiscoveredTool>,
    health: McpHealth,
    catalog_tools_revision: u64,
    /// Whether `tools` is a coherent semantic catalog rather than only the
    /// last observation retained for diagnostics.
    catalog_coherent: bool,
    route_lock: Arc<Mutex<()>>,
    refresh: RefreshState,
}

struct RefreshState {
    generation: u64,
    disposition: RetryDisposition,
}

enum RetryDisposition {
    Automatic,
    UserActionRequired,
}

enum RefreshAttempt {
    Connected(Arc<McpClient>, Vec<rmcp::model::Tool>, u64),
    Failed {
        error: anyhow::Error,
        disposition: RetryDisposition,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefreshOutcome {
    Complete,
    DeadlineElapsed,
}

impl RetryDisposition {
    fn permits_automatic_attempt(&self) -> bool {
        match self {
            Self::Automatic => true,
            Self::UserActionRequired => false,
        }
    }
}

async fn oauth_access_token(
    runtime: &crate::oauth_runtime::OAuthRuntimeManager,
    server_id: &str,
    config: &McpServerConfig,
) -> Result<Option<crate::oauth_runtime::OAuthAccessToken>, RetryDisposition> {
    let crate::config::McpTransportConfig::StreamableHttp(http) = &config.transport else {
        return Ok(None);
    };
    if http
        .auth
        .as_ref()
        .and_then(crate::McpHttpAuthConfig::oauth)
        .is_none()
    {
        return Ok(None);
    }
    runtime
        .access_token(server_id, http)
        .await
        .map(Some)
        .map_err(|failure| match failure {
            crate::oauth_runtime::OAuthRouteFailure::LoginRequired
            | crate::oauth_runtime::OAuthRouteFailure::ReauthenticationRequired
            | crate::oauth_runtime::OAuthRouteFailure::Unsupported
            | crate::oauth_runtime::OAuthRouteFailure::Store => {
                RetryDisposition::UserActionRequired
            }
            crate::oauth_runtime::OAuthRouteFailure::Unknown => RetryDisposition::Automatic,
        })
}

impl ServerState {
    fn is_healthy(&self) -> bool {
        self.health == McpHealth::Healthy
            && self
                .client
                .as_ref()
                .is_some_and(|client| !client.is_closed() && !client.tools_uncertain())
    }

    fn catalog_is_current(&self) -> bool {
        self.catalog_coherent
            && self.client.as_ref().is_none_or(|client| {
                !client.tools_uncertain() && client.tools_revision() == self.catalog_tools_revision
            })
    }

    fn mark_unavailable(&mut self) {
        self.health = McpHealth::Unavailable;
        self.client = None;
        self.refresh.disposition = RetryDisposition::Automatic;
    }
}

pub struct McpManager {
    servers: RwLock<BTreeMap<String, ServerState>>,
    bearer_resolver: Option<crate::http_transport::BearerResolver>,
    oauth: Arc<crate::oauth_login::OAuthCoordinator>,
    oauth_runtime: Arc<crate::oauth_runtime::OAuthRuntimeManager>,
    shutting_down: AtomicBool,
    shutdown_notify: Notify,
}

impl McpManager {
    pub async fn start(config: McpConfig) -> Result<Arc<Self>, McpManagerError> {
        let repository = crate::oauth_credentials::OAuthCredentialRepository::memory();
        Self::start_with_repository(config, None, repository).await
    }

    pub async fn start_with_credential_file(
        config: McpConfig,
        path: PathBuf,
    ) -> Result<Arc<Self>, McpManagerError> {
        let repository = crate::oauth_credentials::OAuthCredentialRepository::open_file(path)
            .unwrap_or_else(crate::oauth_credentials::OAuthCredentialRepository::unavailable);
        Self::start_with_repository(config, None, repository).await
    }

    async fn start_with_repository(
        config: McpConfig,
        bearer_resolver: Option<crate::http_transport::BearerResolver>,
        repository: Arc<crate::oauth_credentials::OAuthCredentialRepository>,
    ) -> Result<Arc<Self>, McpManagerError> {
        config.validate()?;
        let oauth_runtime = crate::oauth_runtime::OAuthRuntimeManager::new(repository.clone());
        let startup_parallelism = config.servers.len().max(1);
        let starts = futures_util::stream::iter(config.servers.clone())
            .map(|(server_id, server_config)| {
                let start_resolver = bearer_resolver.clone();
                let start_oauth = oauth_runtime.clone();
                async move {
                    let deadline = tokio::time::Instant::now() + server_config.startup_timeout();
                    let oauth_token =
                        oauth_access_token(&start_oauth, &server_id, &server_config).await;
                    let state = match oauth_token {
                        Err(disposition) => unavailable_server(server_config, disposition),
                        Ok(oauth_token) => match McpClient::start(
                            &server_config,
                            deadline,
                            start_resolver.as_ref(),
                            oauth_token,
                        )
                        .await
                        {
                            McpClientStart::Connected(client, tools) => {
                                match discovered_tools(&server_id, &server_config, tools) {
                                    Ok(tools) => ServerState {
                                        config: server_config,
                                        catalog_tools_revision: client.tools_revision(),
                                        client: Some(client),
                                        tools,
                                        health: McpHealth::Healthy,
                                        catalog_coherent: true,
                                        route_lock: Arc::new(Mutex::new(())),
                                        refresh: RefreshState {
                                            generation: 1,
                                            disposition: RetryDisposition::Automatic,
                                        },
                                    },
                                    Err(error) => {
                                        eprintln!(
                                    "MCP server {server_id} returned an invalid catalog: {error:#}"
                                );
                                        client.shutdown().await;
                                        unavailable_server(
                                            server_config,
                                            RetryDisposition::Automatic,
                                        )
                                    }
                                }
                            }
                            McpClientStart::OAuthLoginRequired => {
                                eprintln!("MCP server {server_id} requires OAuth login");
                                unavailable_server(
                                    server_config,
                                    RetryDisposition::UserActionRequired,
                                )
                            }
                            McpClientStart::ConnectionFailed(error) => {
                                eprintln!(
                                    "MCP server {server_id} unavailable during startup: {error:#}"
                                );
                                unavailable_server(server_config, RetryDisposition::Automatic)
                            }
                        },
                    };
                    (server_id, state)
                }
            })
            .buffer_unordered(startup_parallelism)
            .collect::<Vec<_>>()
            .await;
        Ok(Arc::new(Self {
            servers: RwLock::new(starts.into_iter().collect()),
            bearer_resolver,
            oauth: crate::oauth_login::OAuthCoordinator::with_runtime(oauth_runtime.clone()),
            oauth_runtime,
            shutting_down: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
        }))
    }

    pub fn disabled() -> Arc<Self> {
        let repository = crate::oauth_credentials::OAuthCredentialRepository::memory();
        let oauth_runtime = crate::oauth_runtime::OAuthRuntimeManager::new(repository.clone());
        Arc::new(Self {
            servers: RwLock::new(BTreeMap::new()),
            bearer_resolver: None,
            oauth: crate::oauth_login::OAuthCoordinator::with_runtime(oauth_runtime.clone()),
            oauth_runtime,
            shutting_down: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
        })
    }

    pub async fn inventory(
        &self,
        provider: ProviderKind,
        first_party: &HashMap<ProviderKind, Vec<ProviderTool>>,
    ) -> Result<McpInventory, McpManagerError> {
        self.refresh_inventory().await;
        let (catalog, globally_coherent) = self.inventory_catalog(first_party).await?;
        if !globally_coherent {
            return Err(McpManagerError::InventoryChanged {
                current_revision: catalog.inventory_revision,
            });
        }
        let servers = self.servers.read().await;
        let inventory_servers = catalog
            .server_revisions
            .iter()
            .map(|(server_id, revision)| {
                let server = servers
                    .get(server_id)
                    .expect("inventory revision only contains configured servers");
                let declarations = catalog.provider_tools(provider);
                let tools = catalog
                    .tools
                    .iter()
                    .filter(|tool| tool.server_id == *server_id)
                    .map(|tool| {
                        let declaration = declarations
                            .iter()
                            .find(|candidate| candidate.name == tool.exposed_name)
                            .expect("inventory provider declaration matches its tool");
                        Ok(McpInventoryTool {
                            raw_name: tool.raw_name.clone(),
                            description: tool.description.clone(),
                            context_token_estimate: declaration_token_estimate(declaration)?,
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                Ok(McpInventoryServer {
                    server: server_id.clone(),
                    revision: revision.clone(),
                    health: server.health,
                    tools,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(McpInventory {
            revision: catalog.inventory_revision,
            servers: inventory_servers,
        })
    }

    pub async fn select(
        &self,
        selection: &McpSessionSelection,
        first_party: &HashMap<ProviderKind, Vec<ProviderTool>>,
    ) -> Result<McpSessionSnapshot, McpManagerError> {
        let selected = validate_selection_shape(selection)?;
        self.refresh_selection(&selected).await;
        let (catalog, globally_coherent) = self.inventory_catalog(first_party).await?;
        if !globally_coherent {
            return Err(McpManagerError::InventoryChanged {
                current_revision: catalog.inventory_revision,
            });
        }
        if catalog.inventory_revision != selection.inventory_revision {
            return Err(McpManagerError::InventoryChanged {
                current_revision: catalog.inventory_revision,
            });
        }
        {
            let servers = self.servers.read().await;
            for server_id in selected.keys() {
                let Some(server) = servers.get(server_id) else {
                    return Err(McpManagerError::SelectionInvalid {
                        message: format!("unknown MCP server {server_id}"),
                    });
                };
                if !server.is_healthy() {
                    return Err(McpManagerError::Unavailable {
                        server: server_id.clone(),
                    });
                }
            }
        }
        let manifest = select_manifest(&catalog, &selected).map_err(|error| {
            McpManagerError::SelectionInvalid {
                message: error.to_string(),
            }
        })?;
        let prompt_summary_bytes = manifest
            .tools
            .iter()
            .map(|tool| tool.server_id.len() + tool.exposed_name.len() + 8)
            .sum::<usize>();
        if prompt_summary_bytes > MAX_MCP_PROMPT_SUMMARY_BYTES {
            return Err(McpManagerError::SelectionInvalid {
                message: format!(
                    "selected MCP names exceed the {MAX_MCP_PROMPT_SUMMARY_BYTES}-byte prompt summary limit"
                ),
            });
        }
        for provider in [ProviderKind::OpenAi, ProviderKind::Claude] {
            let first_party_bytes = first_party
                .get(&provider)
                .into_iter()
                .flatten()
                .map(|tool| serde_json::to_vec(&tool.declaration).map(|bytes| bytes.len()))
                .sum::<serde_json::Result<usize>>()
                .map_err(anyhow::Error::from)?;
            let mcp_bytes = manifest
                .provider_tools(provider)
                .iter()
                .map(|tool| serde_json::to_vec(&tool.declaration).map(|bytes| bytes.len()))
                .sum::<serde_json::Result<usize>>()
                .map_err(anyhow::Error::from)?;
            if first_party_bytes.saturating_add(mcp_bytes) > MAX_PROVIDER_TOOLSET_BYTES {
                return Err(McpManagerError::SelectionInvalid {
                    message: format!(
                        "selected provider toolset exceeds {MAX_PROVIDER_TOOLSET_BYTES} bytes"
                    ),
                });
            }
        }
        Ok(McpSessionSnapshot::new(manifest)?)
    }

    pub fn snapshot_from_manifest(
        &self,
        manifest: McpSessionManifest,
    ) -> Result<McpSessionSnapshot, McpManagerError> {
        Ok(McpSessionSnapshot::from_persisted(manifest)?)
    }

    pub async fn call(
        &self,
        snapshot: &McpSessionSnapshot,
        exposed_name: &str,
        arguments: Value,
    ) -> Result<McpCallOutput, McpCallError> {
        let started = tokio::time::Instant::now();
        let tool = snapshot.manifest().tool(exposed_name).ok_or_else(|| {
            McpCallError::ContractChanged {
                tool: exposed_name.to_string(),
            }
        })?;
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(McpCallError::ServerUnavailable {
                server: tool.server_id.clone(),
            });
        }
        let (route_lock, mut observed_generation, deadline) = {
            let servers = self.servers.read().await;
            let Some(server) = servers.get(&tool.server_id) else {
                return Err(McpCallError::Revoked {
                    tool: exposed_name.to_string(),
                });
            };
            if !server.config.tool_enabled(&tool.raw_name) {
                return Err(McpCallError::Revoked {
                    tool: exposed_name.to_string(),
                });
            }
            (
                server.route_lock.clone(),
                server.refresh.generation,
                started + server.config.call_timeout(),
            )
        };
        let arguments = match arguments {
            Value::Object(arguments) => arguments,
            _ => {
                return Err(McpCallError::Protocol {
                    message: "MCP tool arguments must be a JSON object".to_string(),
                });
            }
        };
        let operation = async {
            let result = loop {
                let _route = route_lock.lock().await;
                if self
                    .refresh_server_if_needed(&tool.server_id, observed_generation, deadline)
                    .await
                    == RefreshOutcome::DeadlineElapsed
                {
                    return Err(McpCallError::Timeout {
                        tool: exposed_name.to_string(),
                    });
                }
                observed_generation = self
                    .servers
                    .read()
                    .await
                    .get(&tool.server_id)
                    .map_or(observed_generation, |server| server.refresh.generation);
                let (client, tools_revision) = self.select_exact_route(tool, exposed_name).await?;
                let pin = client.pin();
                drop(_route);
                match client
                    .call(
                        pin,
                        deadline,
                        tools_revision,
                        &tool.raw_name,
                        arguments.clone(),
                    )
                    .await
                {
                    Err(McpClientCallError::ToolsChanged) => continue,
                    result => break result,
                }
            };
            let result = match result {
                Ok(result) => result,
                Err(McpClientCallError::Timeout) => {
                    self.mark_server_unavailable(&tool.server_id).await;
                    return Err(McpCallError::Timeout {
                        tool: exposed_name.to_string(),
                    });
                }
                Err(McpClientCallError::Protocol(message)) => {
                    self.mark_server_unavailable(&tool.server_id).await;
                    return Err(McpCallError::Protocol {
                        message: bounded_error_message(message),
                    });
                }
                Err(McpClientCallError::ToolsChanged) => {
                    unreachable!("handled in admission loop")
                }
            };
            let (output, is_error) = normalize_call_result(result);
            Ok(McpCallOutput { output, is_error })
        };
        tokio::time::timeout_at(deadline, operation)
            .await
            .unwrap_or_else(|_| {
                Err(McpCallError::Timeout {
                    tool: exposed_name.to_string(),
                })
            })
    }

    pub async fn tool_views(&self, snapshot: &McpSessionSnapshot) -> Vec<McpToolView> {
        let servers = self.servers.read().await;
        snapshot
            .manifest()
            .tools
            .iter()
            .map(|tool| McpToolView {
                server: tool.server_id.clone(),
                raw_name: tool.raw_name.clone(),
                exposed_name: tool.exposed_name.clone(),
                contract_fingerprint: tool.contract_fingerprint.clone(),
                health: servers
                    .get(&tool.server_id)
                    .map(|server| {
                        if server.config.tool_enabled(&tool.raw_name) {
                            server.health
                        } else {
                            McpHealth::Revoked
                        }
                    })
                    .unwrap_or(McpHealth::Revoked),
            })
            .collect()
    }

    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.shutdown_notify.notify_waiters();
        self.oauth.shutdown().await;
        let route_locks = self
            .servers
            .read()
            .await
            .values()
            .map(|server| server.route_lock.clone())
            .collect::<Vec<_>>();
        let mut route_guards = Vec::with_capacity(route_locks.len());
        for route_lock in &route_locks {
            route_guards.push(route_lock.lock().await);
        }
        let clients = self
            .servers
            .write()
            .await
            .values_mut()
            .filter_map(|server| server.client.take())
            .collect::<Vec<_>>();
        let shutdown = async {
            let tasks = clients
                .into_iter()
                .map(|client| async move { client.shutdown().await });
            futures_util::future::join_all(tasks).await;
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), shutdown).await;
        drop(route_guards);
    }

    pub async fn begin_oauth_login(
        &self,
        server_id: &str,
    ) -> Result<crate::McpOAuthLoginStart, crate::McpOAuthLoginError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(crate::McpOAuthLoginError::Unavailable);
        }
        let (config, deadline) = {
            let servers = self.servers.read().await;
            let server = servers
                .get(server_id)
                .ok_or(crate::McpOAuthLoginError::NotConfigured)?;
            let crate::config::McpTransportConfig::StreamableHttp(config) =
                &server.config.transport
            else {
                return Err(crate::McpOAuthLoginError::NotConfigured);
            };
            (
                config.clone(),
                tokio::time::Instant::now() + server.config.startup_timeout(),
            )
        };
        self.oauth.begin(server_id, &config, deadline).await
    }

    pub async fn complete_oauth_login(
        &self,
        server_id: &str,
        login_id: &str,
        callback_url: &str,
    ) -> Result<(), crate::McpOAuthLoginError> {
        self.oauth.complete(server_id, login_id, callback_url).await
    }

    pub async fn cancel_oauth_login(
        &self,
        server_id: &str,
        login_id: &str,
    ) -> Result<(), crate::McpOAuthLoginError> {
        self.oauth.cancel(server_id, login_id).await
    }

    pub async fn oauth_status(&self, server_id: &str) -> McpAuthStatus {
        self.oauth_status_detail(server_id).await.0
    }

    pub async fn auth_statuses(&self) -> Vec<McpAuthServerStatus> {
        let servers = self
            .servers
            .read()
            .await
            .iter()
            .map(|(server_id, server)| {
                let auth_kind = match &server.config.transport {
                    crate::config::McpTransportConfig::Stdio(_) => McpAuthKind::None,
                    crate::config::McpTransportConfig::StreamableHttp(config) => {
                        match config.auth.as_ref() {
                            Some(crate::McpHttpAuthConfig::BearerEnv { .. }) => McpAuthKind::Bearer,
                            Some(crate::McpHttpAuthConfig::Oauth { .. }) => McpAuthKind::Oauth,
                            None => McpAuthKind::None,
                        }
                    }
                };
                (server_id.clone(), auth_kind)
            })
            .collect::<Vec<_>>();
        let mut statuses = futures_util::stream::iter(servers)
            .map(|(server, auth_kind)| async move {
                let (auth_state, failure) = self.oauth_status_detail(&server).await;
                let can_login = auth_kind == McpAuthKind::Oauth
                    && failure != Some(McpAuthFailure::CredentialStoreUnavailable)
                    && matches!(
                        auth_state,
                        McpAuthStatus::LoginRequired
                            | McpAuthStatus::ReauthenticationRequired
                            | McpAuthStatus::Unknown
                    );
                let can_logout = auth_kind == McpAuthKind::Oauth
                    && failure != Some(McpAuthFailure::CredentialStoreUnavailable)
                    && matches!(
                        auth_state,
                        McpAuthStatus::OauthReady
                            | McpAuthStatus::ReauthenticationRequired
                            | McpAuthStatus::AuthorizationPending
                    );
                McpAuthServerStatus {
                    server,
                    auth_kind,
                    auth_state,
                    can_login,
                    can_logout,
                    failure,
                }
            })
            .buffer_unordered(64)
            .collect::<Vec<_>>()
            .await;
        statuses.sort_by(|left, right| left.server.cmp(&right.server));
        statuses
    }

    async fn oauth_status_detail(
        &self,
        server_id: &str,
    ) -> (McpAuthStatus, Option<McpAuthFailure>) {
        let (config, healthy) = {
            let servers = self.servers.read().await;
            let Some(server) = servers.get(server_id) else {
                return (McpAuthStatus::NonOauth, None);
            };
            let crate::config::McpTransportConfig::StreamableHttp(config) =
                &server.config.transport
            else {
                return (McpAuthStatus::NonOauth, None);
            };
            (config.clone(), server.is_healthy())
        };
        match config.auth.as_ref() {
            Some(crate::McpHttpAuthConfig::BearerEnv { .. }) => (McpAuthStatus::Bearer, None),
            Some(crate::McpHttpAuthConfig::Oauth { .. }) if self.oauth.is_pending(server_id) => {
                (McpAuthStatus::AuthorizationPending, None)
            }
            Some(crate::McpHttpAuthConfig::Oauth { .. }) if healthy => {
                (McpAuthStatus::OauthReady, None)
            }
            Some(crate::McpHttpAuthConfig::Oauth { .. }) => {
                match self.oauth_runtime.stored_status(server_id, &config).await {
                    Ok(crate::oauth_runtime::StoredOAuthStatus::Ready) => {
                        (McpAuthStatus::OauthReady, None)
                    }
                    Ok(crate::oauth_runtime::StoredOAuthStatus::ReauthenticationRequired) => {
                        (McpAuthStatus::ReauthenticationRequired, None)
                    }
                    Ok(crate::oauth_runtime::StoredOAuthStatus::Missing) => {
                        match self.oauth_runtime.discover(&config).await {
                            Ok(()) => (McpAuthStatus::LoginRequired, None),
                            Err(crate::oauth_runtime::OAuthRouteFailure::Unsupported) => {
                                (McpAuthStatus::Unsupported, None)
                            }
                            Err(
                                crate::oauth_runtime::OAuthRouteFailure::LoginRequired
                                | crate::oauth_runtime::OAuthRouteFailure::ReauthenticationRequired
                                | crate::oauth_runtime::OAuthRouteFailure::Unknown,
                            ) => (
                                McpAuthStatus::Unknown,
                                Some(McpAuthFailure::DiscoveryFailed),
                            ),
                            Err(crate::oauth_runtime::OAuthRouteFailure::Store) => (
                                McpAuthStatus::Unknown,
                                Some(McpAuthFailure::CredentialStoreUnavailable),
                            ),
                        }
                    }
                    Err(crate::oauth_runtime::OAuthRouteFailure::Unsupported) => {
                        (McpAuthStatus::Unsupported, None)
                    }
                    Err(
                        crate::oauth_runtime::OAuthRouteFailure::LoginRequired
                        | crate::oauth_runtime::OAuthRouteFailure::ReauthenticationRequired
                        | crate::oauth_runtime::OAuthRouteFailure::Unknown,
                    ) => (
                        McpAuthStatus::Unknown,
                        Some(McpAuthFailure::DiscoveryFailed),
                    ),
                    Err(crate::oauth_runtime::OAuthRouteFailure::Store) => (
                        McpAuthStatus::Unknown,
                        Some(McpAuthFailure::CredentialStoreUnavailable),
                    ),
                }
            }
            None => (McpAuthStatus::NonOauth, None),
        }
    }

    pub async fn logout_oauth(
        &self,
        server_id: &str,
    ) -> Result<McpLogoutResult, crate::OAuthCredentialStoreError> {
        let (route_lock, server_url) = {
            let servers = self.servers.read().await;
            let Some(server) = servers.get(server_id) else {
                return Ok(McpLogoutResult::NotFound);
            };
            let crate::config::McpTransportConfig::StreamableHttp(config) =
                &server.config.transport
            else {
                return Ok(McpLogoutResult::NotFound);
            };
            if config
                .auth
                .as_ref()
                .and_then(crate::McpHttpAuthConfig::oauth)
                .is_none()
            {
                return Ok(McpLogoutResult::NotFound);
            }
            (server.route_lock.clone(), config.url.clone())
        };
        let _route = route_lock.lock().await;
        self.oauth.cancel_active(server_id).await;
        let removed = self.oauth_runtime.logout(server_id, &server_url).await;
        let client = {
            let mut servers = self.servers.write().await;
            let server = servers
                .get_mut(server_id)
                .expect("OAuth server remains configured while route lock is held");
            let client = server.client.take();
            server.health = McpHealth::Unavailable;
            server.refresh.generation = server.refresh.generation.wrapping_add(1);
            server.refresh.disposition = RetryDisposition::UserActionRequired;
            client
        };
        if let Some(client) = client {
            client.shutdown().await;
        }
        let removed = removed?;
        Ok(if removed {
            McpLogoutResult::Removed
        } else {
            McpLogoutResult::NotFound
        })
    }

    async fn inventory_catalog(
        &self,
        first_party: &HashMap<ProviderKind, Vec<ProviderTool>>,
    ) -> Result<(McpSessionManifest, bool), McpManagerError> {
        let servers = self.servers.read().await;
        let builtin_names = first_party
            .values()
            .flatten()
            .flat_map(|tool| [tool.name.clone(), tool.canonical_name.clone()])
            .collect::<BTreeSet<_>>();
        let config_fingerprints = servers
            .iter()
            .map(|(server_id, server)| (server_id.clone(), server.config.semantic_fingerprint()))
            .collect::<BTreeMap<_, _>>();
        let tools = servers
            .values()
            .filter(|server| server.catalog_is_current())
            .flat_map(|server| server.tools.clone())
            .collect();
        let catalog = build_inventory_catalog(&config_fingerprints, tools, &builtin_names)?;
        let globally_coherent = servers.values().all(ServerState::catalog_is_current);
        Ok((catalog, globally_coherent))
    }

    async fn refresh_inventory(&self) {
        let routes = self.refresh_routes(|_, _| true).await;
        self.run_refresh_routes(routes).await;
    }

    async fn refresh_selection(&self, selected: &BTreeMap<String, BTreeSet<String>>) {
        let routes = self
            .refresh_routes(|server_id, server| {
                selected.contains_key(server_id) || !server.catalog_is_current()
            })
            .await;
        self.run_refresh_routes(routes).await;
    }

    async fn refresh_routes(
        &self,
        include: impl Fn(&str, &ServerState) -> bool,
    ) -> Vec<(String, Arc<Mutex<()>>, u64, tokio::time::Instant)> {
        self.servers
            .read()
            .await
            .iter()
            .filter(|(server_id, server)| include(server_id, server))
            .map(|(server_id, server)| {
                (
                    server_id.clone(),
                    server.route_lock.clone(),
                    server.refresh.generation,
                    tokio::time::Instant::now() + server.config.startup_timeout(),
                )
            })
            .collect()
    }

    async fn run_refresh_routes(
        &self,
        routes: Vec<(String, Arc<Mutex<()>>, u64, tokio::time::Instant)>,
    ) {
        futures_util::stream::iter(routes)
            .for_each_concurrent(
                None,
                |(server_id, route_lock, observed_generation, deadline)| async move {
                    let Ok(_route) = tokio::time::timeout_at(deadline, route_lock.lock()).await
                    else {
                        return;
                    };
                    self.refresh_server_if_needed(&server_id, observed_generation, deadline)
                        .await;
                },
            )
            .await;
    }

    async fn refresh_server_if_needed(
        &self,
        server_id: &str,
        observed_generation: u64,
        deadline: tokio::time::Instant,
    ) -> RefreshOutcome {
        let shutdown = self.shutdown_notify.notified();
        tokio::pin!(shutdown);
        shutdown.as_mut().enable();
        if self.shutting_down.load(Ordering::Acquire) {
            return RefreshOutcome::Complete;
        }
        let plan = {
            let mut servers = self.servers.write().await;
            let Some(server) = servers.get_mut(server_id) else {
                return RefreshOutcome::Complete;
            };
            if server.refresh.generation != observed_generation {
                return RefreshOutcome::Complete;
            }
            if server
                .client
                .as_ref()
                .is_some_and(|client| client.is_closed())
            {
                server.mark_unavailable();
            }
            let oauth_route = matches!(
                &server.config.transport,
                crate::config::McpTransportConfig::StreamableHttp(config)
                    if config
                        .auth
                        .as_ref()
                        .and_then(crate::McpHttpAuthConfig::oauth)
                        .is_some()
            );
            if !server.refresh.disposition.permits_automatic_attempt() && !oauth_route {
                return RefreshOutcome::Complete;
            }
            let reconnect = server.health != McpHealth::Healthy || server.client.is_none();
            let refresh = server.client.as_ref().is_some_and(|client| {
                client.tools_uncertain() || client.tools_revision() != server.catalog_tools_revision
            });
            (reconnect || refresh).then(|| {
                (
                    server.config.clone(),
                    reconnect,
                    refresh,
                    server.client.clone(),
                )
            })
        };
        let Some((config, reconnect, refreshing_stale_catalog, current_client)) = plan else {
            return RefreshOutcome::Complete;
        };
        if let Some(client) = &current_client {
            tokio::select! {
                () = client.wait_for_calls_idle() => {}
                () = &mut shutdown => return RefreshOutcome::Complete,
                () = tokio::time::sleep_until(deadline) => {
                    return RefreshOutcome::DeadlineElapsed;
                },
            }
        }
        let refresh = async {
            if reconnect {
                let oauth_token = oauth_access_token(&self.oauth_runtime, server_id, &config).await;
                let oauth_token = match oauth_token {
                    Ok(token) => token,
                    Err(disposition) => {
                        return RefreshAttempt::Failed {
                            error: anyhow::anyhow!("MCP OAuth authorization is required"),
                            disposition,
                        };
                    }
                };
                match McpClient::start(
                    &config,
                    deadline,
                    self.bearer_resolver.as_ref(),
                    oauth_token,
                )
                .await
                {
                    McpClientStart::Connected(client, tools) => {
                        let revision = client.tools_revision();
                        RefreshAttempt::Connected(client, tools, revision)
                    }
                    McpClientStart::OAuthLoginRequired => RefreshAttempt::Failed {
                        error: anyhow::anyhow!("MCP OAuth login is required"),
                        disposition: RetryDisposition::UserActionRequired,
                    },
                    McpClientStart::ConnectionFailed(error) => RefreshAttempt::Failed {
                        error,
                        disposition: RetryDisposition::Automatic,
                    },
                }
            } else if let Some(client) = current_client {
                match client.refresh_tools_until(deadline).await {
                    Ok((tools, revision)) => RefreshAttempt::Connected(client, tools, revision),
                    Err(error) => RefreshAttempt::Failed {
                        error,
                        disposition: RetryDisposition::Automatic,
                    },
                }
            } else {
                RefreshAttempt::Failed {
                    error: anyhow::anyhow!("MCP client disappeared before refresh"),
                    disposition: RetryDisposition::Automatic,
                }
            }
        };
        let refreshed = tokio::select! {
            refreshed = refresh => refreshed,
            () = &mut shutdown => return RefreshOutcome::Complete,
        };
        if self.shutting_down.load(Ordering::Acquire) {
            if let RefreshAttempt::Connected(client, _, _) = refreshed {
                client.shutdown_in_background();
            }
            return RefreshOutcome::Complete;
        }
        let mut servers = self.servers.write().await;
        let Some(server) = servers.get_mut(server_id) else {
            return RefreshOutcome::Complete;
        };
        if server.refresh.generation != observed_generation {
            drop(servers);
            if let RefreshAttempt::Connected(client, _, _) = refreshed {
                client.shutdown_in_background();
            }
            return RefreshOutcome::Complete;
        }
        server.refresh.generation = server.refresh.generation.wrapping_add(1);
        match refreshed {
            RefreshAttempt::Connected(client, tools, revision) => {
                let tools = match discovered_tools(server_id, &config, tools) {
                    Ok(tools) => tools,
                    Err(error) => {
                        eprintln!("MCP server {server_id} refresh failed: {error:#}");
                        drop(client);
                        if refreshing_stale_catalog {
                            server.catalog_coherent = false;
                        }
                        server.mark_unavailable();
                        return RefreshOutcome::Complete;
                    }
                };
                server.tools = tools;
                server.client = Some(client);
                server.health = McpHealth::Healthy;
                server.catalog_tools_revision = revision;
                server.catalog_coherent = true;
                server.refresh.disposition = RetryDisposition::Automatic;
            }
            RefreshAttempt::Failed { error, disposition } => {
                eprintln!("MCP server {server_id} refresh failed: {error:#}");
                if refreshing_stale_catalog {
                    server.catalog_coherent = false;
                }
                server.mark_unavailable();
                server.refresh.disposition = disposition;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            RefreshOutcome::DeadlineElapsed
        } else {
            RefreshOutcome::Complete
        }
    }

    async fn mark_server_unavailable(&self, server_id: &str) {
        if let Some(server) = self.servers.write().await.get_mut(server_id) {
            server.mark_unavailable();
            server.refresh.generation = server.refresh.generation.wrapping_add(1);
        }
    }

    async fn select_exact_route(
        &self,
        tool: &crate::McpManifestTool,
        exposed_name: &str,
    ) -> Result<(Arc<McpClient>, u64), McpCallError> {
        let servers = self.servers.read().await;
        let Some(server) = servers.get(&tool.server_id) else {
            return Err(McpCallError::Revoked {
                tool: exposed_name.to_string(),
            });
        };
        if !server.config.tool_enabled(&tool.raw_name) {
            return Err(McpCallError::Revoked {
                tool: exposed_name.to_string(),
            });
        }
        if server.health != McpHealth::Healthy {
            return Err(McpCallError::ServerUnavailable {
                server: tool.server_id.clone(),
            });
        }
        let matching_raw = server
            .tools
            .iter()
            .filter(|current| current.raw_name == tool.raw_name)
            .collect::<Vec<_>>();
        if matching_raw.is_empty() {
            return Err(McpCallError::ServerUnavailable {
                server: tool.server_id.clone(),
            });
        }
        if server.config.semantic_fingerprint() != tool.server_config_fingerprint
            || matching_raw
                .iter()
                .all(|current| contract_fingerprint(current) != tool.contract_fingerprint)
        {
            return Err(McpCallError::ContractChanged {
                tool: exposed_name.to_string(),
            });
        }
        let Some(client) = server.client.clone() else {
            return Err(McpCallError::ServerUnavailable {
                server: tool.server_id.clone(),
            });
        };
        Ok((client, server.catalog_tools_revision))
    }
}

fn unavailable_server(config: McpServerConfig, disposition: RetryDisposition) -> ServerState {
    ServerState {
        config,
        client: None,
        tools: Vec::new(),
        health: McpHealth::Unavailable,
        catalog_tools_revision: 0,
        catalog_coherent: true,
        route_lock: Arc::new(Mutex::new(())),
        refresh: RefreshState {
            generation: 1,
            disposition,
        },
    }
}

fn validate_selection_shape(
    selection: &McpSessionSelection,
) -> Result<BTreeMap<String, BTreeSet<String>>, McpManagerError> {
    if selection.inventory_revision.is_empty()
        || selection.inventory_revision.len() > MAX_REVISION_BYTES
        || selection.servers.len() > MAX_SELECTED_SERVERS
    {
        return Err(McpManagerError::SelectionInvalid {
            message: "invalid inventory revision or too many selected servers".to_string(),
        });
    }
    if selection
        .servers
        .windows(2)
        .any(|pair| !strictly_utf16_ordered(&pair[0].server, &pair[1].server))
    {
        return Err(McpManagerError::SelectionInvalid {
            message: "selected MCP server identities must be sorted and unique".to_string(),
        });
    }
    let mut selected = BTreeMap::new();
    let mut tool_count = 0_usize;
    for server in &selection.servers {
        if server.server.is_empty()
            || server.server.len() > MAX_SERVER_ID_BYTES
            || server.tools.is_empty()
        {
            return Err(McpManagerError::SelectionInvalid {
                message: "selected servers and tool lists must be nonempty".to_string(),
            });
        }
        if server
            .tools
            .windows(2)
            .any(|pair| !strictly_utf16_ordered(&pair[0], &pair[1]))
        {
            return Err(McpManagerError::SelectionInvalid {
                message: format!(
                    "MCP server {} tool identities must be sorted and unique",
                    server.server
                ),
            });
        }
        let tools = server.tools.iter().cloned().collect::<BTreeSet<_>>();
        if tools
            .iter()
            .any(|tool| tool.is_empty() || tool.len() > MAX_RAW_TOOL_NAME_BYTES)
        {
            return Err(McpManagerError::SelectionInvalid {
                message: format!(
                    "MCP server {} has an empty or over-limit tool name",
                    server.server
                ),
            });
        }
        tool_count = tool_count.saturating_add(tools.len());
        if selected.insert(server.server.clone(), tools).is_some() {
            return Err(McpManagerError::SelectionInvalid {
                message: format!("duplicate MCP server {}", server.server),
            });
        }
    }
    if tool_count > MAX_TOOLS {
        return Err(McpManagerError::SelectionInvalid {
            message: format!("MCP selection has more than {MAX_TOOLS} tools"),
        });
    }
    Ok(selected)
}

fn strictly_utf16_ordered(left: &str, right: &str) -> bool {
    left.encode_utf16().cmp(right.encode_utf16()).is_lt()
}

fn contract_fingerprint(tool: &DiscoveredTool) -> String {
    crate::fingerprint_json(&serde_json::json!({
        "server_id": tool.server_id,
        "raw_name": tool.raw_name,
        "description": tool.description,
        "input_schema": crate::canonical_json(&tool.input_schema),
    }))
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "manager_oauth_tests.rs"]
mod oauth_tests;
