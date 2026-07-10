use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Barrier;
use tokio::task::JoinHandle;

use super::*;

pub(crate) static CALLBACK_PORT_TEST_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

#[test]
fn callback_id_is_bound_to_canonical_server_url_without_fragment() {
    let callback_id = callback_id_from_server_url("https://mcp.example.com/mcp?tenant=one")
        .expect("server URL should parse");
    let same_without_fragment =
        callback_id_from_server_url("https://mcp.example.com/mcp?tenant=one#unused")
            .expect("server URL should parse");
    let different_path = callback_id_from_server_url("https://mcp.example.com/sse?tenant=one")
        .expect("server URL should parse");
    let different_query = callback_id_from_server_url("https://mcp.example.com/mcp?tenant=two")
        .expect("server URL should parse");
    let different_origin = callback_id_from_server_url("https://mcp.example.com:8443/mcp")
        .expect("server URL should parse");

    assert_eq!(callback_id, same_without_fragment);
    assert_ne!(callback_id, different_path);
    assert_ne!(callback_id, different_query);
    assert_ne!(callback_id, different_origin);
    assert_eq!(callback_id.len(), 12);
    assert!(callback_id
        .chars()
        .all(|character| character.is_ascii_alphanumeric()
            || character == '-'
            || character == '_'));
}

#[test]
fn callback_redirect_path_matches_codex_shape() {
    let callback_id = callback_id_from_server_url("https://mcp.example.com/mcp?tenant=one")
        .expect("server URL should parse");

    assert_eq!(format!("/callback/{callback_id}").len(), 22);
    assert_eq!(format!("/callback/{callback_id}"), "/callback/XuuuHAzzHOni");
}

#[tokio::test]
async fn rmcp_dynamic_login_follows_discovery_and_registration_redirects() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let (coordinator, config, start) = begin_with_fixed_callback_port(
        &server.origin,
        OAuthConfig {
            scopes: Some(&["read", "search"]),
            resource: Some("https://api.example.test/a?tenant=one"),
            ..OAuthConfig::default()
        },
    )
    .await;
    assert_eq!(
        coordinator
            .begin("server", &config, Instant::now() + Duration::from_secs(5))
            .await,
        Err(McpOAuthLoginError::AlreadyPending)
    );
    assert!(!format!("{start:?}").contains("state="));

    let authorization = query_values(&start.authorization_url);
    assert_eq!(authorization["audience"], vec!["existing"]);
    assert_eq!(
        authorization["resource"],
        vec![
            format!("{}/mcp?tenant=one", server.origin),
            "https://api.example.test/a?tenant=one".to_string(),
        ]
    );
    assert_eq!(authorization["scope"], vec!["read search"]);
    assert_eq!(authorization["code_challenge_method"], vec!["S256"]);
    assert!(!authorization["code_challenge"][0].is_empty());
    assert!(!authorization["state"][0].is_empty());
    let redirect_uri = authorization["redirect_uri"][0].clone();
    assert_eq!(
        reqwest::Url::parse(&redirect_uri)
            .expect("redirect URI")
            .path(),
        format!(
            "/callback/{}",
            callback_id_from_server_url(&format!("{}/mcp?tenant=one", server.origin))
                .expect("server URL")
        )
    );
    let state = authorization["state"][0].clone();

    coordinator
        .complete(
            "server",
            &start.login_id,
            &format!("{redirect_uri}?code=authorization-code&state={state}"),
        )
        .await
        .expect("manual callback completes");
    assert_finalized(&coordinator, "server", CredentialExpectation::Present);

    let requests = server.requests();
    for target in [
        "/mcp?tenant=one",
        "/mcp-final?tenant=one",
        "/.well-known/oauth-authorization-server",
        "/metadata-final",
        "/register-redirect",
        "/register",
        "/token",
    ] {
        assert!(
            requests.iter().any(|request| request.target == target),
            "{target}"
        );
    }
    let registration = body_json(request(&requests, "/register"));
    assert_eq!(registration["response_types"], serde_json::json!(["code"]));
    assert_eq!(
        registration["grant_types"],
        serde_json::json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(
        registration["token_endpoint_auth_method"],
        serde_json::json!("none")
    );
    assert_eq!(registration["scope"], serde_json::json!("read search"));
    let token = body_form(request(&requests, "/token"));
    assert_eq!(token["code"], "authorization-code");
    assert_eq!(token["redirect_uri"], redirect_uri);
    assert_eq!(
        token["resource"],
        format!("{}/mcp?tenant=one", server.origin)
    );
    assert!(!token["code_verifier"].is_empty());
}

#[tokio::test]
async fn durable_commit_keeps_pending_reservation_until_success() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let reached = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    coordinator.set_persistence_barriers((reached.clone(), release.clone()));
    let config = oauth_config(&server.origin, OAuthConfig::default());
    let start = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("login starts");
    let (redirect_uri, state) = login_values(&start.authorization_url);
    let completion = {
        let coordinator = coordinator.clone();
        let login_id = start.login_id.clone();
        tokio::spawn(async move {
            coordinator
                .complete(
                    "server",
                    &login_id,
                    &format!("{redirect_uri}?code=authorization-code&state={state}"),
                )
                .await
        })
    };

    reached.wait().await;
    assert!(coordinator.is_pending("server"));
    assert_eq!(
        coordinator.cancel("server", &start.login_id).await,
        Err(McpOAuthLoginError::AlreadyCompleted)
    );
    assert_eq!(
        coordinator
            .begin("server", &config, Instant::now() + Duration::from_secs(5))
            .await,
        Err(McpOAuthLoginError::AlreadyPending)
    );
    assert!(!completion.is_finished());

    release.wait().await;
    assert_eq!(completion.await.expect("completion task"), Ok(()));
    assert_finalized(&coordinator, "server", CredentialExpectation::Present);
}

