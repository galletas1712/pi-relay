use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use oauth2::basic::BasicTokenType;
use oauth2::{AccessToken, RefreshToken, Scope, TokenResponse};
use rmcp::transport::auth::{OAuthTokenResponse, VendorExtraTokenFields};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

const FILE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_IDENTITY_BYTES: usize = 16 * 1024;
const MAX_TOKEN_BYTES: usize = 256 * 1024;
const MAX_SCOPES: usize = 256;
const MAX_SCOPE_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OAuthCredentialStoreError {
    #[error("oauth_credential_store_empty")]
    Empty,
    #[error("oauth_credential_store_oversized")]
    Oversized,
    #[error("oauth_credential_store_corrupt")]
    Corrupt,
    #[error("oauth_credential_store_version_unsupported")]
    UnsupportedVersion,
    #[error("oauth_credential_store_bounds_exceeded")]
    Bounds,
    #[error("oauth_credential_store_io_failed")]
    Io,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredOAuthCredential {
    pub(crate) server_id: String,
    pub(crate) server_url: String,
    #[serde(default)]
    pub(crate) configured_client_id: Option<String>,
    #[serde(default)]
    pub(crate) resource: Option<String>,
    pub(crate) client_id: String,
    pub(crate) access_token: String,
    #[serde(default)]
    pub(crate) refresh_token: Option<String>,
    #[serde(default)]
    pub(crate) expires_at_millis: Option<u64>,
    #[serde(default)]
    pub(crate) granted_scopes: Vec<String>,
}

impl std::fmt::Debug for StoredOAuthCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredOAuthCredential")
            .field("server_id", &self.server_id)
            .field("server_url", &"<redacted>")
            .field(
                "configured_client_id",
                &self.configured_client_id.as_ref().map(|_| "<redacted>"),
            )
            .field("resource", &self.resource.as_ref().map(|_| "<redacted>"))
            .field("client_id", &"<redacted>")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at_millis", &self.expires_at_millis)
            .field("granted_scope_count", &self.granted_scopes.len())
            .finish()
    }
}

impl StoredOAuthCredential {
    pub(crate) fn from_token_response(
        server_id: String,
        server_url: String,
        configured_client_id: Option<String>,
        resource: Option<String>,
        client_id: String,
        response: &OAuthTokenResponse,
        previous_scopes: &[String],
    ) -> Self {
        let granted_scopes = response
            .scopes()
            .map(|scopes| {
                scopes
                    .iter()
                    .map(|scope| scope.as_ref().to_string())
                    .collect()
            })
            .unwrap_or_else(|| previous_scopes.to_vec());
        Self {
            server_id,
            server_url,
            configured_client_id,
            resource,
            client_id,
            access_token: response.access_token().secret().to_string(),
            refresh_token: response
                .refresh_token()
                .map(|token| token.secret().to_string()),
            expires_at_millis: compute_expires_at_millis(response),
            granted_scopes,
        }
    }

    pub(crate) fn token_response(&self) -> OAuthTokenResponse {
        let mut response = OAuthTokenResponse::new(
            AccessToken::new(self.access_token.clone()),
            BasicTokenType::Bearer,
            VendorExtraTokenFields::default(),
        );
        if let Some(refresh_token) = &self.refresh_token {
            response.set_refresh_token(Some(RefreshToken::new(refresh_token.clone())));
        }
        if !self.granted_scopes.is_empty() {
            response.set_scopes(Some(
                self.granted_scopes
                    .iter()
                    .cloned()
                    .map(Scope::new)
                    .collect(),
            ));
        }
        if let Some(expires_at_millis) = self.expires_at_millis {
            let now = unix_millis();
            let remaining = Duration::from_millis(expires_at_millis.saturating_sub(now));
            response.set_expires_in(Some(&remaining));
        }
        response
    }

