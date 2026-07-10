use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_tools::{ProviderTool, ToolRegistry};
use agent_vocab::ProviderKind;
use pretty_assertions::assert_eq;
use tokio::net::TcpListener;
use tokio::sync::Barrier;

use super::*;
use crate::oauth_login::tests::{
    send_callback, OAuthServer, OAuthServerOptions, CALLBACK_PORT_TEST_LOCK,
};
use crate::{McpOAuthLoginError, OAuthCredentialStoreError};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[tokio::test]
async fn oauth_route_is_immediately_login_required_without_blocking_healthy_routes() {
    let oauth_listener = {
        let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
        TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind OAuth observation listener")
    };
    let oauth_address = oauth_listener.local_addr().expect("OAuth address");
    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "stdio": {
                "transport": {
                    "type": "stdio",
                    "command": env!("AGENT_MCP_FAKE_SERVER"),
                    "env": {"MCP_FIXTURE_MODE": "simple"}

                },
                "allow_all_tools": true
            },
            "oauth": {
                "transport": {
                    "type": "streamable_http",
                    "url": format!("https://{oauth_address}/mcp"),
                    "auth": {"type": "oauth"}
                },
                "allow_all_tools": true
            }
        }
    }))
    .expect("mixed config parses");

    let manager = tokio::time::timeout(Duration::from_secs(2), McpManager::start(config))
        .await
        .expect("OAuth does not block startup")
        .expect("manager starts");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), oauth_listener.accept())
            .await
            .is_err()
    );
    assert_eq!(
        manager.auth_statuses().await,
        vec![
            crate::McpAuthServerStatus {
                server: "oauth".to_string(),
                auth_kind: crate::McpAuthKind::Oauth,
                auth_state: McpAuthStatus::Unsupported,
                can_login: false,
                can_logout: false,
                failure: None,
            },
            crate::McpAuthServerStatus {
                server: "stdio".to_string(),
                auth_kind: crate::McpAuthKind::None,
                auth_state: McpAuthStatus::NonOauth,
                can_login: false,
                can_logout: false,
                failure: None,
            },
        ]
    );

    let first_party = first_party();
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party)
        .await
        .expect("mixed inventory remains coherent");
    assert_eq!(
        inventory
            .servers
            .iter()
            .map(|server| (server.server.as_str(), server.health))
            .collect::<Vec<_>>(),
        vec![
            ("oauth", McpHealth::Unavailable),
            ("stdio", McpHealth::Healthy),
        ]
    );

    let snapshot = manager
        .select(
            &McpSessionSelection {
                inventory_revision: inventory.revision,
                servers: vec![McpServerSelection {
                    server: "stdio".to_string(),
                    tools: vec!["read".to_string()],
                }],
            },
            &first_party,
        )
        .await
        .expect("healthy route selects");
    assert_eq!(
        snapshot
            .manifest()
            .tools
            .iter()
            .map(|tool| (tool.server_id.as_str(), tool.raw_name.as_str()))
            .collect::<Vec<_>>(),
        vec![("stdio", "read")]
    );
    for provider in [ProviderKind::OpenAi, ProviderKind::Claude] {
        assert!(snapshot
            .provider_tools(provider)
            .iter()
            .all(|tool| !tool.name.contains("oauth")));
    }

    let prompt = agent_prompt::render_prompt(
        include_str!("../../../../PI.md"),
        &agent_prompt::PromptContext {
            profile: agent_prompt::PromptProfile::Parent,
            cwd: PathBuf::from("/unused"),
            has_project: false,
            workspaces: Vec::new(),
            tools: Vec::new(),
            skills: Vec::new(),
            subagent_roles: Vec::new(),
            mcp_servers: vec![agent_prompt::PromptMcpServer {
                server: "stdio".to_string(),
                tools: vec!["read".to_string()],
            }],
        },
    );
    let selected_prompt_section = prompt
        .split_once("### Selected MCP tools")
        .expect("selected MCP prompt section exists")
        .1
        .split_once("## Subagent delegation")
        .expect("selected MCP prompt section is bounded")
        .0;
    assert!(selected_prompt_section.contains("- stdio:"));
    assert!(!selected_prompt_section.contains("oauth"));

    manager.shutdown().await;
}