#[tokio::test]
async fn oversized_authorization_url_cleans_up_flow_and_port_for_restart() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve callback port");
    let port = listener.local_addr().expect("callback address").port();
    drop(listener);
    let coordinator = OAuthCoordinator::new();
    let config = oauth_config(
        &server.origin,
        OAuthConfig {
            resource: Some(&"r".repeat(MAX_AUTHORIZATION_URL_BYTES)),
            callback_port: Some(port),
            ..OAuthConfig::default()
        },
    );

    assert_eq!(
        coordinator
            .begin("server", &config, Instant::now() + Duration::from_secs(5))
            .await,
        Err(McpOAuthLoginError::AuthorizationUrlTooLong)
    );
    assert_finalized(&coordinator, "server", CredentialExpectation::Absent);
    drop(
        TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("oversize cleanup releases callback port"),
    );

    let restart = coordinator
        .begin(
            "server",
            &oauth_config(
                &server.origin,
                OAuthConfig {
                    callback_port: Some(port),
                    ..OAuthConfig::default()
                },
            ),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("oversize cleanup permits immediate restart");
    coordinator
        .cancel("server", &restart.login_id)
        .await
        .expect("restart cancels");
}

fn callback_port(authorization_url: &str) -> u16 {
    let (redirect_uri, _) = login_values(authorization_url);
    reqwest::Url::parse(&redirect_uri)
        .expect("redirect URI")
        .port()
        .expect("redirect port")
}

#[tokio::test]
async fn cancel_during_discovery_releases_reservation_for_immediate_restart() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        metadata_delay: Duration::from_secs(1),
        ..OAuthServerOptions::default()
    })
    .await;
    let callback_port_guard = CALLBACK_PORT_TEST_LOCK.lock().await;
    let repository = OAuthCredentialRepository::memory();
    let coordinator = OAuthCoordinator::with_runtime(OAuthRuntimeManager::new(repository.clone()));
    let config = oauth_config(&server.origin, OAuthConfig::default());
    let begin = {
        let coordinator = coordinator.clone();
        let config = config.clone();
        tokio::spawn(async move {
            coordinator
                .begin("server", &config, Instant::now() + Duration::from_secs(5))
                .await
        })
    };
    server.wait_for_target("/metadata-final").await;
    let login_id = coordinator
        .state
        .lock()
        .expect("state lock")
        .active_by_server
        .get("server")
        .cloned()
        .expect("reservation is active during discovery");
    coordinator
        .cancel("server", &login_id)
        .await
        .expect("pre-flow cancellation is acknowledged");
    assert_eq!(
        begin.await.expect("begin task"),
        Err(McpOAuthLoginError::Cancelled)
    );
    assert_finalized(&coordinator, "server", CredentialExpectation::Absent);
    assert_eq!(
        repository
            .get("server", &format!("{}/mcp?tenant=one", server.origin))
            .await
            .expect("store is available"),
        None
    );

    drop(callback_port_guard);
    let second_server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let second_config = oauth_config(&second_server.origin, OAuthConfig::default());
    let second = coordinator
        .begin(
            "server",
            &second_config,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("cancellation permits immediate restart");
    coordinator
        .cancel("server", &second.login_id)
        .await
        .expect("second login cancels");
}

#[tokio::test]
async fn blank_configured_client_id_uses_dcr_and_persists_normalized_identity() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let repository = OAuthCredentialRepository::memory();
    let coordinator = OAuthCoordinator::with_runtime(OAuthRuntimeManager::new(repository.clone()));
    let config = oauth_config(
        &server.origin,
        OAuthConfig {
            client_id: Some(" \t "),
            ..OAuthConfig::default()
        },
    );
    let start = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("blank client id starts DCR login");
    complete(&coordinator, &start)
        .await
        .expect("DCR login completes");

    let stored = repository
        .get("server", &format!("{}/mcp?tenant=one", server.origin))
        .await
        .expect("store is available")
        .expect("DCR credential persists");
    assert_eq!(
        (stored.configured_client_id, stored.client_id),
        (None, "dynamic-client".to_string())
    );
    assert!(server
        .requests()
        .iter()
        .any(|request| request.target == "/register"));
}

