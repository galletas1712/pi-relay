use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};

use agent_store::PostgresAgentStore;
use agent_tools::ToolRegistry;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};

use crate::provider_runtime::{ProviderConnectionRegistry, SessionTitleScheduler};
use crate::runtime_hosts::test_support::{connect_test_runtime, TEST_RUNTIME_ID};
use crate::runtime_hosts::RuntimeRegistry;
use crate::state::AppState;

#[tokio::test]
async fn public_rpc_oauth_lifecycle_is_sanitized_and_manifest_neutral() {
    let Some((mut state, admin_url, database_url, database_name, state_dir)) = test_state().await
    else {
        eprintln!("SKIPPED PostgreSQL MCP OAuth RPC test: PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    let server = OAuthMcpServer::spawn().await;
    let config: agent_mcp::McpConfig = serde_json::from_value(json!({
        "servers": {
            "oauth": {
                "transport": {
                    "type": "streamable_http",
                    "url": format!("{}/mcp", server.origin),
                    "auth": {
                        "type": "oauth",
                        "client_id": "public-client",
                        "scopes": ["read"]
                    }
                },
                "allow_all_tools": true,
            }
        }
    }))
    .expect("OAuth MCP config");
    state.mcp = agent_mcp::McpManager::start(config)
        .await
        .expect("manager starts login-required");
    let manifests_before = manifest_count(&database_url).await;
    let reflected = "secret-code-state-field";
    for (method, params, message) in [
        (
            "mcp.status",
            json!({"unexpected": reflected}),
            "Invalid parameters for mcp.status",
        ),
        (
            "mcp.login",
            json!({"server": {"secret": reflected}}),
            "Invalid parameters for mcp.login",
        ),
        (
            "mcp.complete",
            json!({
                "server": "oauth",
                "login_id": "0000000000000001",
                "callback_url": {"secret": reflected},
            }),
            "Invalid parameters for mcp.complete",
        ),
        (
            "mcp.cancel",
            json!({"server": "oauth", "login_id": {"secret": reflected}}),
            "Invalid parameters for mcp.cancel",
        ),
        (
            "mcp.logout",
            json!({"server": [reflected]}),
            "Invalid parameters for mcp.logout",
        ),
    ] {
        let error = public_rpc(&state, method, params)
            .await
            .expect_err("malformed params reject");
        assert_eq!(
            (error.code.as_str(), error.message.as_str(), &error.data),
            ("invalid_params", message, &json!({})),
            "{method}"
        );
        assert!(!format!("{error:?}").contains(reflected));
    }

    let initial = public_rpc(&state, "mcp.status", json!({}))
        .await
        .expect("status succeeds");
    assert_eq!(
        initial,
        json!({
            "servers": [{
                "server": "oauth",
                "auth_kind": "oauth",
                "auth_state": "login_required",
                "can_login": true,
                "can_logout": false,
            }]
        })
    );

    let login = public_rpc(&state, "mcp.login", json!({ "server": "oauth" }))
        .await
        .expect("login starts");
    assert_eq!(
        login
            .as_object()
            .expect("login response")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["authorization_url", "expires_at_unix_seconds", "login_id"]
    );
    let authorization_url = login["authorization_url"]
        .as_str()
        .expect("authorization URL");
    let authorization = reqwest::Url::parse(authorization_url).expect("authorization URL parses");
    let values = authorization
        .query_pairs()
        .into_owned()
        .collect::<BTreeMap<_, _>>();
    assert_eq!(values["client_id"], "public-client");
    assert_eq!(values["code_challenge_method"], "S256");

    let pending = public_rpc(&state, "mcp.status", json!({}))
        .await
        .expect("pending status succeeds");
    assert_eq!(
        pending["servers"][0]["auth_state"],
        Value::String("authorization_pending".to_string())
    );
    let callback_url = format!(
        "{}?code=authorization-code&state={}",
        values["redirect_uri"], values["state"]
    );
    let completed = public_rpc(
        &state,
        "mcp.complete",
        json!({
            "server": "oauth",
            "login_id": login["login_id"],
            "callback_url": callback_url,
        }),
    )
    .await
    .expect("manual callback completes");
    assert_eq!(completed, json!({ "completed": true }));
    assert!(!format!("{completed:?}").contains("authorization-code"));
    assert!(!format!("{completed:?}").contains(&values["state"]));

    let ready = public_rpc(&state, "mcp.status", json!({}))
        .await
        .expect("ready status succeeds");
    assert_eq!(ready["servers"][0]["auth_state"], "ready");
    assert_eq!(ready["servers"][0]["can_logout"], true);
    let inventory = public_rpc(&state, "mcp.inventory", json!({ "provider": "openai" }))
        .await
        .expect("authenticated inventory succeeds");
    assert_eq!(inventory["servers"][0]["server"], "oauth");
    assert_eq!(inventory["servers"][0]["tools"][0]["raw_name"], "echo");

    let logout = public_rpc(&state, "mcp.logout", json!({ "server": "oauth" }))
        .await
        .expect("logout succeeds");
    assert_eq!(logout, json!({ "result": "removed" }));
    let logged_out = public_rpc(&state, "mcp.status", json!({}))
        .await
        .expect("logged-out status succeeds");
    assert_eq!(logged_out["servers"][0]["auth_state"], "login_required");
    assert_eq!(manifest_count(&database_url).await, manifests_before);

    let invalid = public_rpc(
        &state,
        "mcp.complete",
        json!({
            "server": "oauth",
            "login_id": "0000000000000001",
            "callback_url": "https://provider.example/callback?code=secret-code&state=secret-state",
        }),
    )
    .await
    .expect_err("unknown login rejects");
    let error = format!("{invalid:?}");
    assert!(!error.contains("secret-code"));
    assert!(!error.contains("secret-state"));

    state.mcp.shutdown().await;
    state.repo.close().await;
    cleanup_database(&admin_url, &database_name).await;
    let _ = std::fs::remove_dir_all(state_dir);
}

async fn public_rpc(
    state: &AppState,
    method: &str,
    params: Value,
) -> std::result::Result<Value, crate::types::RpcError> {
    crate::dispatch_request(
        state,
        &mut std::collections::BTreeSet::new(),
        &mut BTreeMap::new(),
        method.to_string(),
        params,
    )
    .await
}

async fn manifest_count(database_url: &str) -> i64 {
    let pool = sqlx::PgPool::connect(database_url)
        .await
        .expect("connect manifest observer");
    let count = sqlx::query_scalar("select count(*) from mcp_session_manifests")
        .fetch_one(&pool)
        .await
        .expect("manifest count");
    pool.close().await;
    count
}

async fn test_state() -> Option<(AppState, String, String, String, PathBuf)> {
    let admin_url = std::env::var("PI_RELAY_TEST_DATABASE_URL").ok()?;
    let database_name = format!("pi_relay_mcp_oauth_test_{}", uuid::Uuid::new_v4().simple());
    let admin = sqlx::PgPool::connect(&admin_url)
        .await
        .expect("connect test database admin");
    sqlx::query(&format!(r#"create database "{database_name}""#))
        .execute(&admin)
        .await
        .expect("create isolated database");
    admin.close().await;
    let database_url = database_url_with_name(&admin_url, &database_name);
    let store = PostgresAgentStore::connect(&database_url)
        .await
        .expect("connect isolated database");
    store.migrate().await.expect("migrate isolated database");
    let state_dir = std::env::temp_dir().join(&database_name);
    std::fs::create_dir_all(&state_dir).expect("create state directory");
    let (events, _) = broadcast::channel(16);
    let repo = Arc::new(store);
    let runtime_hosts = RuntimeRegistry::new(repo.clone());
    connect_test_runtime(&runtime_hosts, TEST_RUNTIME_ID).await;
    let state = AppState {
        repo,
        active: Arc::new(Mutex::new(HashMap::new())),
        session_driver_locks: Arc::new(Mutex::new(HashMap::new())),
        tasks: Arc::new(StdMutex::new(HashMap::new())),
        auxiliary_tasks: Arc::new(StdMutex::new(Vec::new())),
        task_registration_lock: Arc::new(StdMutex::new(())),
        post_compaction_recovery_scheduled: Arc::new(AtomicBool::new(false)),
        post_compaction_recovery_notify: Arc::new(tokio::sync::Notify::new()),
        post_compaction_recovery_task: Arc::new(StdMutex::new(None)),
        shutting_down: Arc::new(AtomicBool::new(false)),
        events,
        tools: Arc::new(ToolRegistry::with_builtin_tools()),
        mcp: agent_mcp::McpManager::disabled(),
        provider_connections: ProviderConnectionRegistry::new(),
        session_titles: SessionTitleScheduler::disabled(),
        runtime_hosts,
        prompt_root: state_dir.clone(),
        config_root: state_dir.clone(),
        daemon_config: crate::config::DaemonConfig::default(),
        pause_subagent_control_after_commit: Arc::new(AtomicBool::new(false)),
        subagent_control_committed: Arc::new(tokio::sync::Notify::new()),
        fail_subagent_control_reload_after_commit: Arc::new(AtomicBool::new(false)),
    };
    Some((state, admin_url, database_url, database_name, state_dir))
}

async fn cleanup_database(admin_url: &str, database_name: &str) {
    if let Ok(admin) = sqlx::PgPool::connect(admin_url).await {
        let _ = sqlx::query(&format!(r#"drop database if exists "{database_name}""#))
            .execute(&admin)
            .await;
        admin.close().await;
    }
}

fn database_url_with_name(base: &str, name: &str) -> String {
    let (prefix, query) = base
        .split_once('?')
        .map(|(prefix, query)| (prefix, format!("?{query}")))
        .unwrap_or((base, String::new()));
    let (root, _) = prefix.rsplit_once('/').expect("database URL path");
    format!("{root}/{name}{query}")
}

struct OAuthMcpServer {
    origin: String,
    task: tokio::task::JoinHandle<()>,
}

impl OAuthMcpServer {
    async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind OAuth MCP fixture");
        let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
        let task_origin = origin.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let origin = task_origin.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut stream).await else {
                        return;
                    };
                    let authenticated = request
                        .headers
                        .get("authorization")
                        .is_some_and(|value| value == "Bearer access-token");
                    let (status, headers, body) = match request.target.as_str() {
                        "/mcp" if authenticated => mcp_response(&request),
                        "/mcp" => (
                            401,
                            format!(
                                "WWW-Authenticate: Bearer resource_metadata=\"{origin}/protected\"\r\n"
                            ),
                            String::new(),
                        ),
                        "/protected" => (
                            200,
                            String::new(),
                            json!({
                                "resource": format!("{origin}/mcp"),
                                "authorization_servers": [origin.clone()],
                                "scopes_supported": ["read"],
                            })
                            .to_string(),
                        ),
                        "/.well-known/oauth-authorization-server" => (
                            200,
                            String::new(),
                            json!({
                                "issuer": origin,
                                "authorization_endpoint": format!("{origin}/authorize"),
                                "token_endpoint": format!("{origin}/token"),
                                "response_types_supported": ["code"],
                                "code_challenge_methods_supported": ["S256"],
                                "scopes_supported": ["read"],
                            })
                            .to_string(),
                        ),
                        "/token" => (
                            200,
                            String::new(),
                            json!({
                                "access_token": "access-token",
                                "refresh_token": "refresh-token",
                                "token_type": "Bearer",
                                "expires_in": 3600,
                                "scope": "read",
                            })
                            .to_string(),
                        ),
                        target => panic!("unexpected OAuth MCP fixture request {target}"),
                    };
                    let response = format!(
                        "HTTP/1.1 {status} Status\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream
                        .write_all(response.as_bytes())
                        .await
                        .expect("write OAuth MCP response");
                });
            }
        });
        Self { origin, task }
    }
}

impl Drop for OAuthMcpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
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
        return Some(Request {
            method: request_line.next()?.to_string(),
            target: request_line.next()?.to_string(),
            headers: head
                .lines()
                .skip(1)
                .filter_map(|line| {
                    line.split_once(':')
                        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
                })
                .collect(),
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
    let payload: Value = serde_json::from_slice(&request.body).expect("MCP request JSON");
    if payload["method"] == "notifications/initialized" {
        return (202, String::new(), String::new());
    }
    let result = match payload["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "oauth-fixture", "version": "1"},
        }),
        Some("tools/list") => json!({
            "tools": [{
                "name": "echo",
                "description": "Echo a value",
                "inputSchema": {"type": "object"}
            }]
        }),
        method => panic!("unexpected MCP fixture method {method:?}"),
    };
    let headers = if payload["method"] == "initialize" {
        "Mcp-Session-Id: oauth-session\r\n"
    } else {
        ""
    };
    (
        200,
        headers.to_string(),
        json!({ "jsonrpc": "2.0", "id": payload["id"], "result": result }).to_string(),
    )
}