#[tokio::test]
async fn shutdown_during_refresh_preserves_old_durable_credential() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        refresh_delay: Duration::from_secs(1),
        mcp_unauthorized_once: true,
        ..OAuthServerOptions::default()
    })
    .await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let temp = TempDir::new();
    let path = temp.path.join("credentials.json");
    let manager =
        McpManager::start_with_credential_file(oauth_manager_config(&server.origin), path.clone())
            .await
            .expect("manager starts");
    login(&manager).await;
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("logged-in inventory loads");
    let snapshot = select_echo(&manager, inventory).await;
    let tool = &snapshot.manifest().tools[0];
    assert!(manager
        .call(
            &snapshot,
            &tool.exposed_name,
            serde_json::json!({"value": "once"}),
        )
        .await
        .is_err());
    let original = fs::read(&path).expect("read credential file");
    let refresh = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .inventory(ProviderKind::OpenAi, &first_party())
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while refresh_request_count(&server) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("refresh reaches token endpoint");

    tokio::time::timeout(Duration::from_millis(250), manager.shutdown())
        .await
        .expect("shutdown cancels refresh promptly");
    let _ = refresh.await;
    assert_eq!(fs::read(path).expect("old credential remains"), original);
}

#[tokio::test]
async fn unavailable_credential_stores_preserve_files_and_do_not_block_stdio_routes() {
    let temp = TempDir::new();
    let empty = temp.path.join("empty.json");
    fs::write(&empty, []).expect("write empty store");
    assert_unavailable_store(&empty, OAuthCredentialStoreError::Empty).await;
    assert_eq!(
        fs::read(&empty).expect("empty store remains"),
        Vec::<u8>::new()
    );

    let corrupt = temp.path.join("corrupt.json");
    let corrupt_bytes = br#"{"credentials":"access-token"}"#;
    fs::write(&corrupt, corrupt_bytes).expect("write corrupt store");
    assert_unavailable_store(&corrupt, OAuthCredentialStoreError::Corrupt).await;
    assert_eq!(
        fs::read(&corrupt).expect("corrupt store remains"),
        corrupt_bytes
    );

    let oversized = temp.path.join("oversized.json");
    let file = fs::File::create(&oversized).expect("create oversized store");
    file.set_len(1024 * 1024 + 1)
        .expect("extend oversized store");
    assert_unavailable_store(&oversized, OAuthCredentialStoreError::Oversized).await;
    assert_eq!(
        fs::metadata(&oversized)
            .expect("oversized store remains")
            .len(),
        1024 * 1024 + 1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unreadable_credential_store_does_not_block_stdio_route() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new();
    let path = temp.path.join("unreadable.json");
    let bytes = br#"{"version":1,"credentials":{}}"#;
    fs::write(&path, bytes).expect("write store");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("make store unreadable");
    assert_unavailable_store(&path, OAuthCredentialStoreError::Io).await;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("restore permissions");
    assert_eq!(fs::read(path).expect("unreadable store remains"), bytes);
}

#[tokio::test]
async fn repeated_status_for_expired_refreshable_credential_is_observational() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let temp = TempDir::new();
    let path = temp.path.join("credentials.json");
    let repository = crate::oauth_credentials::OAuthCredentialRepository::open_file(path.clone())
        .expect("credential repository opens");
    let manager = McpManager::start_with_repository(
        oauth_manager_config_with_client(&server.origin, Some("static-client")),
        None,
        repository.clone(),
    )
    .await
    .expect("manager starts without credentials");
    repository
        .save(stored_credential(
            &server.origin,
            Some("static-client"),
            "static-client",
            Some("refresh-token"),
            Some(crate::oauth_credentials::unix_millis().saturating_sub(1)),
        ))
        .await
        .expect("expired credential saves");
    let before_bytes = fs::read(&path).expect("read credential file");
    let before_modified = fs::metadata(&path)
        .expect("credential metadata")
        .modified()
        .expect("modified timestamp");
    let before_generation = manager
        .servers
        .read()
        .await
        .get("oauth")
        .expect("OAuth server")
        .refresh
        .generation;

    for _ in 0..2 {
        assert_eq!(
            manager.oauth_status("oauth").await,
            McpAuthStatus::OauthReady
        );
    }

    assert_eq!(
        fs::read(&path).expect("credential file remains"),
        before_bytes
    );
    assert_eq!(
        fs::metadata(&path)
            .expect("credential metadata")
            .modified()
            .expect("modified timestamp"),
        before_modified
    );
    assert_eq!(
        manager
            .servers
            .read()
            .await
            .get("oauth")
            .expect("OAuth server")
            .refresh
            .generation,
        before_generation
    );
    assert_eq!(refresh_request_count(&server), 0);
    manager.shutdown().await;
}