#[tokio::test]
async fn static_client_authorization_accepts_only_codex_callback_and_login_succeeds() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let config = oauth_config(
        &server.origin,
        OAuthConfig {
            client_id: Some("static-client"),
            ..OAuthConfig::default()
        },
    );
    let start = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("login starts");
    let (redirect_uri, _) = login_values(&start.authorization_url);
    let redirect_uri = reqwest::Url::parse(&redirect_uri).expect("redirect URI");
    let expected_callback_id =
        callback_id_from_server_url(&format!("{}/mcp?tenant=one", server.origin))
            .expect("fixture server URL");
    let expected_redirect_uri = format!(
        "http://127.0.0.1:{}/callback/{expected_callback_id}",
        redirect_uri.port().expect("callback port")
    );
    assert_eq!(redirect_uri.as_str(), expected_redirect_uri);
    server.allow_redirect_uri(expected_redirect_uri);

    let mut rejected_url =
        reqwest::Url::parse(&start.authorization_url).expect("authorization URL");
    let rejected_query = rejected_url
        .query_pairs()
        .map(|(name, value)| {
            let value = if name == "redirect_uri" {
                "http://127.0.0.1:43210/oauth/callback/per-login-id".into()
            } else {
                value
            };
            (name.into_owned(), value.into_owned())
        })
        .collect::<Vec<_>>();
    rejected_url.set_query(None);
    rejected_url.query_pairs_mut().extend_pairs(
        rejected_query
            .iter()
            .map(|(name, value)| (&**name, &**value)),
    );
    let no_redirects = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build HTTP client");
    let rejected = no_redirects
        .get(rejected_url)
        .send()
        .await
        .expect("authorization server rejects incompatible redirect");
    assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);

    let response = reqwest::get(&start.authorization_url)
        .await
        .expect("authorization follows exact callback");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    wait_for_finalized(&coordinator).await;
    assert_finalized(&coordinator, "server", CredentialExpectation::Present);
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.target.starts_with("/authorize?"))
            .count(),
        2
    );
}

#[tokio::test]
async fn static_client_skips_dcr_and_uses_discovered_scopes() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let repository = OAuthCredentialRepository::memory();
    let coordinator = OAuthCoordinator::with_runtime(OAuthRuntimeManager::new(repository.clone()));
    let config = oauth_config(
        &server.origin,
        OAuthConfig {
            client_id: Some("static-client"),
            ..OAuthConfig::default()
        },
    );
    let start = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("login starts");
    let authorization = query_values(&start.authorization_url);
    assert_eq!(authorization["client_id"], vec!["static-client"]);
    assert_eq!(authorization["scope"], vec!["discovered"]);
    complete(&coordinator, &start)
        .await
        .expect("login completes");
    assert_finalized(&coordinator, "server", CredentialExpectation::Present);
    let stored = repository
        .get("server", &format!("{}/mcp?tenant=one", server.origin))
        .await
        .expect("store is available")
        .expect("static-client credential persists");
    assert_eq!(
        (
            stored.client_id,
            stored.access_token,
            stored.refresh_token,
            stored.granted_scopes,
        ),
        (
            "static-client".to_string(),
            "access-token".to_string(),
            Some("refresh-token".to_string()),
            vec!["read".to_string(), "search".to_string()],
        )
    );
    assert!(!server
        .requests()
        .iter()
        .any(|request| request.target.starts_with("/register")));
}

#[tokio::test]
async fn token_redirect_is_not_followed() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        token_redirect: true,
        ..OAuthServerOptions::default()
    })
    .await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let start = coordinator
        .begin(
            "server",
            &oauth_config(&server.origin, OAuthConfig::default()),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("login starts");
    assert_eq!(
        complete(&coordinator, &start).await,
        Err(McpOAuthLoginError::TokenEndpoint)
    );
    assert_finalized(&coordinator, "server", CredentialExpectation::Absent);
    let requests = server.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target == "/token-redirect")
            .count(),
        1
    );
    assert!(!requests.iter().any(|request| request.target == "/token"));
}

#[tokio::test]
async fn wrong_state_is_rejected_by_rmcp() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let start = coordinator
        .begin(
            "server",
            &oauth_config(&server.origin, OAuthConfig::default()),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("login starts");
    let (redirect_uri, _) = login_values(&start.authorization_url);
    assert_eq!(
        coordinator
            .complete(
                "server",
                &start.login_id,
                &format!("{redirect_uri}?code=authorization-code&state=wrong"),
            )
            .await,
        Err(McpOAuthLoginError::InvalidCallback)
    );
    assert_finalized(&coordinator, "server", CredentialExpectation::Absent);
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.target == "/token")
            .count(),
        0
    );
}

#[tokio::test]
async fn manual_callback_requires_stable_redirect_path() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let start = coordinator
        .begin(
            "server",
            &oauth_config(&server.origin, OAuthConfig::default()),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("login starts");
    let (redirect_uri, state) = login_values(&start.authorization_url);
    let mut callback_url = reqwest::Url::parse(&redirect_uri).expect("redirect URI");
    callback_url.set_path(&format!("/oauth/callback/{}", start.login_id));
    callback_url
        .query_pairs_mut()
        .extend_pairs([("code", "authorization-code"), ("state", state.as_str())]);

    assert_eq!(
        coordinator
            .complete("server", &start.login_id, callback_url.as_str())
            .await,
        Err(McpOAuthLoginError::InvalidCallback)
    );
    assert_finalized(&coordinator, "server", CredentialExpectation::Absent);
    assert!(!server
        .requests()
        .iter()
        .any(|request| request.target == "/token"));
}

#[tokio::test]
async fn callback_timeout_cleans_up_flow_and_port() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let config = oauth_config(
        &server.origin,
        OAuthConfig {
            callback_timeout_ms: 20,
            ..OAuthConfig::default()
        },
    );
    let start = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("login starts");
    let port = callback_port(&start.authorization_url);
    wait_for_finalized(&coordinator).await;
    assert_finalized(&coordinator, "server", CredentialExpectation::Absent);
    TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("expiry releases callback port");
}

