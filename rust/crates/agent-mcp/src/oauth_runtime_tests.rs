use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::future::join_all;
use pretty_assertions::assert_eq;

use super::*;
use crate::oauth_login::tests::{OAuthServer, OAuthServerOptions};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[tokio::test]
async fn refresh_persists_rmcp_credentials_without_extra_discovery() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let temp = TempDir::new();
    let path = temp.path.join("credentials.json");
    let repository =
        OAuthCredentialRepository::open_file(path.clone()).expect("credential repository opens");
    repository
        .save(credential(
            &server.origin,
            Some("refresh-token"),
            Some(unix_millis() + REFRESH_SKEW_MILLIS - 1),
        ))
        .await
        .expect("near-expiry credential saves");
    let runtime = OAuthRuntimeManager::new(repository.clone());
    let config = oauth_config(&server.origin);

    let tokens = join_all((0..8).map(|_| runtime.access_token("server", &config))).await;
    assert!(tokens
        .iter()
        .all(|token| token.as_ref().expect("refresh succeeds").secret() == "rotated-access-token"));
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| {
                request.target == "/token"
                    && String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token")
            })
            .count(),
        1
    );
    let rotated = repository
        .get("server", &format!("{}/mcp?tenant=one", server.origin))
        .await
        .expect("store is available")
        .expect("rotated credential remains stored");
    assert_eq!(
        (
            rotated.access_token,
            rotated.refresh_token,
            rotated.granted_scopes
        ),
        (
            "rotated-access-token".to_string(),
            None,
            vec!["read".to_string(), "search".to_string()],
        )
    );
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.target == "/metadata-final")
            .count(),
        1
    );
    let serialized = fs::read_to_string(path).expect("rotated file reads");
    assert!(serialized.contains("rotated-access-token"));
    assert!(!serialized.contains("\"access_token\":\"access-token\""));

    let restored_runtime = OAuthRuntimeManager::new(repository);
    let restored = restored_runtime
        .access_token("server", &config)
        .await
        .expect("stored rmcp credential restores");
    assert_eq!(restored.secret(), "rotated-access-token");
    restored.mark_rejected();
    assert_eq!(
        restored_runtime.access_token("server", &config).await.err(),
        Some(OAuthRouteFailure::ReauthenticationRequired)
    );
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.target == "/metadata-final")
            .count(),
        2
    );
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| {
                request.target == "/token"
                    && String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token")
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn invalid_grant_refresh_requires_reauthentication_and_preserves_store() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        refresh_invalid_grant: true,
        ..OAuthServerOptions::default()
    })
    .await;
    let temp = TempDir::new();
    let path = temp.path.join("credentials.json");
    let repository =
        OAuthCredentialRepository::open_file(path.clone()).expect("credential repository opens");
    repository
        .save(credential(
            &server.origin,
            Some("refresh-token"),
            Some(unix_millis().saturating_sub(1)),
        ))
        .await
        .expect("expired credential saves");
    let original = fs::read(&path).expect("read original credential file");
    let runtime = OAuthRuntimeManager::new(repository);
    let config = oauth_config(&server.origin);

    for _ in 0..2 {
        assert_eq!(
            runtime.access_token("server", &config).await.err(),
            Some(OAuthRouteFailure::ReauthenticationRequired)
        );
    }
    assert_eq!(fs::read(path).expect("old credential remains"), original);
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| {
                request.target == "/token"
                    && String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token")
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn refresh_persistence_failure_leaves_old_credentials_intact() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        refresh_delay: std::time::Duration::from_millis(100),
        ..OAuthServerOptions::default()
    })
    .await;
    let temp = TempDir::new();
    let parent = temp.path.join("credential-parent");
    fs::create_dir(&parent).expect("create credential parent");
    let path = parent.join("credentials.json");
    let repository =
        OAuthCredentialRepository::open_file(path.clone()).expect("credential repository opens");
    repository
        .save(credential(
            &server.origin,
            Some("refresh-token"),
            Some(unix_millis() + REFRESH_SKEW_MILLIS - 1),
        ))
        .await
        .expect("near-expiry credential saves");
    let original = fs::read(&path).expect("read original credential file");
    let runtime = OAuthRuntimeManager::new(repository);
    let config = oauth_config(&server.origin);
    let acquisition = {
        let runtime = runtime.clone();
        let config = config.clone();
        tokio::spawn(async move { runtime.access_token("server", &config).await })
    };
    server.wait_for_target("/token").await;
    let displaced_parent = temp.path.join("credential-parent-backup");
    fs::rename(&parent, &displaced_parent).expect("move credential parent");
    fs::write(&parent, "blocked").expect("block credential parent path");

    assert_eq!(
        acquisition.await.expect("acquisition task").err(),
        Some(OAuthRouteFailure::Store)
    );
    assert_eq!(
        fs::read(displaced_parent.join("credentials.json"))
            .expect("old file remains readable in displaced parent"),
        original
    );
    fs::remove_file(&parent).expect("remove blocking file");
    fs::rename(&displaced_parent, &parent).expect("restore credential parent");
    assert_eq!(
        runtime
            .access_token("server", &config)
            .await
            .expect("later refresh retries")
            .secret(),
        "rotated-access-token"
    );
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| {
                request.target == "/token"
                    && String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token")
            })
            .count(),
        2
    );

    let serialized = fs::read_to_string(path).expect("replacement persists");
    assert!(serialized.contains("rotated-access-token"));
    assert!(!serialized.contains("\"access_token\":\"access-token\""));
}