#[tokio::test]
async fn transient_first_refresh_failure_retries_without_login() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        refresh_failures: 1,
        ..OAuthServerOptions::default()
    })
    .await;
    let temp = TempDir::new();
    let path = temp.path.join("credentials.json");
    let repository = crate::oauth_credentials::OAuthCredentialRepository::open_file(path.clone())
        .expect("credential repository opens");
    repository
        .save(stored_credential(
            &server.origin,
            Some("static-client"),
            "static-client",
            Some("refresh-token"),
            Some(crate::oauth_credentials::unix_millis().saturating_sub(1)),
        ))
        .await
        .expect("expired credential saves");
    let old_bytes = fs::read(&path).expect("read old credential file");
    drop(repository);

    let manager = McpManager::start_with_credential_file(
        oauth_manager_config_with_client(&server.origin, Some("static-client")),
        path.clone(),
    )
    .await
    .expect("manager survives transient refresh failure");
    assert_eq!(fs::read(&path).expect("old store remains"), old_bytes);
    assert_eq!(refresh_request_count(&server), 1);

    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("later acquisition retries refresh");
    assert_eq!(inventory.servers[0].health, McpHealth::Healthy);
    assert_eq!(refresh_request_count(&server), 2);
    assert!(fs::read_to_string(path)
        .expect("rotated store reads")
        .contains("rotated-access-token"));
    manager.shutdown().await;
}

#[tokio::test]
async fn whitespace_client_id_dcr_persists_and_authenticates_after_restart() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let temp = TempDir::new();
    let path = temp.path.join("credentials.json");
    let config = oauth_manager_config_with_client(&server.origin, Some(" \t "));
    let manager = McpManager::start_with_credential_file(config.clone(), path.clone())
        .await
        .expect("manager starts");
    login(&manager).await;
    manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("DCR login connects");
    manager.shutdown().await;

    let contents: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("credential file reads"))
            .expect("credential JSON");
    let entry = contents["credentials"]
        .as_object()
        .and_then(|credentials| credentials.values().next())
        .expect("stored credential");
    assert_eq!(entry["configured_client_id"], serde_json::Value::Null);
    assert_eq!(entry["client_id"], "dynamic-client");

    let restarted = McpManager::start_with_credential_file(config, path)
        .await
        .expect("restart restores normalized DCR credential");
    let inventory = restarted
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("restart authenticates");
    assert_eq!(inventory.servers[0].health, McpHealth::Healthy);
    assert!(server.requests().iter().any(|request| {
        request.headers.get("authorization").map(String::as_str) == Some("Bearer access-token")
    }));
    restarted.shutdown().await;
}