#[tokio::test]
async fn dropped_begin_and_completion_waiters_do_not_cancel_owner() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        metadata_delay: Duration::from_millis(100),
        token_delay: Duration::from_millis(100),
        ..OAuthServerOptions::default()
    })
    .await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let begin = {
        let coordinator = coordinator.clone();
        let config = oauth_config(&server.origin, OAuthConfig::default());
        tokio::spawn(async move {
            coordinator
                .begin("server", &config, Instant::now() + Duration::from_secs(5))
                .await
        })
    };
    server.wait_for_target("/metadata-final").await;
    begin.abort();
    let login_id = wait_for_flow(&coordinator).await;
    let authorization_url = coordinator
        .state
        .lock()
        .expect("state lock")
        .active_by_server
        .get("server")
        .cloned();
    assert_eq!(authorization_url.as_deref(), Some(login_id.as_str()));
    coordinator
        .cancel("server", &login_id)
        .await
        .expect("owner survives dropped begin waiter");
    assert_finalized(&coordinator, "server", CredentialExpectation::Absent);

    let start = coordinator
        .begin(
            "server",
            &oauth_config(&server.origin, OAuthConfig::default()),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("second login starts");
    let (redirect_uri, state) = login_values(&start.authorization_url);
    let completion = {
        let coordinator = coordinator.clone();
        let login_id = start.login_id.clone();
        tokio::spawn(async move {
            coordinator
                .complete(
                    "server",
                    &login_id,
                    &format!("{redirect_uri}?code=authorization-code&state={state}"),
                )
                .await
        })
    };
    server.wait_for_target_count("/token", 1).await;
    completion.abort();
    wait_for_finalized(&coordinator).await;
    assert_finalized(&coordinator, "server", CredentialExpectation::Present);
}

#[tokio::test]
async fn listener_and_manual_callbacks_converge_on_one_exchange() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        token_delay: Duration::from_millis(50),
        ..OAuthServerOptions::default()
    })
    .await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let start = coordinator
        .begin(
            "server",
            &oauth_config(&server.origin, OAuthConfig::default()),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("login starts");
    let (redirect_uri, state) = login_values(&start.authorization_url);
    let callback_url = format!("{redirect_uri}?code=authorization-code&state={state}");
    let manual = {
        let coordinator = coordinator.clone();
        let login_id = start.login_id.clone();
        let callback_url = callback_url.clone();
        tokio::spawn(async move {
            coordinator
                .complete("server", &login_id, &callback_url)
                .await
        })
    };
    let listener = tokio::spawn(send_callback(callback_url));
    let _ = manual.await.expect("manual task");
    let _ = listener.await.expect("listener task");
    wait_for_finalized(&coordinator).await;
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.target == "/token")
            .count(),
        1
    );
}

#[tokio::test]
async fn listener_success_waits_for_cleanup_and_allows_immediate_port_reuse() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let finalization_reached = Arc::new(Barrier::new(2));
    let release_finalization = Arc::new(Barrier::new(2));
    *coordinator
        .acknowledgement_barriers
        .lock()
        .expect("barrier lock") =
        Some((finalization_reached.clone(), release_finalization.clone()));
    let config = oauth_config(&server.origin, OAuthConfig::default());
    let first = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("first login starts");
    let (redirect_uri, state) = login_values(&first.authorization_url);
    let port = callback_port(&first.authorization_url);
    let callback = tokio::spawn(send_callback(format!(
        "{redirect_uri}?code=authorization-code&state={state}"
    )));

    finalization_reached.wait().await;
    assert!(!callback.is_finished());
    assert_finalized(&coordinator, "server", CredentialExpectation::Present);
    drop(
        TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("callback port is released before success response"),
    );

    release_finalization.wait().await;
    let response = callback.await.expect("callback task");
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let second = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("success response permits immediate restart");
    coordinator
        .cancel("server", &second.login_id)
        .await
        .expect("second login cancels");
}

#[tokio::test]
async fn cancel_acknowledgement_allows_immediate_restart_and_port_reuse() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let config = oauth_config(&server.origin, OAuthConfig::default());
    let first = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("first login starts");
    let first_redirect_uri = login_values(&first.authorization_url).0;
    let first_port = callback_port(&first.authorization_url);
    coordinator
        .cancel("server", &first.login_id)
        .await
        .expect("cancel is acknowledged");
    assert_finalized(&coordinator, "server", CredentialExpectation::Absent);
    drop(
        TcpListener::bind(("127.0.0.1", first_port))
            .await
            .expect("cancel acknowledgement releases first callback port"),
    );

    let second = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("cancel acknowledgement permits immediate restart");
    let second_redirect_uri = login_values(&second.authorization_url).0;
    let second_port = callback_port(&second.authorization_url);
    assert_ne!(first.login_id, second.login_id);
    assert_eq!(
        reqwest::Url::parse(&first_redirect_uri)
            .expect("first redirect URI")
            .path(),
        reqwest::Url::parse(&second_redirect_uri)
            .expect("second redirect URI")
            .path()
    );
    coordinator
        .cancel("server", &second.login_id)
        .await
        .expect("second cancel is acknowledged");
    TcpListener::bind(("127.0.0.1", second_port))
        .await
        .expect("second cancel acknowledgement releases callback port");
}

#[tokio::test]
async fn cancel_distinguishes_completed_login_from_wrong_or_replaced_login() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let config = oauth_config(&server.origin, OAuthConfig::default());
    let completed = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("login starts");
    complete(&coordinator, &completed)
        .await
        .expect("login completes");

    assert_eq!(
        coordinator.cancel("server", &completed.login_id).await,
        Err(McpOAuthLoginError::AlreadyCompleted)
    );
    assert_eq!(
        coordinator.cancel("unknown", &completed.login_id).await,
        Err(McpOAuthLoginError::NotFound)
    );

    let active = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("new login starts");
    assert_eq!(
        coordinator.cancel("server", &completed.login_id).await,
        Err(McpOAuthLoginError::NotFound)
    );
    coordinator
        .cancel("server", &active.login_id)
        .await
        .expect("active login cancels");
}