    pub(crate) fn is_compatible(
        &self,
        server_id: &str,
        server_url: &str,
        configured_client_id: Option<&str>,
        configured_scopes: Option<&[String]>,
        resource: Option<&str>,
    ) -> bool {
        self.server_id == server_id
            && self.server_url == server_url
            && self.configured_client_id.as_deref() == configured_client_id
            && self.resource.as_deref() == resource
            && configured_client_id.is_none_or(|client_id| client_id == self.client_id)
            && configured_scopes.is_none_or(|scopes| {
                scopes
                    .iter()
                    .all(|scope| self.granted_scopes.contains(scope))
            })
    }

    fn validate(&self) -> Result<(), OAuthCredentialStoreError> {
        let identities = [
            self.server_id.as_str(),
            self.server_url.as_str(),
            self.client_id.as_str(),
        ];
        if identities
            .iter()
            .any(|value| value.trim().is_empty() || value.len() > MAX_IDENTITY_BYTES)
            || self
                .configured_client_id
                .as_ref()
                .is_some_and(|value| value.trim().is_empty() || value.len() > MAX_IDENTITY_BYTES)
            || self
                .resource
                .as_ref()
                .is_some_and(|value| value.len() > MAX_IDENTITY_BYTES)
            || self.access_token.trim().is_empty()
            || self.access_token.len() > MAX_TOKEN_BYTES
            || self
                .refresh_token
                .as_ref()
                .is_some_and(|value| value.trim().is_empty() || value.len() > MAX_TOKEN_BYTES)
            || self.granted_scopes.len() > MAX_SCOPES
            || self
                .granted_scopes
                .iter()
                .any(|scope| scope.trim().is_empty() || scope.len() > MAX_SCOPE_BYTES)
        {
            return Err(OAuthCredentialStoreError::Bounds);
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialFile {
    version: u32,
    credentials: BTreeMap<String, StoredOAuthCredential>,
}

impl Default for CredentialFile {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            credentials: BTreeMap::new(),
        }
    }
}

enum Backend {
    Memory,
    File(PathBuf),
    Unavailable(OAuthCredentialStoreError),
}

pub(crate) struct OAuthCredentialRepository {
    backend: Backend,
    contents: Mutex<CredentialFile>,
}

impl OAuthCredentialRepository {
    pub(crate) fn memory() -> Arc<Self> {
        Arc::new(Self {
            backend: Backend::Memory,
            contents: Mutex::new(CredentialFile::default()),
        })
    }

    pub(crate) fn open_file(path: PathBuf) -> Result<Arc<Self>, OAuthCredentialStoreError> {
        let contents = read_file(&path)?.unwrap_or_default();
        Ok(Arc::new(Self {
            backend: Backend::File(path),
            contents: Mutex::new(contents),
        }))
    }

    pub(crate) fn unavailable(error: OAuthCredentialStoreError) -> Arc<Self> {
        Arc::new(Self {
            backend: Backend::Unavailable(error),
            contents: Mutex::new(CredentialFile::default()),
        })
    }

    pub(crate) fn availability(&self) -> Result<(), OAuthCredentialStoreError> {
        match &self.backend {
            Backend::Memory | Backend::File(_) => Ok(()),
            Backend::Unavailable(error) => Err(error.clone()),
        }
    }

    pub(crate) async fn get(
        &self,
        server_id: &str,
        server_url: &str,
    ) -> Result<Option<StoredOAuthCredential>, OAuthCredentialStoreError> {
        self.availability()?;
        Ok(self
            .contents
            .lock()
            .await
            .credentials
            .get(&credential_key(server_id, server_url))
            .cloned())
    }

    pub(crate) async fn save(
        &self,
        credential: StoredOAuthCredential,
    ) -> Result<(), OAuthCredentialStoreError> {
        self.availability()?;
        credential.validate()?;
        let mut contents = self.contents.lock().await;
        let mut replacement = contents.clone();
        replacement.credentials.insert(
            credential_key(&credential.server_id, &credential.server_url),
            credential,
        );
        replacement.validate()?;
        if let Backend::File(path) = &self.backend {
            write_file(path, &replacement)?;
        }
        *contents = replacement;
        Ok(())
    }