#[tokio::test]
async fn persisted_login_reconnects_and_restores_bounded_authenticated_route() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let temp = TempDir::new();
    let credentials_path = temp.path.join("credentials.json");
    let config = oauth_manager_config(&server.origin);
    let manager = McpManager::start_with_credential_file(config.clone(), credentials_path.clone())
        .await
        .expect("manager starts login-required");
    let login_start = manager
        .begin_oauth_login("oauth")
        .await
        .expect("OAuth login starts");
    let authorization =
        reqwest::Url::parse(&login_start.authorization_url).expect("authorization URL");
    let values = authorization
        .query_pairs()
        .into_owned()
        .collect::<HashMap<_, _>>();
    let response = send_callback(format!(
        "{}?code=authorization-code&state={}",
        values["redirect_uri"], values["state"]
    ))
    .await;
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let serialized = fs::read_to_string(&credentials_path).expect("credential file exists");
    assert!(serialized.contains("access-token"));
    assert!(serialized.contains("refresh-token"));
    assert!(serialized.contains("dynamic-client"));

    let first_inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("completed login reconnects on inventory");
    assert_eq!(first_inventory.servers[0].health, McpHealth::Healthy);
    assert_eq!(
        manager.oauth_status("oauth").await,
        McpAuthStatus::OauthReady
    );
    manager.shutdown().await;

    let restarted = McpManager::start_with_credential_file(config, credentials_path.clone())
        .await
        .expect("restart restores OAuth credentials");
    let inventory = restarted
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("restored route inventories");
    let snapshot = select_echo(&restarted, inventory).await;
    let manifest_before_logout =
        serde_json::to_vec(snapshot.manifest()).expect("manifest serializes");
    let tool = &snapshot.manifest().tools[0];
    assert_eq!(
        restarted
            .call(
                &snapshot,
                &tool.exposed_name,
                serde_json::json!({"value": "hello"}),
            )
            .await
            .expect("restored OAuth call succeeds"),
        McpCallOutput {
            output: "hello".to_string(),
            is_error: false,
        }
    );

    assert_eq!(
        restarted
            .logout_oauth("oauth")
            .await
            .expect("logout succeeds"),
        McpLogoutResult::Removed
    );
    assert!(!fs::read_to_string(&credentials_path)
        .unwrap_or_default()
        .contains("access-token"));
    assert_eq!(
        serde_json::to_vec(snapshot.manifest()).expect("manifest remains serializable"),
        manifest_before_logout
    );
    assert!(matches!(
        restarted
            .call(
                &snapshot,
                &tool.exposed_name,
                serde_json::json!({"value": "after"}),
            )
            .await,
        Err(McpCallError::ServerUnavailable { .. })
    ));
    assert_eq!(
        restarted.oauth_status("oauth").await,
        McpAuthStatus::LoginRequired
    );
    login(&restarted).await;
    restarted
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("compatible re-login reconnects");
    assert_eq!(
        restarted
            .call(
                &snapshot,
                &tool.exposed_name,
                serde_json::json!({"value": "restored"}),
            )
            .await
            .expect("frozen route resumes after compatible re-login"),
        McpCallOutput {
            output: "restored".to_string(),
            is_error: false,
        }
    );
    assert_eq!(
        serde_json::to_vec(snapshot.manifest()).expect("manifest remains stable after re-login"),
        manifest_before_logout
    );
    restarted.shutdown().await;

    let requests = server.requests();
    for method in ["POST", "GET", "DELETE"] {
        assert!(
            requests.iter().any(|request| {
                request.method == method
                    && request.headers.get("authorization").map(String::as_str)
                        == Some("Bearer access-token")
            }),
            "{method}"
        );
    }
}

#[tokio::test]
async fn oauth_tools_call_401_is_not_replayed_and_later_inventory_refreshes() {
    let server = OAuthServer::spawn(OAuthServerOptions {
        mcp_unauthorized_once: true,
        ..OAuthServerOptions::default()
    })
    .await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let temp = TempDir::new();
    let credentials_path = temp.path.join("credentials.json");
    let manager = McpManager::start_with_credential_file(
        oauth_manager_config(&server.origin),
        credentials_path,
    )
    .await
    .expect("manager starts");
    login(&manager).await;
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("logged-in inventory loads");
    let snapshot = select_echo(&manager, inventory).await;
    let tool = &snapshot.manifest().tools[0];

    assert!(matches!(
        manager
            .call(
                &snapshot,
                &tool.exposed_name,
                serde_json::json!({"value": "once"}),
            )
            .await,
        Err(McpCallError::Protocol { .. })
    ));
    assert_eq!(mcp_method_count(&server, "tools/call"), 1);

    manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("later inventory refreshes and reconnects");
    assert_eq!(
        manager
            .call(
                &snapshot,
                &tool.exposed_name,
                serde_json::json!({"value": "later"}),
            )
            .await
            .expect("later call uses refreshed route"),
        McpCallOutput {
            output: "later".to_string(),
            is_error: false,
        }
    );
    let requests = server.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.target == "/token"
                    && String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token")
            })
            .count(),
        1
    );
    assert!(requests.iter().any(|request| {
        request.headers.get("authorization").map(String::as_str)
            == Some("Bearer rotated-access-token")
    }));
    manager.shutdown().await;
}