#[tokio::test]
async fn transient_refresh_failure_preserves_store_and_later_acquisition_retries() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        refresh_failures: 1,
        ..OAuthServerOptions::default()
    })
    .await;
    let temp = TempDir::new();
    let path = temp.path.join("credentials.json");
    let repository =
        OAuthCredentialRepository::open_file(path.clone()).expect("credential repository opens");
    repository
        .save(credential(
            &server.origin,
            Some("refresh-token"),
            Some(unix_millis().saturating_sub(1)),
        ))
        .await
        .expect("expired credential saves");
    let original = fs::read(&path).expect("read original credential file");
    let runtime = OAuthRuntimeManager::new(repository);
    let config = oauth_config(&server.origin);

    assert_eq!(
        runtime.access_token("server", &config).await.err(),
        Some(OAuthRouteFailure::Unknown)
    );
    assert_eq!(fs::read(&path).expect("old credential remains"), original);
    assert_eq!(
        runtime
            .access_token("server", &config)
            .await
            .expect("second acquisition refreshes")
            .secret(),
        "rotated-access-token"
    );
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| {
                request.target == "/token"
                    && String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token")
            })
            .count(),
        2
    );
}

#[tokio::test]
async fn cancelled_refresh_preserves_old_store_and_can_retry() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        refresh_delay: std::time::Duration::from_millis(200),
        ..OAuthServerOptions::default()
    })
    .await;
    let temp = TempDir::new();
    let path = temp.path.join("credentials.json");
    let repository =
        OAuthCredentialRepository::open_file(path.clone()).expect("credential repository opens");
    repository
        .save(credential(
            &server.origin,
            Some("refresh-token"),
            Some(unix_millis().saturating_sub(1)),
        ))
        .await
        .expect("expired credential saves");
    let original = fs::read(&path).expect("read original credential file");
    let runtime = OAuthRuntimeManager::new(repository);
    let config = oauth_config(&server.origin);
    let acquisition = {
        let runtime = runtime.clone();
        let config = config.clone();
        tokio::spawn(async move { runtime.access_token("server", &config).await })
    };
    server.wait_for_target("/token").await;
    acquisition.abort();
    let _ = acquisition.await;

    assert_eq!(fs::read(&path).expect("old credential remains"), original);
    assert_eq!(
        runtime
            .access_token("server", &config)
            .await
            .expect("later acquisition refreshes")
            .secret(),
        "rotated-access-token"
    );
}

#[tokio::test]
async fn unknown_expiry_is_usable_and_expired_without_refresh_requires_reauthentication() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let repository = OAuthCredentialRepository::memory();
    let config = oauth_config(&server.origin);
    repository
        .save(credential(&server.origin, None, None))
        .await
        .expect("unknown-expiry credential saves");
    let runtime = OAuthRuntimeManager::new(repository.clone());
    let token = runtime
        .access_token("server", &config)
        .await
        .expect("unknown expiry stays usable");
    assert_eq!(token.secret(), "access-token");
    assert!(!server
        .requests()
        .iter()
        .any(|request| request.target == "/token"));
    token.mark_rejected();
    for _ in 0..2 {
        assert_eq!(
            runtime.access_token("server", &config).await.err(),
            Some(OAuthRouteFailure::ReauthenticationRequired)
        );
    }

    repository
        .save(credential(
            &server.origin,
            None,
            Some(unix_millis().saturating_sub(1)),
        ))
        .await
        .expect("expired credential saves");
    let runtime = OAuthRuntimeManager::new(repository);
    assert_eq!(
        runtime.access_token("server", &config).await.err(),
        Some(OAuthRouteFailure::ReauthenticationRequired)
    );
}

fn credential(
    origin: &str,
    refresh_token: Option<&str>,
    expires_at_millis: Option<u64>,
) -> StoredOAuthCredential {
    StoredOAuthCredential {
        server_id: "server".to_string(),
        server_url: format!("{origin}/mcp?tenant=one"),
        configured_client_id: Some("static-client".to_string()),
        resource: None,
        client_id: "static-client".to_string(),
        access_token: "access-token".to_string(),
        refresh_token: refresh_token.map(ToString::to_string),
        expires_at_millis,
        granted_scopes: vec!["read".to_string(), "search".to_string()],
    }
}

fn oauth_config(origin: &str) -> McpStreamableHttpTransportConfig {
    serde_json::from_value(serde_json::json!({
        "url": format!("{origin}/mcp?tenant=one"),
        "auth": {
            "type": "oauth",
            "client_id": "static-client",
        }
    }))
    .expect("OAuth runtime config")
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pi-relay-oauth-runtime-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create runtime temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