#[tokio::test]
async fn cancel_with_arbitrary_login_id_after_completion_is_not_found() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let config = oauth_config(&server.origin, OAuthConfig::default());
    let completed = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("login starts");
    complete(&coordinator, &completed)
        .await
        .expect("login completes");

    assert_eq!(
        coordinator.cancel("server", "fabricated-login-id").await,
        Err(McpOAuthLoginError::NotFound)
    );
}

#[tokio::test]
async fn cancelled_replacement_is_not_completed_by_older_credential() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let config = oauth_config(&server.origin, OAuthConfig::default());
    let completed = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("first login starts");
    complete(&coordinator, &completed)
        .await
        .expect("first login completes");
    let cancelled = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("replacement login starts");
    coordinator
        .cancel("server", &cancelled.login_id)
        .await
        .expect("replacement login cancels");
    assert_finalized(&coordinator, "server", CredentialExpectation::Present);

    assert_eq!(
        coordinator.cancel("server", &cancelled.login_id).await,
        Err(McpOAuthLoginError::NotFound)
    );
    assert_eq!(
        coordinator.cancel("server", &completed.login_id).await,
        Err(McpOAuthLoginError::AlreadyCompleted)
    );
}

#[tokio::test]
async fn cancel_after_token_exchange_is_linearized_with_credential_commit() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let finalization_reached = Arc::new(Barrier::new(2));
    let release_finalization = Arc::new(Barrier::new(2));
    *coordinator
        .finalization_barriers
        .lock()
        .expect("barrier lock") =
        Some((finalization_reached.clone(), release_finalization.clone()));
    let start = coordinator
        .begin(
            "server",
            &oauth_config(&server.origin, OAuthConfig::default()),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("login starts");
    let (redirect_uri, state) = login_values(&start.authorization_url);
    let completion = {
        let coordinator = coordinator.clone();
        let login_id = start.login_id.clone();
        tokio::spawn(async move {
            coordinator
                .complete(
                    "server",
                    &login_id,
                    &format!("{redirect_uri}?code=authorization-code&state={state}"),
                )
                .await
        })
    };

    finalization_reached.wait().await;
    let cancellation = {
        let coordinator = coordinator.clone();
        let login_id = start.login_id.clone();
        tokio::spawn(async move { coordinator.cancel("server", &login_id).await })
    };
    release_finalization.wait().await;

    let outcome = tokio::time::timeout(Duration::from_secs(2), async {
        (
            cancellation.await.expect("cancellation task"),
            completion.await.expect("completion task"),
            coordinator
                .state
                .lock()
                .expect("state lock")
                .credentials
                .contains_key("server"),
        )
    })
    .await
    .expect("cancellation and completion are acknowledged");
    assert!(
        [
            (Ok(()), Err(McpOAuthLoginError::Cancelled), false),
            (Err(McpOAuthLoginError::AlreadyCompleted), Ok(()), true),
        ]
        .contains(&outcome),
        "invalid cancellation/commit outcome: {outcome:?}"
    );
    assert_finalized(
        &coordinator,
        "server",
        if outcome.2 {
            CredentialExpectation::Present
        } else {
            CredentialExpectation::Absent
        },
    );
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.target == "/token")
            .count(),
        1
    );
}

#[tokio::test]
async fn cancel_interrupts_token_exchange_and_finalizes_waiters() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        token_delay: Duration::from_secs(1),
        ..OAuthServerOptions::default()
    })
    .await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let start = coordinator
        .begin(
            "server",
            &oauth_config(&server.origin, OAuthConfig::default()),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("login starts");
    let port = callback_port(&start.authorization_url);
    let (redirect_uri, state) = login_values(&start.authorization_url);
    let completion = {
        let coordinator = coordinator.clone();
        let login_id = start.login_id.clone();
        tokio::spawn(async move {
            coordinator
                .complete(
                    "server",
                    &login_id,
                    &format!("{redirect_uri}?code=authorization-code&state={state}"),
                )
                .await
        })
    };
    server.wait_for_target("/token").await;
    tokio::time::timeout(
        Duration::from_millis(250),
        coordinator.cancel("server", &start.login_id),
    )
    .await
    .expect("cancel does not wait for token endpoint")
    .expect("cancel is acknowledged after cleanup");
    assert_eq!(
        completion.await.expect("completion waiter joins"),
        Err(McpOAuthLoginError::Cancelled)
    );
    assert_finalized(&coordinator, "server", CredentialExpectation::Absent);
    TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("cancelled exchange releases callback port");
}

