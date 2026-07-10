use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::transport::auth::{AuthError, OAuthClientConfig, OAuthHttpClient, OAuthState};
use rmcp::transport::AuthorizationManager;
use rmcp::transport::AuthorizationSession;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::{McpHttpAuthConfig, McpStreamableHttpTransportConfig};
use crate::oauth_callback::{respond, CallbackListener, CallbackRequest};
#[cfg(test)]
use crate::oauth_credentials::OAuthCredentialRepository;
use crate::oauth_credentials::StoredOAuthCredential;
use crate::oauth_http::DirectOAuthClient;
use crate::oauth_runtime::OAuthRuntimeManager;

const CONTROL_CAPACITY: usize = 1;
const MAX_CALLBACK_URL_BYTES: usize = 16 * 1024;
const MAX_AUTHORIZATION_URL_BYTES: usize = 16 * 1024;
static NEXT_LOGIN_ID: AtomicU64 = AtomicU64::new(1);
type StartResult = Result<McpOAuthLoginStart, McpOAuthLoginError>;
type StartSender = oneshot::Sender<StartResult>;
type ResponseSender = oneshot::Sender<Result<(), McpOAuthLoginError>>;
#[cfg(test)]
type FinalizationBarriers = (Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>);

#[derive(PartialEq, Eq)]
pub struct McpOAuthLoginStart {
    pub login_id: String,
    pub authorization_url: String,
    pub expires_at_unix_seconds: u64,
}

fn map_callback_error(error: AuthError) -> McpOAuthLoginError {
    match error {
        AuthError::InternalError(_)
        | AuthError::AuthorizationFailed(_)
        | AuthError::AuthorizationServerMismatch { .. }
        | AuthError::AuthorizationServerMissingIssuer { .. } => McpOAuthLoginError::InvalidCallback,
        error => map_auth_error(error),
    }
}

impl fmt::Debug for McpOAuthLoginStart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthLoginStart")
            .field("login_id", &self.login_id)
            .field("authorization_url", &"<redacted>")
            .field("expires_at_unix_seconds", &self.expires_at_unix_seconds)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpOAuthLoginError {
    #[error("oauth_login_not_configured")]
    NotConfigured,
    #[error("oauth_login_already_pending")]
    AlreadyPending,
    #[error("oauth_login_not_found")]
    NotFound,
    #[error("oauth_login_already_completed")]
    AlreadyCompleted,
    #[error("oauth_login_cancelled")]
    Cancelled,
    #[error("oauth_login_expired")]
    Expired,
    #[error("oauth_discovery_failed")]
    Discovery,
    #[error("oauth_registration_failed")]
    Registration,
    #[error("oauth_callback_bind_failed")]
    CallbackBind,
    #[error("oauth_callback_invalid")]
    InvalidCallback,
    #[error("oauth_provider_error")]
    Provider,
    #[error("oauth_token_endpoint_error")]
    TokenEndpoint,
    #[error("oauth_credential_store_failed")]
    Persistence,
    #[error("oauth_network_failed")]
    Network,
    #[error("oauth_login_unavailable")]
    Unavailable,
    #[error("oauth_authorization_url_too_long")]
    AuthorizationUrlTooLong,
}

pub(crate) struct OAuthCoordinator {
    state: StdMutex<CoordinatorState>,
    runtime: Arc<OAuthRuntimeManager>,
    shutdown: CancellationToken,
    shutdown_lock: Mutex<()>,
    #[cfg(test)]
    finalization_barriers: StdMutex<Option<FinalizationBarriers>>,
    #[cfg(test)]
    acknowledgement_barriers: StdMutex<Option<FinalizationBarriers>>,
    #[cfg(test)]
    persistence_barriers: StdMutex<Option<FinalizationBarriers>>,
}

#[derive(Default)]
struct CoordinatorState {
    shutting_down: bool,
    active_by_server: BTreeMap<String, String>,
    flows: BTreeMap<String, FlowHandle>,
    credentials: BTreeMap<String, String>,
    tasks: Vec<JoinHandle<()>>,
}

