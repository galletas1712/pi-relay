use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_mcp_types::OAuthCredentialStoreError;
use rmcp::transport::auth::{AuthError, OAuthState, OAuthTokenResponse};
use rmcp::transport::AuthorizationManager;
use tokio::sync::Mutex;

use crate::config::{McpHttpAuthConfig, McpStreamableHttpTransportConfig};
use crate::oauth_credentials::{unix_millis, OAuthCredentialRepository, StoredOAuthCredential};
use crate::oauth_http::DirectOAuthClient;

const REFRESH_SKEW_MILLIS: u64 = 30_000;

#[derive(Clone)]
pub(crate) struct OAuthAccessToken {
    secret: Arc<str>,
    rejected: Arc<AtomicBool>,
}

#[cfg(test)]
#[path = "oauth_runtime_tests.rs"]
mod tests;

impl OAuthAccessToken {
    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }

    pub(crate) fn mark_rejected(&self) {
        self.rejected.store(true, Ordering::Release);
    }
}

impl std::fmt::Debug for OAuthAccessToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OAuthAccessToken(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OAuthRouteFailure {
    LoginRequired,
    ReauthenticationRequired,
    Unsupported,
    Unknown,
    Store,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoredOAuthStatus {
    Missing,
    Ready,
    ReauthenticationRequired,
}

struct OAuthRuntime {
    manager: AuthorizationManager,
    credential: StoredOAuthCredential,
    rejected: Arc<AtomicBool>,
    failure: Option<OAuthRouteFailure>,
}

#[derive(Default)]
struct OAuthRuntimeSlot {
    runtime: Option<OAuthRuntime>,
    logout_generation: u64,
    logged_out: bool,
}

type RuntimeSlot = Arc<Mutex<OAuthRuntimeSlot>>;

pub(crate) struct OAuthRuntimeManager {
    repository: Arc<OAuthCredentialRepository>,
    slots: Mutex<BTreeMap<String, RuntimeSlot>>,
}

impl OAuthRuntimeManager {
    pub(crate) fn new(repository: Arc<OAuthCredentialRepository>) -> Arc<Self> {
        Arc::new(Self {
            repository,
            slots: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) async fn access_token(
        &self,
        server_id: &str,
        config: &McpStreamableHttpTransportConfig,
    ) -> Result<OAuthAccessToken, OAuthRouteFailure> {
        let oauth = config
            .auth
            .as_ref()
            .and_then(McpHttpAuthConfig::oauth)
            .ok_or(OAuthRouteFailure::Unsupported)?;
        let slot = self.slot(server_id, &config.url).await;
        let mut slot = slot.lock().await;
        if slot.logged_out {
            return Err(OAuthRouteFailure::LoginRequired);
        }
        if slot.runtime.is_none() {
            let credential = self
                .repository
                .get(server_id, &config.url)
                .await
                .map_err(|_| OAuthRouteFailure::Store)?
                .ok_or(OAuthRouteFailure::LoginRequired)?;
            if !credential.is_compatible(
                server_id,
                &config.url,
                oauth.normalized_client_id(),
                oauth.scopes,
                oauth.resource,
            ) {
                return Err(OAuthRouteFailure::LoginRequired);
            }
            let manager = restore_manager(&credential).await?;
            slot.runtime = Some(OAuthRuntime {
                manager,
                credential,
                rejected: Arc::new(AtomicBool::new(false)),
                failure: None,
            });
        }
        let result = slot
            .runtime
            .as_mut()
            .expect("OAuth runtime was restored")
            .access_token(&self.repository)
            .await;
        if matches!(result, Err(OAuthRouteFailure::Store)) {
            slot.runtime = None;
        }
        result
    }

    pub(crate) fn store_available(&self) -> Result<(), OAuthRouteFailure> {
        self.repository
            .availability()
            .map_err(|_| OAuthRouteFailure::Store)
    }

    pub(crate) async fn stored_status(
        &self,
        server_id: &str,
        config: &McpStreamableHttpTransportConfig,
    ) -> Result<StoredOAuthStatus, OAuthRouteFailure> {
        let oauth = config
            .auth
            .as_ref()
            .and_then(McpHttpAuthConfig::oauth)
            .ok_or(OAuthRouteFailure::Unsupported)?;
        let key = format!("{server_id}\u{0}{}", config.url);
        if let Some(slot) = self.slots.lock().await.get(&key).cloned() {
            let slot = slot.lock().await;
            if slot.logged_out {
                return Ok(StoredOAuthStatus::Missing);
            }
            if slot.runtime.as_ref().is_some_and(|runtime| {
                runtime.failure == Some(OAuthRouteFailure::ReauthenticationRequired)
            }) {
                return Ok(StoredOAuthStatus::ReauthenticationRequired);
            }
        }
        let Some(credential) = self
            .repository
            .get(server_id, &config.url)
            .await
            .map_err(|_| OAuthRouteFailure::Store)?
        else {
            return Ok(StoredOAuthStatus::Missing);
        };
        if !credential.is_compatible(
            server_id,
            &config.url,
            oauth.normalized_client_id(),
            oauth.scopes,
            oauth.resource,
        ) {
            return Ok(StoredOAuthStatus::Missing);
        }
        let needs_refresh = credential.expires_at_millis.is_some_and(|expires_at| {
            unix_millis().saturating_add(REFRESH_SKEW_MILLIS) >= expires_at
        });
        if needs_refresh
            && credential
                .refresh_token
                .as_deref()
                .is_none_or(|token| token.trim().is_empty())
        {
            return Ok(StoredOAuthStatus::ReauthenticationRequired);
        }
        Ok(StoredOAuthStatus::Ready)
    }

    pub(crate) async fn login_generation(&self, server_id: &str, server_url: &str) -> u64 {
        self.slot(server_id, server_url)
            .await
            .lock()
            .await
            .logout_generation
    }

    pub(crate) async fn install_durable(
        &self,
        credential: StoredOAuthCredential,
        oauth_state: OAuthState,
        login_generation: u64,
    ) -> Result<(), OAuthRouteFailure> {
        let server_id = credential.server_id.clone();
        let server_url = credential.server_url.clone();
        let manager = oauth_state
            .into_authorization_manager()
            .ok_or(OAuthRouteFailure::Unknown)?;
        let slot = self.slot(&server_id, &server_url).await;
        let mut slot = slot.lock().await;
        if slot.logout_generation != login_generation {
            return Err(OAuthRouteFailure::LoginRequired);
        }
        self.repository
            .save(credential.clone())
            .await
            .map_err(|_| OAuthRouteFailure::Store)?;
        slot.logged_out = false;
        slot.runtime = Some(OAuthRuntime {
            manager,
            credential,
            rejected: Arc::new(AtomicBool::new(false)),
            failure: None,
        });
        Ok(())
    }

    pub(crate) async fn discover(
        &self,
        config: &McpStreamableHttpTransportConfig,
    ) -> Result<(), OAuthRouteFailure> {
        let http_client =
            Arc::new(DirectOAuthClient::build().map_err(|_| OAuthRouteFailure::Unknown)?);
        let manager =
            AuthorizationManager::new_with_oauth_http_client(config.url.clone(), http_client)
                .await
                .map_err(map_restore_error)?;
        manager
            .discover_metadata()
            .await
            .map(|_| ())
            .map_err(map_restore_error)
    }

    pub(crate) async fn logout(
        &self,
        server_id: &str,
        server_url: &str,
    ) -> Result<bool, OAuthCredentialStoreError> {
        let slot = self.slot(server_id, server_url).await;
        let mut slot = slot.lock().await;
        slot.logout_generation = slot.logout_generation.wrapping_add(1);
        slot.logged_out = true;
        let in_memory = slot.runtime.take().is_some();
        let persisted = self.repository.remove(server_id, server_url).await?;
        Ok(in_memory || persisted)
    }

    async fn slot(&self, server_id: &str, server_url: &str) -> RuntimeSlot {
        let key = format!("{server_id}\u{0}{server_url}");
        self.slots
            .lock()
            .await
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(OAuthRuntimeSlot::default())))
            .clone()
    }
}

impl OAuthRuntime {
    async fn access_token(
        &mut self,
        repository: &OAuthCredentialRepository,
    ) -> Result<OAuthAccessToken, OAuthRouteFailure> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        let forced_refresh = self.rejected.swap(false, Ordering::AcqRel);
        let needs_refresh = self.credential.expires_at_millis.is_some_and(|expires_at| {
            unix_millis().saturating_add(REFRESH_SKEW_MILLIS) >= expires_at
        });
        if forced_refresh || needs_refresh {
            if self
                .credential
                .refresh_token
                .as_deref()
                .is_none_or(|token| token.trim().is_empty())
            {
                self.failure = Some(OAuthRouteFailure::ReauthenticationRequired);
                return Err(OAuthRouteFailure::ReauthenticationRequired);
            }
            if let Err(error) = self.refresh(repository).await {
                if forced_refresh {
                    self.rejected.store(true, Ordering::Release);
                }
                return Err(error);
            }
        }
        Ok(OAuthAccessToken {
            secret: Arc::from(self.credential.access_token.as_str()),
            rejected: self.rejected.clone(),
        })
    }

    async fn refresh(
        &mut self,
        repository: &OAuthCredentialRepository,
    ) -> Result<(), OAuthRouteFailure> {
        let old_scopes = self.credential.granted_scopes.clone();
        match self.manager.refresh_token().await {
            Ok(_) => {}
            Err(AuthError::AuthorizationRequired) => {
                self.failure = Some(OAuthRouteFailure::ReauthenticationRequired);
                return Err(OAuthRouteFailure::ReauthenticationRequired);
            }
            Err(AuthError::TokenRefreshFailed(message))
                if refresh_error_requires_reauthentication(&message) =>
            {
                self.failure = Some(OAuthRouteFailure::ReauthenticationRequired);
                return Err(OAuthRouteFailure::ReauthenticationRequired);
            }
            Err(_) => return Err(OAuthRouteFailure::Unknown),
        }
        let (client_id, response) = self
            .manager
            .get_credentials()
            .await
            .map_err(map_restore_error)?;
        let response = response.ok_or(OAuthRouteFailure::Unknown)?;
        let credential = StoredOAuthCredential::from_token_response(
            self.credential.server_id.clone(),
            self.credential.server_url.clone(),
            self.credential.configured_client_id.clone(),
            self.credential.resource.clone(),
            client_id,
            &response,
            &old_scopes,
        );
        if repository.save(credential.clone()).await.is_err() {
            return Err(OAuthRouteFailure::Store);
        }
        self.credential = credential;
        Ok(())
    }
}

async fn restore_manager(
    credential: &StoredOAuthCredential,
) -> Result<AuthorizationManager, OAuthRouteFailure> {
    restore_manager_with_response(credential, credential.token_response()).await
}

async fn restore_manager_with_response(
    credential: &StoredOAuthCredential,
    response: OAuthTokenResponse,
) -> Result<AuthorizationManager, OAuthRouteFailure> {
    let http_client = Arc::new(DirectOAuthClient::build().map_err(|_| OAuthRouteFailure::Unknown)?);
    let mut state =
        OAuthState::new_with_oauth_http_client(credential.server_url.clone(), http_client)
            .await
            .map_err(map_restore_error)?;
    state
        .set_credentials(&credential.client_id, response)
        .await
        .map_err(map_restore_error)?;
    state
        .into_authorization_manager()
        .ok_or(OAuthRouteFailure::Unknown)
}

fn refresh_error_requires_reauthentication(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("invalid_grant") || message.contains("invalid grant")
}

fn map_restore_error(error: AuthError) -> OAuthRouteFailure {
    match error {
        AuthError::NoAuthorizationSupport => OAuthRouteFailure::Unsupported,
        AuthError::AuthorizationRequired
        | AuthError::TokenExpired
        | AuthError::TokenRefreshFailed(_) => OAuthRouteFailure::ReauthenticationRequired,
        AuthError::MetadataError(_)
        | AuthError::HttpError(_)
        | AuthError::OAuthError(_)
        | AuthError::UrlError(_)
        | AuthError::InternalError(_)
        | AuthError::RegistrationFailed(_)
        | AuthError::AuthorizationFailed(_)
        | AuthError::InvalidScope(_)
        | AuthError::InsufficientScope { .. }
        | AuthError::TokenExchangeFailed(_)
        | AuthError::InvalidTokenType(_)
        | AuthError::AuthorizationServerMismatch { .. }
        | AuthError::AuthorizationServerMissingIssuer { .. }
        | AuthError::ClientCredentialsError(_) => OAuthRouteFailure::Unknown,
        _ => OAuthRouteFailure::Unknown,
    }
}