#[tokio::test]
async fn provider_errors_are_generic_and_shutdown_awaits_cleanup() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let coordinator = OAuthCoordinator::new();
    let config = oauth_config(&server.origin, OAuthConfig::default());
    let start = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("login starts");
    let first_port = callback_port(&start.authorization_url);
    let (redirect_uri, state) = login_values(&start.authorization_url);
    let reflected = "client-secret-state-code-access-refresh-registration";
    let error = coordinator
        .complete(
            "server",
            &start.login_id,
            &format!("{redirect_uri}?error={reflected}&state={state}"),
        )
        .await
        .expect_err("provider error");
    assert_eq!(error, McpOAuthLoginError::Provider);
    assert!(!format!("{error:?}").contains(reflected));
    assert!(!error.to_string().contains(reflected));
    assert_finalized(&coordinator, "server", CredentialExpectation::Absent);
    drop(
        TcpListener::bind(("127.0.0.1", first_port))
            .await
            .expect("provider error releases callback port"),
    );

    let active = coordinator
        .begin("server", &config, Instant::now() + Duration::from_secs(5))
        .await
        .expect("another login starts");
    let active_port = callback_port(&active.authorization_url);
    coordinator.shutdown().await;
    assert_finalized(&coordinator, "server", CredentialExpectation::Absent);
    assert_eq!(
        coordinator.cancel("server", &active.login_id).await,
        Err(McpOAuthLoginError::NotFound)
    );
    assert_eq!(
        coordinator
            .begin("server", &config, Instant::now() + Duration::from_secs(5))
            .await,
        Err(McpOAuthLoginError::Unavailable)
    );
    TcpListener::bind(("127.0.0.1", active_port))
        .await
        .expect("shutdown releases callback port");
}

#[derive(Clone, Copy)]
struct OAuthConfig<'a> {
    client_id: Option<&'a str>,
    scopes: Option<&'a [&'a str]>,
    resource: Option<&'a str>,
    callback_port: Option<u16>,
    callback_timeout_ms: u64,
}

impl Default for OAuthConfig<'_> {
    fn default() -> Self {
        Self {
            client_id: None,
            scopes: None,
            resource: None,
            callback_port: None,
            callback_timeout_ms: 2_000,
        }
    }
}

fn oauth_config(origin: &str, oauth: OAuthConfig<'_>) -> McpStreamableHttpTransportConfig {
    serde_json::from_value(serde_json::json!({
        "url": format!("{origin}/mcp?tenant=one"),
        "auth": {
            "type": "oauth",
            "client_id": oauth.client_id,
            "scopes": oauth.scopes,
            "resource": oauth.resource,
            "callback_port": oauth.callback_port,
            "callback_timeout_ms": oauth.callback_timeout_ms,
        }
    }))
    .expect("OAuth config")
}

async fn complete(
    coordinator: &OAuthCoordinator,
    start: &McpOAuthLoginStart,
) -> Result<(), McpOAuthLoginError> {
    let (redirect_uri, state) = login_values(&start.authorization_url);
    coordinator
        .complete(
            "server",
            &start.login_id,
            &format!("{redirect_uri}?code=authorization-code&state={state}"),
        )
        .await
}

fn login_values(authorization_url: &str) -> (String, String) {
    let values = query_values(authorization_url);
    (
        values["redirect_uri"][0].clone(),
        values["state"][0].clone(),
    )
}

fn query_values(url: &str) -> BTreeMap<String, Vec<String>> {
    let mut values = BTreeMap::<String, Vec<String>>::new();
    for (name, value) in reqwest::Url::parse(url).expect("URL parses").query_pairs() {
        values
            .entry(name.into_owned())
            .or_default()
            .push(value.into_owned());
    }
    values
}

pub(crate) async fn send_callback(url: String) -> Vec<u8> {
    let url = reqwest::Url::parse(&url).expect("callback URL");
    let port = url.port().expect("callback port");
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect callback");
    let target = format!("{}?{}", url.path(), url.query().expect("query"));
    stream
        .write_all(
            format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("send callback");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response).await;
    response
}

async fn begin_with_fixed_callback_port(
    origin: &str,
    oauth: OAuthConfig<'_>,
) -> (
    Arc<OAuthCoordinator>,
    McpStreamableHttpTransportConfig,
    McpOAuthLoginStart,
) {
    for port in 20_000..20_128 {
        let coordinator = OAuthCoordinator::new();
        let config = oauth_config(
            origin,
            OAuthConfig {
                callback_port: Some(port),
                ..oauth
            },
        );
        match coordinator
            .begin("server", &config, Instant::now() + Duration::from_secs(5))
            .await
        {
            Ok(start) => {
                assert_eq!(callback_port(&start.authorization_url), port);
                return (coordinator, config, start);
            }
            Err(McpOAuthLoginError::CallbackBind) => coordinator.shutdown().await,
            Err(error) => panic!("fixed-port login failed: {error:?}"),
        }
    }
    panic!("no fixed callback port candidate was available");
}

async fn wait_for_flow(coordinator: &OAuthCoordinator) -> String {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(login_id) = coordinator
                .state
                .lock()
                .expect("state lock")
                .flows
                .keys()
                .next()
                .cloned()
            {
                return login_id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("flow appears")
}

async fn wait_for_finalized(coordinator: &OAuthCoordinator) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let finalized = {
                let state = coordinator.state.lock().expect("state lock");
                state.flows.is_empty() && state.active_by_server.is_empty()
            };
            if finalized {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("flow cleanup");
}

#[derive(Clone, Copy)]
enum CredentialExpectation {
    Present,
    Absent,
}

fn assert_finalized(
    coordinator: &OAuthCoordinator,
    server_id: &str,
    expectation: CredentialExpectation,
) {
    let state = coordinator.state.lock().expect("state lock");
    assert!(state.flows.is_empty());
    assert!(state.active_by_server.is_empty());
    assert_eq!(
        state.credentials.contains_key(server_id),
        matches!(expectation, CredentialExpectation::Present)
    );
}

#[derive(Clone)]
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) body: Vec<u8>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct OAuthServerOptions {
    pub(crate) metadata_delay: Duration,
    pub(crate) token_delay: Duration,
    pub(crate) refresh_delay: Duration,
    pub(crate) token_redirect: bool,
    pub(crate) mcp_unauthorized_once: bool,
    pub(crate) refresh_failures: u64,
    pub(crate) refresh_invalid_grant: bool,
}