struct FlowHandle {
    control: mpsc::Sender<FlowControl>,
    cancel: CancellationToken,
    cancel_response: Option<ResponseSender>,
    phase: FlowPhase,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FlowPhase {
    Active,
    Committing,
}

enum FlowControl {
    Complete {
        callback_url: String,
        response: ResponseSender,
    },
}

struct CompletedCredential {
    login_id: String,
    stored: StoredOAuthCredential,
    oauth_state: OAuthState,
}

struct FlowSetup {
    server_id: String,
    login_id: String,
    server_url: String,
    client_id: Option<String>,
    scopes: Option<Vec<String>>,
    resource: Option<String>,
    callback_port: Option<u16>,
    callback_timeout: Duration,
    operation_deadline: Instant,
    login_generation: u64,
    control_rx: Option<mpsc::Receiver<FlowControl>>,
    cancel: CancellationToken,
}

struct PreparedFlow {
    listener: CallbackListener,
    oauth_state: Option<OAuthState>,
    redirect_uri: String,
    expected_state: String,
    requested_scopes: Vec<String>,
    deadline: Instant,
    control_rx: mpsc::Receiver<FlowControl>,
    cancel: CancellationToken,
}

struct FlowOutcome {
    result: Result<CompletedCredential, McpOAuthLoginError>,
    acknowledgement: Option<ResponseSender>,
    browser_stream: Option<tokio::net::TcpStream>,
}

impl FlowOutcome {
    fn complete(
        result: Result<CompletedCredential, McpOAuthLoginError>,
        acknowledgement: Option<ResponseSender>,
    ) -> Self {
        Self {
            result,
            acknowledgement,
            browser_stream: None,
        }
    }
}

impl OAuthCoordinator {
    #[cfg(test)]
    pub(crate) fn new() -> Arc<Self> {
        let repository = OAuthCredentialRepository::memory();
        let runtime = OAuthRuntimeManager::new(repository.clone());
        Self::with_runtime(runtime)
    }

    pub(crate) fn with_runtime(runtime: Arc<OAuthRuntimeManager>) -> Arc<Self> {
        Arc::new(Self {
            state: StdMutex::new(CoordinatorState::default()),
            runtime,
            shutdown: CancellationToken::new(),
            shutdown_lock: Mutex::new(()),
            #[cfg(test)]
            finalization_barriers: StdMutex::new(None),
            #[cfg(test)]
            acknowledgement_barriers: StdMutex::new(None),
            #[cfg(test)]
            persistence_barriers: StdMutex::new(None),
        })
    }

    #[cfg(test)]
    pub(crate) fn set_persistence_barriers(
        &self,
        barriers: (Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>),
    ) {
        *self
            .persistence_barriers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(barriers);
    }

    pub(crate) async fn begin(
        self: &Arc<Self>,
        server_id: &str,
        config: &McpStreamableHttpTransportConfig,
        operation_deadline: Instant,
    ) -> Result<McpOAuthLoginStart, McpOAuthLoginError> {
        let oauth = config
            .auth
            .as_ref()
            .and_then(McpHttpAuthConfig::oauth)
            .ok_or(McpOAuthLoginError::NotConfigured)?;
        let login_id = format!("{:016x}", NEXT_LOGIN_ID.fetch_add(1, Ordering::Relaxed));
        self.runtime
            .store_available()
            .map_err(|_| McpOAuthLoginError::Persistence)?;
        let login_generation = self.runtime.login_generation(server_id, &config.url).await;
        let (start_tx, start_rx) = oneshot::channel::<StartResult>();
        let (control_tx, control_rx) = mpsc::channel(CONTROL_CAPACITY);
        let cancel = self.shutdown.child_token();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.shutting_down {
                return Err(McpOAuthLoginError::Unavailable);
            }
            state.tasks.retain(|task| !task.is_finished());
            if state.active_by_server.contains_key(server_id) {
                return Err(McpOAuthLoginError::AlreadyPending);
            }
            state
                .active_by_server
                .insert(server_id.to_string(), login_id.clone());
            state.flows.insert(
                login_id.clone(),
                FlowHandle {
                    control: control_tx,
                    cancel: cancel.clone(),
                    cancel_response: None,
                    phase: FlowPhase::Active,
                },
            );
            let task = tokio::spawn(self.clone().run_flow(
                FlowSetup {
                    server_id: server_id.to_string(),
                    login_id,
                    server_url: config.url.clone(),
                    client_id: oauth.normalized_client_id().map(ToString::to_string),
                    scopes: oauth.scopes.map(<[String]>::to_vec),
                    resource: oauth.resource.map(ToString::to_string),
                    callback_port: oauth.callback_port,
                    callback_timeout: Duration::from_millis(oauth.callback_timeout_ms),
                    operation_deadline,
                    login_generation,
                    control_rx: Some(control_rx),
                    cancel,
                },
                start_tx,
            ));
            state.tasks.push(task);
        }
        start_rx.await.unwrap_or(Err(McpOAuthLoginError::Cancelled))
    }