#[tokio::test]
async fn login_persistence_failure_is_not_reported_as_success() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let temp = TempDir::new();
    let blocked_parent = temp.path.join("credential-parent");
    fs::create_dir(&blocked_parent).expect("create credential parent");
    let manager = McpManager::start_with_credential_file(
        oauth_manager_config(&server.origin),
        blocked_parent.join("credentials.json"),
    )
    .await
    .expect("empty credential path starts");
    fs::remove_dir(&blocked_parent).expect("remove empty credential parent");
    fs::write(&blocked_parent, "blocked").expect("replace parent with blocking file");
    let login = manager
        .begin_oauth_login("oauth")
        .await
        .expect("OAuth login starts");
    let authorization = reqwest::Url::parse(&login.authorization_url).expect("authorization URL");
    let values = authorization
        .query_pairs()
        .into_owned()
        .collect::<HashMap<_, _>>();

    assert_eq!(
        manager
            .complete_oauth_login(
                "oauth",
                &login.login_id,
                &format!(
                    "{}?code=authorization-code&state={}",
                    values["redirect_uri"], values["state"]
                ),
            )
            .await,
        Err(McpOAuthLoginError::Persistence)
    );
    assert_eq!(
        manager.oauth_status("oauth").await,
        McpAuthStatus::LoginRequired
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn delayed_persistence_failure_stays_pending_until_store_failure() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let temp = TempDir::new();
    let blocked_parent = temp.path.join("credential-parent");
    fs::create_dir(&blocked_parent).expect("create credential parent");
    let manager = McpManager::start_with_credential_file(
        oauth_manager_config(&server.origin),
        blocked_parent.join("credentials.json"),
    )
    .await
    .expect("empty credential path starts");
    fs::remove_dir(&blocked_parent).expect("remove empty credential parent");
    fs::write(&blocked_parent, "blocked").expect("replace parent with blocking file");
    let reached = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    manager
        .oauth
        .set_persistence_barriers((reached.clone(), release.clone()));
    let login = manager
        .begin_oauth_login("oauth")
        .await
        .expect("OAuth login starts");
    let authorization = reqwest::Url::parse(&login.authorization_url).expect("authorization URL");
    let values = authorization
        .query_pairs()
        .into_owned()
        .collect::<HashMap<_, _>>();
    let completion = {
        let manager = manager.clone();
        let login_id = login.login_id.clone();
        tokio::spawn(async move {
            manager
                .complete_oauth_login(
                    "oauth",
                    &login_id,
                    &format!(
                        "{}?code=authorization-code&state={}",
                        values["redirect_uri"], values["state"]
                    ),
                )
                .await
        })
    };

    reached.wait().await;
    assert_eq!(
        manager.oauth_status("oauth").await,
        McpAuthStatus::AuthorizationPending
    );
    assert_eq!(
        manager.cancel_oauth_login("oauth", &login.login_id).await,
        Err(McpOAuthLoginError::AlreadyCompleted)
    );
    assert_eq!(
        manager.begin_oauth_login("oauth").await,
        Err(McpOAuthLoginError::AlreadyPending)
    );
    assert!(!completion.is_finished());

    release.wait().await;
    assert_eq!(
        completion.await.expect("completion task"),
        Err(McpOAuthLoginError::Persistence)
    );
    assert_eq!(
        manager.oauth_status("oauth").await,
        McpAuthStatus::LoginRequired
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn delayed_persistence_success_stays_pending_until_ready() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let manager = McpManager::start(oauth_manager_config(&server.origin))
        .await
        .expect("manager starts");
    let reached = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    manager
        .oauth
        .set_persistence_barriers((reached.clone(), release.clone()));
    let login = manager
        .begin_oauth_login("oauth")
        .await
        .expect("OAuth login starts");
    let authorization = reqwest::Url::parse(&login.authorization_url).expect("authorization URL");
    let values = authorization
        .query_pairs()
        .into_owned()
        .collect::<HashMap<_, _>>();
    let completion = {
        let manager = manager.clone();
        let login_id = login.login_id.clone();
        tokio::spawn(async move {
            manager
                .complete_oauth_login(
                    "oauth",
                    &login_id,
                    &format!(
                        "{}?code=authorization-code&state={}",
                        values["redirect_uri"], values["state"]
                    ),
                )
                .await
        })
    };

    reached.wait().await;
    assert_eq!(
        manager.oauth_status("oauth").await,
        McpAuthStatus::AuthorizationPending
    );
    assert_eq!(
        manager.cancel_oauth_login("oauth", &login.login_id).await,
        Err(McpOAuthLoginError::AlreadyCompleted)
    );
    assert_eq!(
        manager.begin_oauth_login("oauth").await,
        Err(McpOAuthLoginError::AlreadyPending)
    );
    assert!(!completion.is_finished());

    release.wait().await;
    assert_eq!(completion.await.expect("completion task"), Ok(()));
    assert_eq!(
        manager.oauth_status("oauth").await,
        McpAuthStatus::OauthReady
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn logout_between_callback_cleanup_and_persistence_wins() {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let _callback_port = CALLBACK_PORT_TEST_LOCK.lock().await;
    let temp = TempDir::new();
    let credentials_path = temp.path.join("credentials.json");
    let manager = McpManager::start_with_credential_file(
        oauth_manager_config(&server.origin),
        credentials_path.clone(),
    )
    .await
    .expect("manager starts");
    let reached = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    manager
        .oauth
        .set_persistence_barriers((reached.clone(), release.clone()));
    let login = manager
        .begin_oauth_login("oauth")
        .await
        .expect("OAuth login starts");
    let authorization = reqwest::Url::parse(&login.authorization_url).expect("authorization URL");
    let values = authorization
        .query_pairs()
        .into_owned()
        .collect::<HashMap<_, _>>();
    let completion = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .complete_oauth_login(
                    "oauth",
                    &login.login_id,
                    &format!(
                        "{}?code=authorization-code&state={}",
                        values["redirect_uri"], values["state"]
                    ),
                )
                .await
        })
    };

    reached.wait().await;
    assert_eq!(
        manager
            .logout_oauth("oauth")
            .await
            .expect("logout succeeds"),
        McpLogoutResult::NotFound
    );
    release.wait().await;
    assert_eq!(
        completion.await.expect("completion task"),
        Err(McpOAuthLoginError::Unavailable)
    );
    assert!(!credentials_path.exists());
    manager.shutdown().await;
}