pub(crate) struct OAuthServer {
    pub(crate) origin: String,
    requests: Arc<StdMutex<Vec<Request>>>,
    allowed_redirect_uri: Arc<StdMutex<Option<String>>>,
    task: JoinHandle<()>,
}

impl OAuthServer {
    pub(crate) async fn spawn(options: OAuthServerOptions) -> Self {
        let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let address = listener.local_addr().expect("address");
        let origin = format!("http://{address}");
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let task_requests = requests.clone();
        let task_origin = origin.clone();
        let allowed_redirect_uri = Arc::new(StdMutex::new(None::<String>));
        let task_allowed_redirect_uri = allowed_redirect_uri.clone();
        let reject_mcp = Arc::new(AtomicBool::new(options.mcp_unauthorized_once));
        let refresh_failures = Arc::new(AtomicU64::new(options.refresh_failures));
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let requests = task_requests.clone();
                let origin = task_origin.clone();
                let allowed_redirect_uri = task_allowed_redirect_uri.clone();
                let reject_mcp = reject_mcp.clone();
                let refresh_failures = refresh_failures.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut stream).await else {
                        return;
                    };
                    requests.lock().expect("request lock").push(request.clone());
                    let authenticated_mcp = request.target == "/mcp?tenant=one"
                        && request
                            .headers
                            .get("authorization")
                            .map(String::as_str)
                            .is_some_and(|authorization| {
                                matches!(
                                    authorization,
                                    "Bearer access-token" | "Bearer rotated-access-token"
                                )
                            });
                    let reject_request = authenticated_mcp
                        && request.method == "POST"
                        && serde_json::from_slice::<serde_json::Value>(&request.body)
                            .ok()
                            .and_then(|payload| payload["method"].as_str().map(ToString::to_string))
                            .as_deref()
                            == Some("tools/call")
                        && reject_mcp.swap(false, Ordering::AcqRel);
                    let (status, headers, body) = if reject_request {
                        (401, String::new(), String::new())
                    } else if authenticated_mcp {
                        mcp_response(&request)
                    } else {
                        match request.target.as_str() {
                        "/mcp?tenant=one" => (
                            302,
                            "Location: /mcp-final?tenant=one\r\n".to_string(),
                            String::new(),
                        ),
                        "/mcp-final?tenant=one" => (
                            401,
                            format!(
                                "WWW-Authenticate: Bearer resource_metadata=\"{origin}/protected?tenant=one\"\r\n"
                            ),
                            String::new(),
                        ),
                        "/protected?tenant=one" => (
                            200,
                            String::new(),
                            serde_json::json!({
                                "resource": format!("{origin}/mcp?tenant=one"),
                                "authorization_servers": [origin.clone()],
                                "scopes_supported": ["discovered"],
                            })
                            .to_string(),
                        ),
                        "/.well-known/oauth-authorization-server" => (
                            302,
                            "Location: /metadata-final\r\n".to_string(),
                            String::new(),
                        ),
                        "/metadata-final" => {
                            tokio::time::sleep(options.metadata_delay).await;
                            (
                                200,
                                String::new(),
                                serde_json::json!({
                                    "issuer": origin,
                                    "authorization_endpoint": format!(
                                        "{origin}/authorize?audience=existing"
                                    ),
                                    "token_endpoint": if options.token_redirect {
                                        format!("{origin}/token-redirect")
                                    } else {
                                        format!("{origin}/token")
                                    },
                                    "registration_endpoint": format!("{origin}/register-redirect"),
                                    "response_types_supported": ["code"],
                                    "code_challenge_methods_supported": ["S256"],
                                    "scopes_supported": ["discovered"],
                                })
                                .to_string(),
                            )
                        }
                        target if target.starts_with("/authorize?") => {
                            let authorization = reqwest::Url::parse(&format!("{origin}{target}"))
                                .expect("authorization request URL");
                            let redirect_uri = authorization
                                .query_pairs()
                                .find_map(|(name, value)| {
                                    (name == "redirect_uri").then(|| value.into_owned())
                                })
                                .and_then(|redirect_uri| {
                                    (allowed_redirect_uri
                                        .lock()
                                        .expect("allowed redirect lock")
                                        .as_deref()
                                        == Some(redirect_uri.as_str()))
                                    .then(|| {
                                        reqwest::Url::parse(&redirect_uri)
                                            .expect("allowed redirect URI")
                                    })
                                });
                            let state = authorization.query_pairs().find_map(|(name, value)| {
                                (name == "state").then(|| value.into_owned())
                            });
                            match (redirect_uri, state) {
                                (Some(mut redirect_uri), Some(state)) => {
                                    redirect_uri.query_pairs_mut().extend_pairs([
                                        ("code", "authorization-code"),
                                        ("state", state.as_str()),
                                    ]);
                                    (
                                        302,
                                        format!("Location: {redirect_uri}\r\n"),
                                        String::new(),
                                    )
                                }
                                _ => (
                                    400,
                                    String::new(),
                                    serde_json::json!({"error": "invalid_request"}).to_string(),
                                ),
                            }
                        }
                        "/register-redirect" => (
                            307,
                            "Location: /register\r\n".to_string(),
                            String::new(),
                        ),
                        "/register" => {
                            let registration = body_json(&request);
                            (
                                201,
                                String::new(),
                                serde_json::json!({
                                    "client_id": "dynamic-client",
                                    "redirect_uris": registration["redirect_uris"],
                                })
                                .to_string(),
                            )
                        }
                        "/token-redirect" => (
                            307,
                            "Location: /token\r\n".to_string(),
                            String::new(),
                        ),
                        "/token" => {
                            let refresh = String::from_utf8_lossy(&request.body)
                                .contains("grant_type=refresh_token");
                            tokio::time::sleep(if refresh {
                                options.refresh_delay
                            } else {
                                options.token_delay
                            })
                            .await;
                            if refresh {
                                if options.refresh_invalid_grant {
                                    (
                                        400,
                                        String::new(),
                                        serde_json::json!({"error": "invalid_grant"}).to_string(),
                                    )
                                } else if refresh_failures
                                    .fetch_update(
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                        |remaining| remaining.checked_sub(1),
                                    )
                                    .is_ok()
                                {
                                    (
                                        503,
                                        String::new(),
                                        serde_json::json!({"error": "temporarily_unavailable"})
                                            .to_string(),
                                    )
                                } else {
                                    (
                                        200,
                                        String::new(),
                                        serde_json::json!({
                                            "access_token": "rotated-access-token",
                                            "token_type": "Bearer",
                                            "expires_in": 3600,
                                        })
                                        .to_string(),
                                    )
                                }
                            } else {
                                (
                                    200,
                                    String::new(),
                                    serde_json::json!({
                                        "access_token": "access-token",
                                        "refresh_token": "refresh-token",
                                        "token_type": "Bearer",
                                        "expires_in": 3600,
                                        "scope": "read search",
                                    })
                                    .to_string(),
                                )
                            }
                        }
                        target => panic!("unexpected OAuth request {target}"),
                    }
                    };
                    let reply = format!(
                        "HTTP/1.1 {status} Status\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream
                        .write_all(reply.as_bytes())
                        .await
                        .expect("write response");
                });
            }
        });
        Self {
            origin,
            requests,
            allowed_redirect_uri,
            task,
        }
    }

    fn allow_redirect_uri(&self, redirect_uri: String) {
        *self
            .allowed_redirect_uri
            .lock()
            .expect("allowed redirect lock") = Some(redirect_uri);
    }

    pub(crate) fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("request lock").clone()
    }

    pub(crate) async fn wait_for_target(&self, target: &str) {
        self.wait_for_target_count(target, 1).await;
    }

    async fn wait_for_target_count(&self, target: &str, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self
                .requests()
                .iter()
                .filter(|request| request.target == target)
                .count()
                < count
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request target");
    }
}