    pub(crate) async fn complete(
        &self,
        server_id: &str,
        login_id: &str,
        callback_url: &str,
    ) -> Result<(), McpOAuthLoginError> {
        if callback_url.len() > MAX_CALLBACK_URL_BYTES {
            return Err(McpOAuthLoginError::InvalidCallback);
        }
        let control = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.active_by_server.get(server_id).map(String::as_str) != Some(login_id) {
                return Err(McpOAuthLoginError::NotFound);
            }
            state
                .flows
                .get(login_id)
                .ok_or(McpOAuthLoginError::NotFound)?
                .control
                .clone()
        };
        let (response_tx, response_rx) = oneshot::channel();
        control
            .send(FlowControl::Complete {
                callback_url: callback_url.to_string(),
                response: response_tx,
            })
            .await
            .map_err(|_| McpOAuthLoginError::AlreadyCompleted)?;
        response_rx
            .await
            .unwrap_or(Err(McpOAuthLoginError::AlreadyCompleted))
    }

    pub(crate) fn is_pending(&self, server_id: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_by_server
            .contains_key(server_id)
    }

    pub(crate) async fn cancel_active(&self, server_id: &str) {
        let login_id = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_by_server
            .get(server_id)
            .cloned();
        if let Some(login_id) = login_id {
            let _ = self.cancel(server_id, &login_id).await;
        }
    }

    pub(crate) async fn cancel(
        &self,
        server_id: &str,
        login_id: &str,
    ) -> Result<(), McpOAuthLoginError> {
        let (response_tx, response_rx) = oneshot::channel();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match state.active_by_server.get(server_id) {
                Some(active_login_id) if active_login_id == login_id => {}
                Some(_) => return Err(McpOAuthLoginError::NotFound),
                None if state
                    .credentials
                    .get(server_id)
                    .is_some_and(|completed_login_id| completed_login_id == login_id) =>
                {
                    return Err(McpOAuthLoginError::AlreadyCompleted);
                }
                None => return Err(McpOAuthLoginError::NotFound),
            }
            let flow = state
                .flows
                .get_mut(login_id)
                .ok_or(McpOAuthLoginError::NotFound)?;
            if flow.phase == FlowPhase::Committing {
                return Err(McpOAuthLoginError::AlreadyCompleted);
            }
            if flow.cancel_response.is_some() {
                return Err(McpOAuthLoginError::AlreadyCompleted);
            }
            flow.cancel_response = Some(response_tx);
            flow.cancel.cancel();
        }
        response_rx
            .await
            .unwrap_or(Err(McpOAuthLoginError::AlreadyCompleted))
    }

    pub(crate) async fn shutdown(&self) {
        let _shutdown = self.shutdown_lock.lock().await;
        let tasks = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.shutting_down {
                return;
            }
            state.shutting_down = true;
            self.shutdown.cancel();
            std::mem::take(&mut state.tasks)
        };
        for task in tasks {
            let _ = task.await;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active_by_server.clear();
        state.flows.clear();
        state.credentials.clear();
    }

    async fn run_flow(self: Arc<Self>, mut setup: FlowSetup, start_tx: StartSender) {
        let mut start_tx = Some(start_tx);
        let outcome = match self.prepare_flow(&mut setup, &mut start_tx).await {
            Ok(mut flow) => {
                let outcome = self.run_prepared_flow(&setup, &mut flow).await;
                #[cfg(test)]
                if outcome.result.is_ok() {
                    let barriers = self
                        .finalization_barriers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    if let Some((reached, release)) = barriers {
                        reached.wait().await;
                        release.wait().await;
                    }
                }
                drop(flow);
                outcome
            }
            Err(error) => FlowOutcome {
                result: Err(error),
                acknowledgement: None,
                browser_stream: None,
            },
        };

        let FlowOutcome {
            mut result,
            acknowledgement,
            mut browser_stream,
        } = outcome;
        let mut cancel_response = None;
        let committing = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state
                .flows
                .get(&setup.login_id)
                .is_some_and(|flow| flow.cancel.is_cancelled())
            {
                result = Err(McpOAuthLoginError::Cancelled);
            }
            if result.is_ok() {
                if let Some(flow) = state.flows.get_mut(&setup.login_id) {
                    flow.phase = FlowPhase::Committing;
                }
                true
            } else {
                cancel_response = state
                    .flows
                    .remove(&setup.login_id)
                    .and_then(|flow| flow.cancel_response);
                if state.active_by_server.get(&setup.server_id) == Some(&setup.login_id) {
                    state.active_by_server.remove(&setup.server_id);
                }
                false
            }
        };
        #[cfg(test)]
        if committing {
            let barriers = self
                .persistence_barriers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some((reached, release)) = barriers {
                reached.wait().await;
                release.wait().await;
            }
        }
        let completed_login_id = match result {
            Ok(credential) => {
                let stored = credential.stored.clone();
                match self
                    .runtime
                    .install_durable(stored, credential.oauth_state, setup.login_generation)
                    .await
                {
                    Ok(()) => Ok(credential.login_id),
                    Err(crate::oauth_runtime::OAuthRouteFailure::Store) => {
                        Err(McpOAuthLoginError::Persistence)
                    }
                    Err(
                        crate::oauth_runtime::OAuthRouteFailure::LoginRequired
                        | crate::oauth_runtime::OAuthRouteFailure::ReauthenticationRequired
                        | crate::oauth_runtime::OAuthRouteFailure::Unsupported
                        | crate::oauth_runtime::OAuthRouteFailure::Unknown,
                    ) => Err(McpOAuthLoginError::Unavailable),
                }
            }
            Err(error) => Err(error),
        };
        let response_result = completed_login_id.map(|_| ());
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if committing {
                cancel_response = state
                    .flows
                    .remove(&setup.login_id)
                    .and_then(|flow| flow.cancel_response);
                if state.active_by_server.get(&setup.server_id) == Some(&setup.login_id) {
                    state.active_by_server.remove(&setup.server_id);
                }
            }
            if response_result.is_ok() {
                state
                    .credentials
                    .insert(setup.server_id.clone(), setup.login_id.clone());
            }
            debug_assert!(!state.flows.contains_key(&setup.login_id));
            debug_assert!(state.active_by_server.get(&setup.server_id) != Some(&setup.login_id));
        };
        #[cfg(test)]
        if response_result.is_ok() {
            let barriers = self
                .acknowledgement_barriers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some((reached, release)) = barriers {
                reached.wait().await;
                release.wait().await;
            }
        }
        if let Some(response) = acknowledgement {
            let _ = response.send(response_result.clone());
        }
        if let Some(response) = start_tx.take() {
            let error = response_result
                .clone()
                .expect_err("the login start is acknowledged before callback completion");
            let _ = response.send(Err(error));
        }
        if let Some(response) = cancel_response {
            let _ = response.send(if committing {
                Err(McpOAuthLoginError::AlreadyCompleted)
            } else {
                Ok(())
            });
        }
        if let Some(stream) = browser_stream.as_mut() {
            respond(stream, response_result.is_ok()).await;
        }
    }

    async fn prepare_flow(
        &self,
        setup: &mut FlowSetup,
        start_tx: &mut Option<StartSender>,
    ) -> Result<PreparedFlow, McpOAuthLoginError> {
        let callback_path = format!("/oauth/callback/{}", setup.login_id);
        let listener = CallbackListener::bind(setup.callback_port, callback_path)
            .await
            .map_err(|_| McpOAuthLoginError::CallbackBind)?;
        let redirect_uri = listener.redirect_uri();
        let http_client: Arc<dyn OAuthHttpClient> =
            Arc::new(DirectOAuthClient::build().map_err(|_| McpOAuthLoginError::Network)?);
        let oauth_state = tokio::select! {
            () = setup.cancel.cancelled() => {
                return Err(McpOAuthLoginError::Cancelled);
            }
            result = tokio::time::timeout_at(
                setup.operation_deadline,
                start_authorization(
                    &setup.server_url,
                    http_client,
                    setup.client_id.as_deref(),
                    setup.scopes.as_deref(),
                    &redirect_uri,
                ),
            ) => match result {
                Ok(Ok(state)) => state,
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(McpOAuthLoginError::Discovery),
            },
        };
        let authorization_url = append_query_param(
            &oauth_state
                .get_authorization_url()
                .await
                .map_err(map_auth_error)?,
            "resource",
            setup.resource.as_deref(),
        );
        if authorization_url.len() > MAX_AUTHORIZATION_URL_BYTES {
            return Err(McpOAuthLoginError::AuthorizationUrlTooLong);
        }
        let expected_state = reqwest::Url::parse(&authorization_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find_map(|(name, value)| (name == "state").then(|| value.into_owned()))
            })
            .ok_or(McpOAuthLoginError::Unavailable)?;
        let requested_scopes = reqwest::Url::parse(&authorization_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find_map(|(name, value)| (name == "scope").then(|| value.into_owned()))
            })
            .map(|scopes| {
                scopes
                    .split_ascii_whitespace()
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let deadline = Instant::now() + setup.callback_timeout;
        let expires_at_unix_seconds = unix_seconds()
            .checked_add(setup.callback_timeout.as_secs().max(1))
            .ok_or(McpOAuthLoginError::Unavailable)?;
        {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.shutting_down
                || state.active_by_server.get(&setup.server_id) != Some(&setup.login_id)
            {
                return Err(McpOAuthLoginError::Cancelled);
            }
        }
        if let Some(start_tx) = start_tx.take() {
            let _ = start_tx.send(Ok(McpOAuthLoginStart {
                login_id: setup.login_id.clone(),
                authorization_url,
                expires_at_unix_seconds,
            }));
        }

        Ok(PreparedFlow {
            listener,
            oauth_state: Some(oauth_state),
            redirect_uri,
            expected_state,
            requested_scopes,
            deadline,
            control_rx: setup
                .control_rx
                .take()
                .expect("flow control receiver is prepared once"),
            cancel: setup.cancel.clone(),
        })
    }

    async fn run_prepared_flow(&self, setup: &FlowSetup, flow: &mut PreparedFlow) -> FlowOutcome {
        loop {
            enum Source {
                Control(FlowControl),
                Listener {
                    stream: tokio::net::TcpStream,
                    request: CallbackRequest,
                },
            }
            let source = tokio::select! {
                () = flow.cancel.cancelled() => {
                    return FlowOutcome::complete(Err(McpOAuthLoginError::Cancelled), None);
                }
                control = flow.control_rx.recv() => {
                    let Some(control) = control else {
                        return FlowOutcome::complete(Err(McpOAuthLoginError::Cancelled), None);
                    };
                    Source::Control(control)
                }
                listener_request = flow.listener.next(&self.shutdown, flow.deadline) => {
                    match listener_request {
                        Ok(Some((stream, request))) => Source::Listener { stream, request },
                        Ok(None) => {
                            return FlowOutcome::complete(Err(McpOAuthLoginError::Expired), None);
                        }
                        Err(_) => {
                            return FlowOutcome::complete(
                                Err(McpOAuthLoginError::InvalidCallback),
                                None,
                            );
                        }
                    }
                }
            };
            let (callback_url, response, mut stream) = match source {
                Source::Control(FlowControl::Complete {
                    callback_url,
                    response,
                }) => (callback_url, Some(response), None),
                Source::Listener {
                    mut stream,
                    request: CallbackRequest::ProviderError(url),
                } => {
                    if !callback_has_state(&url, &flow.expected_state) {
                        respond(&mut stream, false).await;
                        continue;
                    }
                    respond(&mut stream, false).await;
                    return FlowOutcome::complete(Err(McpOAuthLoginError::Provider), None);
                }
                Source::Listener {
                    mut stream,
                    request: CallbackRequest::Invalid,
                } => {
                    respond(&mut stream, false).await;
                    continue;
                }
                Source::Listener {
                    stream,
                    request: CallbackRequest::Url(url),
                } => (url, None, Some(stream)),
            };

            let parsed = reqwest::Url::parse(&callback_url).ok();
            if parsed.as_ref().is_none_or(|url| {
                url.fragment().is_some()
                    || url.as_str().split_once('?').map(|(base, _)| base)
                        != Some(flow.redirect_uri.as_str())
            }) {
                if let Some(stream) = stream.as_mut() {
                    respond(stream, false).await;
                    continue;
                }
                return FlowOutcome::complete(Err(McpOAuthLoginError::InvalidCallback), response);
            }
            if parsed
                .as_ref()
                .is_some_and(|url| url.query_pairs().any(|(name, _)| name == "error"))
            {
                let error = if callback_has_state(&callback_url, &flow.expected_state) {
                    McpOAuthLoginError::Provider
                } else {
                    McpOAuthLoginError::InvalidCallback
                };
                if let Some(stream) = stream.as_mut() {
                    respond(stream, false).await;
                }
                return FlowOutcome::complete(Err(error), response);
            }

            let result = tokio::select! {
                () = flow.cancel.cancelled() => Err(McpOAuthLoginError::Cancelled),
                result = tokio::time::timeout_at(
                    flow.deadline,
                    flow.oauth_state
                        .as_mut()
                        .expect("OAuth state is present until completion")
                        .handle_callback_url(&callback_url),
                ) => match result {
                    Ok(result) => result.map_err(map_callback_error),
                    Err(_) => Err(McpOAuthLoginError::Expired),
                },
            };
            if let Err(error) = result {
                if let Some(stream) = stream.as_mut() {
                    respond(stream, false).await;
                }
                return FlowOutcome::complete(Err(error), response);
            }
            let oauth_state = flow
                .oauth_state
                .take()
                .expect("OAuth state is present until completion");
            let credentials = oauth_state
                .get_credentials()
                .await
                .map_err(map_auth_error)
                .and_then(|(client_id, credentials)| {
                    credentials
                        .map(|credentials| (client_id, credentials))
                        .ok_or(McpOAuthLoginError::TokenEndpoint)
                });
            let result = credentials.map(|(client_id, credentials)| CompletedCredential {
                login_id: setup.login_id.clone(),
                stored: StoredOAuthCredential::from_token_response(
                    setup.server_id.clone(),
                    setup.server_url.clone(),
                    setup.client_id.clone(),
                    setup.resource.clone(),
                    client_id,
                    &credentials,
                    &flow.requested_scopes,
                ),
                oauth_state,
            });
            if result.is_err() {
                if let Some(stream) = stream.as_mut() {
                    respond(stream, false).await;
                }
            }
            return FlowOutcome {
                browser_stream: result.is_ok().then_some(stream).flatten(),
                result,
                acknowledgement: response,
            };
        }
    }
}