fn first_party() -> HashMap<ProviderKind, Vec<ProviderTool>> {
    let registry = ToolRegistry::with_builtin_tools();
    [ProviderKind::OpenAi, ProviderKind::Claude]
        .into_iter()
        .map(|provider| (provider, registry.provider_tools_for_provider(provider)))
        .collect()
}

fn oauth_manager_config(origin: &str) -> McpConfig {
    oauth_manager_config_with_client(origin, None)
}

fn oauth_manager_config_with_client(origin: &str, client_id: Option<&str>) -> McpConfig {
    serde_json::from_value(serde_json::json!({
        "servers": {
            "oauth": {
                "transport": {
                    "type": "streamable_http",
                    "url": format!("{origin}/mcp?tenant=one"),
                    "auth": {
                        "type": "oauth",
                        "client_id": client_id,
                    }
                },
                "allow_all_tools": true
            }
        }
    }))
    .expect("OAuth manager config")
}

fn mixed_manager_config(origin: &str) -> McpConfig {
    serde_json::from_value(serde_json::json!({
        "servers": {
            "oauth": {
                "transport": {
                    "type": "streamable_http",
                    "url": format!("{origin}/mcp?tenant=one"),
                    "auth": {"type": "oauth"}
                },
                "allow_all_tools": true
            },
            "stdio": {
                "transport": {
                    "type": "stdio",
                    "command": env!("AGENT_MCP_FAKE_SERVER"),
                    "env": {"MCP_FIXTURE_MODE": "simple"}
                },
                "allow_all_tools": true
            }
        }
    }))
    .expect("mixed manager config")
}