    pub(crate) async fn remove(
        &self,
        server_id: &str,
        server_url: &str,
    ) -> Result<bool, OAuthCredentialStoreError> {
        self.availability()?;
        let mut contents = self.contents.lock().await;
        let mut replacement = contents.clone();
        let removed = replacement
            .credentials
            .remove(&credential_key(server_id, server_url))
            .is_some();
        if removed {
            if let Backend::File(path) = &self.backend {
                write_file(path, &replacement)?;
            }
            *contents = replacement;
        }
        Ok(removed)
    }
}

impl CredentialFile {
    fn validate(&self) -> Result<(), OAuthCredentialStoreError> {
        if self.version != FILE_VERSION {
            return Err(OAuthCredentialStoreError::UnsupportedVersion);
        }
        for (key, credential) in &self.credentials {
            credential.validate()?;
            if key != &credential_key(&credential.server_id, &credential.server_url) {
                return Err(OAuthCredentialStoreError::Corrupt);
            }
        }
        Ok(())
    }
}

fn read_file(path: &Path) -> Result<Option<CredentialFile>, OAuthCredentialStoreError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(OAuthCredentialStoreError::Io),
    };
    if metadata.len() > MAX_FILE_BYTES {
        return Err(OAuthCredentialStoreError::Oversized);
    }
    let bytes = fs::read(path).map_err(|_| OAuthCredentialStoreError::Io)?;
    if bytes.is_empty() || bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(OAuthCredentialStoreError::Empty);
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(OAuthCredentialStoreError::Oversized);
    }
    let contents: CredentialFile =
        serde_json::from_slice(&bytes).map_err(|_| OAuthCredentialStoreError::Corrupt)?;
    contents.validate()?;
    Ok(Some(contents))
}

fn write_file(path: &Path, contents: &CredentialFile) -> Result<(), OAuthCredentialStoreError> {
    let parent = path.parent().ok_or(OAuthCredentialStoreError::Io)?;
    fs::create_dir_all(parent).map_err(|_| OAuthCredentialStoreError::Io)?;
    set_directory_permissions(parent)?;
    let serialized =
        serde_json::to_vec(contents).map_err(|_| OAuthCredentialStoreError::Corrupt)?;
    if serialized.len() as u64 > MAX_FILE_BYTES {
        return Err(OAuthCredentialStoreError::Oversized);
    }
    let mut temp = tempfile::Builder::new()
        .prefix(".mcp-oauth-")
        .tempfile_in(parent)
        .map_err(|_| OAuthCredentialStoreError::Io)?;
    set_file_permissions(temp.path())?;
    temp.write_all(&serialized)
        .map_err(|_| OAuthCredentialStoreError::Io)?;
    temp.as_file()
        .sync_all()
        .map_err(|_| OAuthCredentialStoreError::Io)?;
    temp.persist(path)
        .map_err(|_| OAuthCredentialStoreError::Io)?;
    Ok(())
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<(), OAuthCredentialStoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| OAuthCredentialStoreError::Io)
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<(), OAuthCredentialStoreError> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<(), OAuthCredentialStoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| OAuthCredentialStoreError::Io)
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<(), OAuthCredentialStoreError> {
    Ok(())
}

fn credential_key(server_id: &str, server_url: &str) -> String {
    let payload = serde_json::json!({
        "type": "http",
        "url": server_url,
        "headers": {},
    });
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(&payload).expect("OAuth credential key serializes"));
    let digest = format!("{:x}", hasher.finalize());
    format!("{server_id}|{}", &digest[..16])
}

pub(crate) fn compute_expires_at_millis(response: &OAuthTokenResponse) -> Option<u64> {
    let expires_in = response.expires_in()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let expires_at = now.checked_add(expires_in)?;
    u64::try_from(expires_at.as_millis()).ok()
}

pub(crate) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "oauth_credentials_tests.rs"]
mod tests;