impl Drop for OAuthServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn request<'a>(requests: &'a [Request], target: &str) -> &'a Request {
    requests
        .iter()
        .find(|request| request.target == target)
        .expect("request target exists")
}

fn body_json(request: &Request) -> serde_json::Value {
    serde_json::from_slice(&request.body).expect("request JSON")
}

fn body_form(request: &Request) -> BTreeMap<String, String> {
    reqwest::Url::parse(&format!(
        "http://localhost/?{}",
        String::from_utf8_lossy(&request.body)
    ))
    .expect("form URL")
    .query_pairs()
    .into_owned()
    .collect()
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
        let Some(head_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head_end = head_end + 4;
        let head = std::str::from_utf8(&bytes[..head_end]).ok()?;
        let content_length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or_default();
        if bytes.len() < head_end + content_length {
            continue;
        }
        let mut request_line = head.lines().next()?.split_ascii_whitespace();
        let method = request_line.next()?.to_string();
        let target = request_line.next()?.to_string();
        let headers = head
            .lines()
            .skip(1)
            .filter_map(|line| {
                line.split_once(':')
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect();
        return Some(Request {
            method,
            target,
            headers,
            body: bytes[head_end..head_end + content_length].to_vec(),
        });
    }
}

fn mcp_response(request: &Request) -> (u16, String, String) {
    if request.method == "DELETE" {
        return (200, String::new(), String::new());
    }
    if request.method == "GET" {
        return (405, String::new(), String::new());
    }
    let payload: serde_json::Value =
        serde_json::from_slice(&request.body).expect("MCP request JSON");
    if payload["method"] == "notifications/initialized" {
        return (202, String::new(), String::new());
    }
    let result = match payload["method"].as_str() {
        Some("initialize") => serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "oauth-fixture", "version": "1"},
        }),
        Some("tools/list") => serde_json::json!({
            "tools": [{
                "name": "echo",
                "description": "Echo a value",
                "inputSchema": {
                    "type": "object",
                    "properties": {"value": {"type": "string"}}
                }
            }]
        }),
        Some("tools/call") => serde_json::json!({
            "content": [{
                "type": "text",
                "text": payload["params"]["arguments"]["value"]
            }]
        }),
        method => panic!("unexpected MCP method {method:?}"),
    };
    let headers = if payload["method"] == "initialize" {
        "Mcp-Session-Id: oauth-session\r\n".to_string()
    } else {
        String::new()
    };
    (
        200,
        headers,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": payload["id"],
            "result": result,
        })
        .to_string(),
    )
}