async fn assert_unavailable_store(path: &std::path::Path, expected: OAuthCredentialStoreError) {
    let server = OAuthServer::spawn(OAuthServerOptions::default()).await;
    let manager =
        McpManager::start_with_credential_file(mixed_manager_config(&server.origin), path.into())
            .await
            .expect("manager starts with unavailable OAuth store");
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("healthy route remains usable");
    assert_eq!(
        inventory
            .servers
            .iter()
            .map(|server| (server.server.as_str(), server.health))
            .collect::<Vec<_>>(),
        vec![
            ("oauth", McpHealth::Unavailable),
            ("stdio", McpHealth::Healthy),
        ]
    );
    assert_eq!(manager.oauth_status("oauth").await, McpAuthStatus::Unknown);
    assert_eq!(
        manager.begin_oauth_login("oauth").await,
        Err(McpOAuthLoginError::Persistence)
    );
    assert_eq!(manager.logout_oauth("oauth").await, Err(expected));
    manager.shutdown().await;
}

fn stored_credential(
    origin: &str,
    configured_client_id: Option<&str>,
    client_id: &str,
    refresh_token: Option<&str>,
    expires_at_millis: Option<u64>,
) -> crate::oauth_credentials::StoredOAuthCredential {
    crate::oauth_credentials::StoredOAuthCredential {
        server_id: "oauth".to_string(),
        server_url: format!("{origin}/mcp?tenant=one"),
        configured_client_id: configured_client_id.map(ToString::to_string),
        resource: None,
        client_id: client_id.to_string(),
        access_token: "access-token".to_string(),
        refresh_token: refresh_token.map(ToString::to_string),
        expires_at_millis,
        granted_scopes: vec!["read".to_string(), "search".to_string()],
    }
}

async fn login(manager: &McpManager) {
    let login = manager
        .begin_oauth_login("oauth")
        .await
        .expect("OAuth login starts");
    let authorization = reqwest::Url::parse(&login.authorization_url).expect("authorization URL");
    let values = authorization
        .query_pairs()
        .into_owned()
        .collect::<HashMap<_, _>>();
    manager
        .complete_oauth_login(
            "oauth",
            &login.login_id,
            &format!(
                "{}?code=authorization-code&state={}",
                values["redirect_uri"], values["state"]
            ),
        )
        .await
        .expect("OAuth login completes");
}

async fn select_echo(manager: &McpManager, inventory: McpInventory) -> McpSessionSnapshot {
    manager
        .select(
            &McpSessionSelection {
                inventory_revision: inventory.revision,
                servers: vec![McpServerSelection {
                    server: "oauth".to_string(),
                    tools: vec!["echo".to_string()],
                }],
            },
            &first_party(),
        )
        .await
        .expect("OAuth route selects")
}

fn mcp_method_count(server: &OAuthServer, method: &str) -> usize {
    server
        .requests()
        .iter()
        .filter(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .ok()
                .and_then(|payload| payload["method"].as_str().map(ToString::to_string))
                .as_deref()
                == Some(method)
        })
        .count()
}

fn refresh_request_count(server: &OAuthServer) -> usize {
    server
        .requests()
        .iter()
        .filter(|request| {
            request.target == "/token"
                && String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token")
        })
        .count()
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pi-relay-oauth-manager-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create manager temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