async fn start_authorization(
    server_url: &str,
    http_client: Arc<dyn OAuthHttpClient>,
    client_id: Option<&str>,
    scopes: Option<&[String]>,
    redirect_uri: &str,
) -> Result<OAuthState, McpOAuthLoginError> {
    let configured_scopes = scopes;
    let scopes = configured_scopes.unwrap_or_default();
    let scope_refs = scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let Some(client_id) = client_id.filter(|client_id| !client_id.trim().is_empty()) else {
        let mut oauth_state = OAuthState::new_with_oauth_http_client(server_url, http_client)
            .await
            .map_err(map_auth_error)?;
        oauth_state
            .start_authorization(&scope_refs, redirect_uri, Some("pi-relay"))
            .await
            .map_err(map_auth_error)?;
        return Ok(oauth_state);
    };

    let mut manager =
        AuthorizationManager::new_with_oauth_http_client(server_url, http_client.clone())
            .await
            .map_err(map_auth_error)?;
    let metadata = crate::oauth_discovery::discover_metadata(&manager)
        .await
        .map_err(map_auth_error)?;
    let scopes = configured_scopes
        .map(<[String]>::to_vec)
        .or_else(|| crate::oauth_discovery::normalize_scopes(metadata.scopes_supported.clone()))
        .unwrap_or_default();
    manager.set_metadata(metadata);
    manager
        .configure_client(
            OAuthClientConfig::new(client_id, redirect_uri).with_scopes(scopes.clone()),
        )
        .map_err(map_auth_error)?;
    let scope_refs = scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let authorization_url = manager
        .get_authorization_url(&scope_refs)
        .await
        .map_err(map_auth_error)?;
    Ok(OAuthState::Session(
        AuthorizationSession::for_scope_upgrade(manager, authorization_url, redirect_uri),
    ))
}

fn append_query_param(url: &str, key: &str, value: Option<&str>) -> String {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return url.to_string();
    };
    let Ok(mut url) = reqwest::Url::parse(url) else {
        return url.to_string();
    };
    url.query_pairs_mut().append_pair(key, value);
    url.to_string()
}

fn callback_has_state(callback_url: &str, expected_state: &str) -> bool {
    reqwest::Url::parse(callback_url).ok().is_some_and(|url| {
        let mut states = url
            .query_pairs()
            .filter_map(|(name, value)| (name == "state").then_some(value));
        states.next().is_some_and(|state| state == expected_state) && states.next().is_none()
    })
}

fn map_auth_error(error: AuthError) -> McpOAuthLoginError {
    match error {
        AuthError::NoAuthorizationSupport | AuthError::MetadataError(_) => {
            McpOAuthLoginError::Discovery
        }
        AuthError::RegistrationFailed(_) => McpOAuthLoginError::Registration,
        AuthError::TokenExchangeFailed(_)
        | AuthError::InvalidTokenType(_)
        | AuthError::TokenExpired => McpOAuthLoginError::TokenEndpoint,
        AuthError::AuthorizationFailed(_)
        | AuthError::AuthorizationServerMismatch { .. }
        | AuthError::AuthorizationServerMissingIssuer { .. } => McpOAuthLoginError::InvalidCallback,
        AuthError::AuthorizationRequired => McpOAuthLoginError::Provider,
        AuthError::HttpError(_) => McpOAuthLoginError::Network,
        AuthError::OAuthError(_)
        | AuthError::UrlError(_)
        | AuthError::InternalError(_)
        | AuthError::InvalidScope(_)
        | AuthError::InsufficientScope { .. }
        | AuthError::TokenRefreshFailed(_)
        | AuthError::ClientCredentialsError(_) => McpOAuthLoginError::Unavailable,
        _ => McpOAuthLoginError::Unavailable,
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
#[path = "oauth_login_tests.rs"]
pub(crate) mod tests;
