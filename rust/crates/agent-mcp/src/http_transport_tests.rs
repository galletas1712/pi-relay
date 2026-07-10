use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use agent_tools::{ProviderTool, ToolRegistry};
use agent_vocab::ProviderKind;
use pretty_assertions::assert_eq;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::*;

const MAX_TEST_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy)]
enum FakeMode {
    StatelessJson,
    StatefulSseStaleCall,
    StatefulSseSecrets,
    StatefulTimeoutCall,
    StatefulListChanged,
    OversizedInitialize,
    OversizedList,
    OversizedCall,
    OversizedError,
    OversizedSse,
    UnterminatedSseLine,
    UnterminatedSseEvent,
    StalledHeader,
    StalledBody,
    ReconnectExhaustion,
    InitialCommonGetUnavailable,
    InitialCommonGetUnsupported,
}

#[test]
fn bearer_scalar_representations_are_scrubbed_before_json_rpc_deserialization() {
    for secret in ["123456", "true", "null"] {
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":"id","error":{{"code":-32603,"message":"safe","data":{secret}}}}}"#
        );
        let scrubbed = SecretScrubber::new(secret.to_string()).scrub_json_text(&input);

        assert!(!scrubbed.contains(secret), "{secret}");
        serde_json::from_str::<ServerJsonRpcMessage>(&scrubbed)
            .expect("scrubbed JSON remains typed JSON-RPC");
    }

    let safe = r#"{"value":1234560,"enabled":false,"optional":"nullish"}"#;
    let scrubbed = SecretScrubber::new("123456".to_string()).scrub_json_text(safe);
    assert_eq!(
        serde_json::from_str::<Value>(&scrubbed).expect("safe JSON remains valid"),
        serde_json::from_str::<Value>(safe).expect("safe fixture is valid")
    );
}

#[test]
fn bearer_scalar_representations_are_scrubbed_from_sse_before_rmcp() {
    for secret in ["123456", "true", "null"] {
        let notifications = Arc::new(ClientNotifications::default());
        let mut parser = BoundedSseParser::new(
            Some(SecretScrubber::new(secret.to_string())),
            notifications,
            SseDispatch::Response,
        );
        let mut pending = VecDeque::new();
        let event = format!(
            "data: {{\"jsonrpc\":\"2.0\",\"id\":\"id\",\"error\":{{\"code\":-32603,\"message\":\"safe\",\"data\":{secret}}}}}\n\n"
        );
        parser
            .push(event.as_bytes(), &mut pending)
            .expect("SSE event parses");
        let data = pending
            .pop_front()
            .expect("SSE event emitted")
            .data
            .expect("SSE data is present");

        assert!(!data.contains(secret), "{secret}");
        serde_json::from_str::<ServerJsonRpcMessage>(&data)
            .expect("scrubbed SSE remains typed JSON-RPC");
    }

    let notifications = Arc::new(ClientNotifications::default());
    let mut parser = BoundedSseParser::new(
        Some(SecretScrubber::new("1".to_string())),
        notifications,
        SseDispatch::Common,
    );
    assert!(matches!(
        parser.push(b"retry: 1\n\n", &mut VecDeque::new()),
        Err(BoundedHttpError::InvalidJson)
    ));
}

#[test]
fn content_type_parser_accepts_only_exact_media_type_essences() {
    for (value, expected) in [
        ("application/json", Some(ContentType::Json)),
        ("Application/JSON; Charset=utf-8", Some(ContentType::Json)),
        (
            " text/event-stream ; charset=\"utf-8\" ",
            Some(ContentType::Sse),
        ),
        ("TEXT/EVENT-STREAM", Some(ContentType::Sse)),
        ("application/jsonp", None),
        ("text/event-stream-malicious", None),
        ("application/json;", None),
        ("application/json; charset", None),
        ("application/json; =utf-8", None),
        ("application/json; charset=\"unterminated", None),
        ("application/json, text/event-stream", None),
        ("text/plain", None),
    ] {
        assert_eq!(parse_content_type(value.as_bytes()), expected, "{value}");
    }
}

#[test]
fn sse_revision_hook_matches_rmcp_dispatch_and_advances_once() {
    let valid = concat!(
        "data: {\"jsonrpc\":\"2.0\",",
        "\"method\":\"notifications/tools/list_changed\"}\n\n"
    );
    let notifications = Arc::new(ClientNotifications::default());
    notifications.set_accepts_tools_changed(true);
    let mut parser = BoundedSseParser::new(None, notifications.clone(), SseDispatch::Common);
    let mut pending = VecDeque::new();
    parser
        .push(valid.as_bytes(), &mut pending)
        .expect("valid notification parses");
    assert_eq!(notifications.tools_revision(), 1);
    assert!(notifications.tools_uncertain());

    for (dispatch, event) in [
        (SseDispatch::Common, "data: {not-json}\n\n"),
        (
            SseDispatch::Common,
            concat!(
                "event: ping\n",
                "data: {\"jsonrpc\":\"2.0\",",
                "\"method\":\"notifications/tools/list_changed\"}\n\n"
            ),
        ),
        (SseDispatch::Initialize, valid),
        (SseDispatch::Discard, valid),
        (
            SseDispatch::Common,
            "data: {\"method\":\"notifications/tools/list_changed\"}\n\n",
        ),
        (
            SseDispatch::Common,
            concat!(
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,",
                "\"method\":\"notifications/tools/list_changed\"}\n\n"
            ),
        ),
    ] {
        let notifications = Arc::new(ClientNotifications::default());
        notifications.set_accepts_tools_changed(true);
        let mut parser = BoundedSseParser::new(None, notifications.clone(), dispatch);
        let mut pending = VecDeque::new();
        parser
            .push(event.as_bytes(), &mut pending)
            .expect("ignored event remains a valid SSE frame");
        assert_eq!(notifications.tools_revision(), 0, "{event}");
    }

    let notifications = Arc::new(ClientNotifications::default());
    notifications.set_accepts_tools_changed(true);
    let mut parser = BoundedSseParser::new(None, notifications.clone(), SseDispatch::Response);
    let mut pending = VecDeque::new();
    parser
        .push(
            concat!(
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n",
                "data: {\"jsonrpc\":\"2.0\",",
                "\"method\":\"notifications/tools/list_changed\"}\n\n"
            )
            .as_bytes(),
            &mut pending,
        )
        .expect("response stream parses");
    assert_eq!(notifications.tools_revision(), 0);
}

#[test]
fn unsolicited_sse_list_changed_is_ignored_without_negotiated_capability() {
    let notifications = Arc::new(ClientNotifications::default());
    let mut parser = BoundedSseParser::new(None, notifications.clone(), SseDispatch::Common);
    let mut pending = VecDeque::new();
    parser
        .push(
            b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
            &mut pending,
        )
        .expect("unsolicited notification parses");

    assert_eq!(notifications.tools_revision(), 0);
    assert!(!notifications.tools_uncertain());
}

#[test]
fn negotiated_dynamic_capability_honors_earlier_terminal_common_stream() {
    let notifications = ClientNotifications::default();
    let liveness = ClientLiveness::default();
    let control = HttpRequestControl {
        requests: Arc::new(RequestRegistry::default()),
        stream_attempts: Arc::new(Mutex::new(CommonStreams {
            sessions: HashMap::from([(
                "session".to_string(),
                CommonStreamState {
                    attempts: 1,
                    established: false,
                    terminal: true,
                },
            )]),
            ..CommonStreams::default()
        })),
        before_apply_negotiated_capabilities: None,
    };

    notifications.set_accepts_tools_changed(true);
    control.apply_negotiated_capabilities(&notifications, &liveness);
    control.apply_negotiated_capabilities(&notifications, &liveness);

    assert_eq!(notifications.tools_revision(), 1);
    assert!(notifications.tools_uncertain());
    assert!(liveness.is_closed());
}

#[test]
fn terminal_capability_fence_is_exactly_once_during_concurrent_transition() {
    let terminal_reached = Arc::new(Barrier::new(2));
    let apply_reached = Arc::new(Barrier::new(2));
    let release_transition = Arc::new(Barrier::new(3));
    let terminal_hook = {
        let terminal_reached = terminal_reached.clone();
        let release_transition = release_transition.clone();
        Arc::new(move || {
            terminal_reached.wait();
            release_transition.wait();
        })
    };
    let apply_hook = {
        let apply_reached = apply_reached.clone();
        let release_transition = release_transition.clone();
        Arc::new(move || {
            apply_reached.wait();
            release_transition.wait();
        })
    };
    let stream_attempts = Arc::new(Mutex::new(CommonStreams {
        terminal_transition_hook: Some(terminal_hook),
        ..CommonStreams::default()
    }));
    let notifications = Arc::new(ClientNotifications::default());
    notifications.set_accepts_tools_changed(true);
    let liveness = Arc::new(ClientLiveness::default());
    let client = BoundedHttpClient {
        client: build_reqwest_client(reqwest::Client::builder()).expect("HTTP client builds"),
        scrubber: None,
        requests: Arc::new(RequestRegistry::default()),
        stream_attempts: stream_attempts.clone(),
        notifications: notifications.clone(),
        liveness: liveness.clone(),
    };
    let control = HttpRequestControl {
        requests: Arc::new(RequestRegistry::default()),
        stream_attempts,
        before_apply_negotiated_capabilities: Some(apply_hook),
    };

    std::thread::scope(|scope| {
        scope.spawn(|| client.mark_common_stream_terminal("session"));
        terminal_reached.wait();
        scope.spawn(|| {
            control.apply_negotiated_capabilities(&notifications, &liveness);
        });
        apply_reached.wait();
        release_transition.wait();
    });

    client
        .stream_attempts
        .lock()
        .expect("HTTP stream attempt lock")
        .terminal_transition_hook = None;
    client.mark_common_stream_terminal("session");
    client.mark_common_stream_terminal("later-session");
    let control = HttpRequestControl {
        before_apply_negotiated_capabilities: None,
        ..control
    };
    control.apply_negotiated_capabilities(&notifications, &liveness);

    assert_eq!(notifications.tools_revision(), 1);
    assert!(notifications.tools_uncertain());
    assert!(liveness.is_closed());
}

#[test]
fn terminal_common_stream_does_not_fence_static_tools() {
    let stream_attempts = Arc::new(Mutex::new(CommonStreams::default()));
    let notifications = Arc::new(ClientNotifications::default());
    let liveness = Arc::new(ClientLiveness::default());
    let client = BoundedHttpClient {
        client: build_reqwest_client(reqwest::Client::builder()).expect("HTTP client builds"),
        scrubber: None,
        requests: Arc::new(RequestRegistry::default()),
        stream_attempts: stream_attempts.clone(),
        notifications: notifications.clone(),
        liveness: liveness.clone(),
    };
    let control = HttpRequestControl {
        requests: Arc::new(RequestRegistry::default()),
        stream_attempts,
        before_apply_negotiated_capabilities: None,
    };

    client.mark_common_stream_terminal("capability-absent");
    notifications.set_accepts_tools_changed(false);
    client.mark_common_stream_terminal("capability-false");
    client.mark_common_stream_terminal("capability-false");
    control.apply_negotiated_capabilities(&notifications, &liveness);

    assert_eq!(notifications.tools_revision(), 0);
    assert!(!notifications.tools_uncertain());
    assert!(!liveness.is_closed());
}

#[tokio::test]
async fn http_client_builder_clears_injected_proxies_and_routes_directly() {
    let server = FakeHttpServer::spawn(FakeMode::StatelessJson, None).await;
    let proxy = reqwest::Proxy::all("http://127.0.0.1:9").expect("test proxy URL parses");
    let client = build_reqwest_client(reqwest::Client::builder().proxy(proxy))
        .expect("direct-only client builds");

    let response = client
        .get(&server.url)
        .send()
        .await
        .expect("origin is reached without the unusable injected proxy");
    assert_eq!(response.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
    assert!(!format!("{client:?}").contains("proxies"));
}

async fn wait_for_method(server: &FakeHttpServer, method: &str) {
    for _ in 0..200 {
        if method_count(&server.requests(), method) > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("expected HTTP method observation was not recorded");
}

#[derive(Debug)]
struct RequestRecord {
    method: String,
    authorized: bool,
    session_id: Option<String>,
}

struct FakeHttpServer {
    url: String,
    requests: Arc<Mutex<Vec<RequestRecord>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeHttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeHttpServer {
    async fn spawn(mode: FakeMode, expected_bearer: Option<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake HTTP server");
        let address = listener.local_addr().expect("fake server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task_requests = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let requests = task_requests.clone();
                let expected_bearer = expected_bearer.clone();
                tokio::spawn(async move {
                    handle_request(stream, mode, expected_bearer.as_deref(), &requests).await;
                });
            }
        });
        Self {
            url: format!("http://{address}/mcp"),
            requests,
            task,
        }
    }

    fn requests(&self) -> Vec<(String, bool, Option<String>)> {
        self.requests
            .lock()
            .expect("request record lock")
            .iter()
            .map(|request| {
                (
                    request.method.clone(),
                    request.authorized,
                    request.session_id.clone(),
                )
            })
            .collect()
    }
}

#[tokio::test]
async fn stateless_json_remote_discovers_calls_authenticates_and_ignores_instructions() {
    let bearer = std::env::var("PATH").expect("PATH is set for test processes");
    let server = FakeHttpServer::spawn(FakeMode::StatelessJson, Some(bearer.clone())).await;
    let manager = McpManager::start(remote_config(&server.url, Some("PATH"), 1_000))
        .await
        .expect("remote manager starts");
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("remote inventory loads");
    assert_eq!(
        inventory.servers[0]
            .tools
            .iter()
            .map(|tool| tool.raw_name.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "echo"]
    );
    let snapshot = select_all(&manager, &inventory).await;
    let persisted = serde_json::to_string(snapshot.manifest()).expect("manifest serializes");
    assert!(!persisted.contains("REMOTE_INSTRUCTIONS_MUST_NOT_APPEAR"));
    assert!(!persisted.contains(&bearer));

    let echo = snapshot
        .manifest()
        .tools
        .iter()
        .find(|tool| tool.raw_name == "echo")
        .expect("echo selected");
    assert_eq!(
        manager
            .call(&snapshot, &echo.exposed_name, json!({"value": "hello"}))
            .await
            .expect("remote call succeeds"),
        McpCallOutput {
            output: "hello <redacted>".to_string(),
            is_error: false,
        }
    );
    let error = manager
        .call(
            &snapshot,
            &echo.exposed_name,
            json!({"value": "echo-secret"}),
        )
        .await
        .expect_err("server error is surfaced");
    assert!(!error.to_string().contains(&bearer));
    manager.shutdown().await;

    let requests = server.requests();
    assert!(requests.iter().all(|(_, authorized, _)| *authorized));
    assert!(requests.iter().all(|(_, _, session)| session.is_none()));
    assert_eq!(method_count(&requests, "initialize"), 1);
    assert_eq!(method_count(&requests, "notifications/initialized"), 1);
    assert_eq!(method_count(&requests, "tools/call"), 2);
}

#[tokio::test]
async fn stateful_sse_stale_session_fails_without_reinitializing_or_replaying() {
    let server = FakeHttpServer::spawn(FakeMode::StatefulSseStaleCall, None).await;
    let manager = McpManager::start(remote_config(&server.url, None, 1_000))
        .await
        .expect("stateful remote manager starts");
    let snapshot = {
        let inventory = manager
            .inventory(ProviderKind::OpenAi, &first_party())
            .await
            .expect("SSE inventory loads");
        select_all(&manager, &inventory).await
    };
    let tool = &snapshot.manifest().tools[0];
    let error = manager
        .call(&snapshot, &tool.exposed_name, json!({"value": "once"}))
        .await
        .expect_err("stale session fails closed");
    assert!(matches!(error, McpCallError::Protocol { .. }));
    tokio::time::sleep(Duration::from_millis(100)).await;
    manager.shutdown().await;

    let requests = server.requests();
    assert_eq!(method_count(&requests, "initialize"), 1);
    assert_eq!(method_count(&requests, "tools/call"), 1);
    assert!(requests
        .iter()
        .filter(|(method, _, _)| method != "initialize" && method != "GET")
        .all(|(_, _, session)| session.as_deref() == Some("test-session")));
}

#[tokio::test]
async fn timed_out_http_call_is_cancelled_and_never_replayed() {
    let server = FakeHttpServer::spawn(FakeMode::StatefulTimeoutCall, None).await;
    let manager = McpManager::start(remote_config(&server.url, None, 50))
        .await
        .expect("timeout remote manager starts");
    let snapshot = {
        let inventory = manager
            .inventory(ProviderKind::OpenAi, &first_party())
            .await
            .expect("remote inventory loads");
        select_all(&manager, &inventory).await
    };
    let tool = &snapshot.manifest().tools[0];
    let started = tokio::time::Instant::now();
    assert!(matches!(
        manager
            .call(&snapshot, &tool.exposed_name, json!({"value": "once"}))
            .await,
        Err(McpCallError::Timeout { .. })
    ));
    assert!(started.elapsed() < Duration::from_millis(500));
    wait_for_method(&server, "notifications/cancelled").await;
    wait_for_method(&server, "DELETE").await;
    let requests = server.requests();
    assert_eq!(method_count(&requests, "tools/call"), 1);
    assert_eq!(method_count(&requests, "notifications/cancelled"), 1);
    assert_eq!(method_count(&requests, "DELETE"), 1);
    assert!(requests
        .iter()
        .filter(|(method, _, _)| matches!(method.as_str(), "notifications/cancelled" | "DELETE"))
        .all(|(_, _, session)| session.as_deref() == Some("test-session")));
    manager.shutdown().await;
}

#[tokio::test]
async fn cancelled_http_call_uses_live_control_path_and_is_never_replayed() {
    let server = FakeHttpServer::spawn(FakeMode::StatefulTimeoutCall, None).await;
    let manager = McpManager::start(remote_config(&server.url, None, 5_000))
        .await
        .expect("remote manager starts");
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("remote inventory loads");
    let snapshot = select_all(&manager, &inventory).await;
    let tool = snapshot
        .manifest()
        .tools
        .iter()
        .find(|tool| tool.raw_name == "echo")
        .expect("echo tool selected")
        .exposed_name
        .clone();
    let task = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .call(&snapshot, &tool, json!({"value": "once"}))
                .await
        })
    };
    wait_for_method(&server, "tools/call").await;
    task.abort();
    assert!(task
        .await
        .expect_err("call task is cancelled")
        .is_cancelled());
    wait_for_method(&server, "notifications/cancelled").await;
    wait_for_method(&server, "DELETE").await;
    let requests = server.requests();
    assert_eq!(method_count(&requests, "tools/call"), 1);
    assert_eq!(method_count(&requests, "notifications/cancelled"), 1);
    assert_eq!(method_count(&requests, "DELETE"), 1);
    manager.shutdown().await;
}

#[tokio::test]
async fn stateful_shutdown_sends_delete_without_hanging() {
    let server = FakeHttpServer::spawn(FakeMode::StatefulSseStaleCall, None).await;
    let manager = McpManager::start(remote_config(&server.url, None, 1_000))
        .await
        .expect("stateful manager starts");
    tokio::time::timeout(Duration::from_secs(1), manager.shutdown())
        .await
        .expect("shutdown is bounded");
    wait_for_method(&server, "DELETE").await;
    assert_eq!(method_count(&server.requests(), "DELETE"), 1);
}

#[tokio::test]
async fn inbound_secret_is_scrubbed_from_json_sse_errors_catalog_and_output() {
    let bearer = std::env::var("PATH").expect("PATH is set for test processes");
    for mode in [FakeMode::StatelessJson, FakeMode::StatefulSseSecrets] {
        let server = FakeHttpServer::spawn(mode, Some(bearer.clone())).await;
        let manager = McpManager::start(remote_config(&server.url, Some("PATH"), 1_000))
            .await
            .expect("secret-reflecting manager starts");
        let inventory = manager
            .inventory(ProviderKind::OpenAi, &first_party())
            .await
            .expect("secret-reflecting inventory loads");
        let snapshot = select_all(&manager, &inventory).await;
        let inventory_json = serde_json::to_string(&inventory).expect("inventory serializes");
        let manifest_json =
            serde_json::to_string(snapshot.manifest()).expect("manifest serializes");
        assert!(!inventory_json.contains(&bearer));
        assert!(!manifest_json.contains(&bearer));
        assert!(!format!("{inventory:?}").contains(&bearer));
        assert!(!format!("{:?}", snapshot.manifest()).contains(&bearer));
        let tool = snapshot
            .manifest()
            .tools
            .iter()
            .find(|tool| tool.raw_name == "echo")
            .expect("echo tool selected")
            .exposed_name
            .clone();
        let output = manager
            .call(&snapshot, &tool, json!({"value": "reflected"}))
            .await
            .expect("reflected result succeeds");
        assert!(!output.output.contains(&bearer));
        if matches!(mode, FakeMode::StatelessJson) {
            let error = manager
                .call(&snapshot, &tool, json!({"value": "echo-secret"}))
                .await
                .expect_err("reflected JSON-RPC error is surfaced");
            assert!(!format!("{error:?}").contains(&bearer));
            assert!(!error.to_string().contains(&bearer));
            let transport_error = manager
                .call(&snapshot, &tool, json!({"value": "transport-secret"}))
                .await
                .expect_err("reflected transport error is surfaced");
            assert!(!format!("{transport_error:?}").contains(&bearer));
            assert!(!transport_error.to_string().contains(&bearer));
        }
        manager.shutdown().await;
    }
    assert!(!format!("{:?}", SecretScrubber::new(bearer.clone())).contains(&bearer));
    assert!(!format!("{:?}", BoundedHttpError::Request).contains(&bearer));
}

#[tokio::test]
async fn hard_bounds_reject_chunked_json_error_and_sse_before_catalog_or_output() {
    for mode in [
        FakeMode::OversizedInitialize,
        FakeMode::OversizedList,
        FakeMode::OversizedCall,
        FakeMode::OversizedError,
        FakeMode::OversizedSse,
        FakeMode::UnterminatedSseLine,
        FakeMode::UnterminatedSseEvent,
        FakeMode::StalledHeader,
        FakeMode::StalledBody,
    ] {
        let server = FakeHttpServer::spawn(mode, None).await;
        let manager = tokio::time::timeout(
            Duration::from_secs(2),
            McpManager::start(remote_config(&server.url, None, 1_000)),
        )
        .await
        .expect("adversarial server handling is bounded")
        .expect("manager contains unavailable adversarial route");
        let inventory = manager
            .inventory(ProviderKind::OpenAi, &first_party())
            .await
            .expect("unavailable inventory remains bounded");
        if matches!(
            mode,
            FakeMode::OversizedCall
                | FakeMode::OversizedError
                | FakeMode::OversizedSse
                | FakeMode::UnterminatedSseLine
                | FakeMode::UnterminatedSseEvent
                | FakeMode::StalledBody
        ) && inventory.servers[0].health == McpHealth::Healthy
        {
            let snapshot = select_all(&manager, &inventory).await;
            let tool = snapshot.manifest().tools[0].exposed_name.clone();
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                manager.call(&snapshot, &tool, json!({"value": "adversarial"})),
            )
            .await
            .expect("adversarial call is bounded");
            assert!(result.is_err());
        } else {
            assert_eq!(inventory.servers[0].health, McpHealth::Unavailable);
        }
        manager.shutdown().await;
    }
}

#[tokio::test]
async fn common_sse_reconnect_policy_is_bounded() {
    let server = FakeHttpServer::spawn(FakeMode::ReconnectExhaustion, None).await;
    let manager = McpManager::start(remote_config(&server.url, None, 1_000))
        .await
        .expect("stateful manager starts");
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("initial inventory loads");
    let snapshot = select_all(&manager, &inventory).await;
    let tool = snapshot.manifest().tools[0].exposed_name.clone();
    manager
        .call(&snapshot, &tool, json!({"value": "start-stream-failure"}))
        .await
        .expect("call before the event stream failure succeeds");
    for _ in 0..100 {
        if method_count(&server.requests(), "GET") > SSE_RECONNECT_LIMIT {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        method_count(&server.requests(), "GET"),
        SSE_RECONNECT_LIMIT + 1
    );
    let unavailable = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("terminal stream failure retains a coherent unavailable inventory");
    assert_eq!(unavailable.servers[0].health, McpHealth::Unavailable);
    assert!(matches!(
        manager
            .select(
                &McpSessionSelection {
                    inventory_revision: unavailable.revision,
                    servers: vec![McpServerSelection {
                        server: "remote".to_string(),
                        tools: vec!["alpha".to_string()],
                    }],
                },
                &first_party(),
            )
            .await,
        Err(McpManagerError::Unavailable { server }) if server == "remote"
    ));
    let result = manager
        .call(&snapshot, &tool, json!({"value": "stale"}))
        .await;
    assert!(
        matches!(
            result,
            Err(McpCallError::ServerUnavailable { .. } | McpCallError::Protocol { .. })
        ),
        "stale call must fail after terminal event stream loss: {result:?}"
    );
    assert_eq!(method_count(&server.requests(), "tools/call"), 1);
    manager.shutdown().await;
}

#[tokio::test]
async fn initial_common_get_failure_is_optional_only_for_static_catalogs() {
    for mode in [
        FakeMode::InitialCommonGetUnavailable,
        FakeMode::InitialCommonGetUnsupported,
    ] {
        let server = FakeHttpServer::spawn(mode, None).await;
        let static_manager = McpManager::start(remote_config(&server.url, None, 1_000))
            .await
            .expect("static manager starts without an optional common stream");
        let static_inventory = static_manager
            .inventory(ProviderKind::OpenAi, &first_party())
            .await
            .expect("static inventory remains usable");
        let static_snapshot = select_all(&static_manager, &static_inventory).await;
        let tool = static_snapshot.manifest().tools[0].exposed_name.clone();
        static_manager
            .call(&static_snapshot, &tool, json!({"value": "static"}))
            .await
            .expect("static call remains usable");
        static_manager.shutdown().await;
        let calls_before_dynamic_start = method_count(&server.requests(), "tools/call");
        let lists_before_dynamic_start = method_count(&server.requests(), "tools/list");
        assert_eq!(calls_before_dynamic_start, 1);

        let dynamic_manager = McpManager::start(remote_config(&server.url, None, 1_000))
            .await
            .expect("manager retains the unavailable dynamic route");
        assert!(
            method_count(&server.requests(), "tools/list") > lists_before_dynamic_start,
            "dynamic startup reaches the responsive POST endpoint before closing"
        );
        let unavailable = dynamic_manager
            .inventory(ProviderKind::OpenAi, &first_party())
            .await
            .expect("unavailable dynamic inventory remains coherent");
        assert_eq!(unavailable.servers[0].health, McpHealth::Unavailable);
        assert!(unavailable.servers[0].tools.is_empty());
        assert!(matches!(
            dynamic_manager
                .select(
                    &McpSessionSelection {
                        inventory_revision: unavailable.revision,
                        servers: vec![McpServerSelection {
                            server: "remote".to_string(),
                            tools: vec!["alpha".to_string()],
                        }],
                    },
                    &first_party(),
                )
                .await,
            Err(McpManagerError::Unavailable { server }) if server == "remote"
        ));
        assert!(matches!(
            dynamic_manager
                .call(&static_snapshot, &tool, json!({"value": "dynamic"}))
                .await,
            Err(McpCallError::ServerUnavailable { server }) if server == "remote"
        ));
        assert_eq!(
            method_count(&server.requests(), "tools/call"),
            calls_before_dynamic_start
        );
        dynamic_manager.shutdown().await;
    }
}

#[tokio::test]
async fn http_list_changed_fences_selection_and_frozen_contract() {
    let server = FakeHttpServer::spawn(FakeMode::StatefulListChanged, None).await;
    let manager = McpManager::start(remote_config(&server.url, None, 1_000))
        .await
        .expect("list-changed manager starts");
    let before = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("initial inventory loads");
    let snapshot = select_all(&manager, &before).await;
    let frozen = snapshot.manifest().clone();
    let tool = snapshot
        .manifest()
        .tools
        .iter()
        .find(|tool| tool.raw_name == "echo")
        .expect("echo tool selected")
        .exposed_name
        .clone();
    manager
        .call(&snapshot, &tool, json!({"value": "notify"}))
        .await
        .expect("notification-racing call was already admitted");
    wait_for_method(&server, "notification-sent").await;

    let after = loop {
        let inventory = manager
            .inventory(ProviderKind::OpenAi, &first_party())
            .await
            .expect("changed inventory refreshes");
        if inventory.revision != before.revision {
            break inventory;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert!(matches!(
        manager
            .select(
                &McpSessionSelection {
                    inventory_revision: before.revision.clone(),
                    servers: vec![]
                },
                &first_party()
            )
            .await,
        Err(McpManagerError::InventoryChanged { .. })
    ));
    assert_ne!(after.revision, before.revision);
    assert_ne!(after.servers[0].revision, before.servers[0].revision);
    assert_eq!(snapshot.manifest(), &frozen);
    let later = manager
        .call(&snapshot, &tool, json!({"value": "stale"}))
        .await;
    assert!(
        matches!(
            later,
            Err(McpCallError::ContractChanged { .. } | McpCallError::Revoked { .. })
        ),
        "later call must fail closed after the catalog changed: {later:?}"
    );
    assert_eq!(method_count(&server.requests(), "tools/call"), 1);
    manager.shutdown().await;
}

#[test]
fn sse_parser_enforces_line_event_data_event_count_and_termination_limits() {
    fn parser() -> BoundedSseParser {
        BoundedSseParser::new(
            None,
            Arc::new(ClientNotifications::default()),
            SseDispatch::Discard,
        )
    }

    let mut pending = VecDeque::new();
    let mut line = parser();
    assert!(matches!(
        line.push(&vec![b'x'; SSE_LINE_LIMIT + 1], &mut pending),
        Err(BoundedHttpError::BodyTooLarge)
    ));

    let mut event = parser();
    let event_bytes = format!(
        "event: {}\nid: {}\n",
        "e".repeat(SSE_FIELD_LIMIT),
        "i".repeat(SSE_FIELD_LIMIT)
    );
    for _ in 0..(SSE_EVENT_LIMIT / event_bytes.len() + 1) {
        if event.push(event_bytes.as_bytes(), &mut pending).is_err() {
            return;
        }
    }
    panic!("SSE event byte limit was not enforced");
}

#[test]
fn sse_parser_enforces_data_count_and_unterminated_event_limits() {
    let notifications = Arc::new(ClientNotifications::default());
    let mut pending = VecDeque::new();
    let mut data = BoundedSseParser::new(None, notifications.clone(), SseDispatch::Discard);
    assert!(matches!(
        data.push(
            format!("data: {}\n", "x".repeat(SSE_DATA_LIMIT + 1)).as_bytes(),
            &mut pending
        ),
        Err(BoundedHttpError::BodyTooLarge)
    ));

    let mut events = BoundedSseParser::new(None, notifications.clone(), SseDispatch::Discard);
    let repeated = "data: {}\n\n".repeat(SSE_EVENTS_PER_RESPONSE_LIMIT + 1);
    assert!(matches!(
        events.push(repeated.as_bytes(), &mut pending),
        Err(BoundedHttpError::BodyTooLarge)
    ));

    let mut unterminated = BoundedSseParser::new(None, notifications, SseDispatch::Discard);
    unterminated
        .push(b"data: {}\n", &mut pending)
        .expect("bounded partial event parses");
    assert!(matches!(
        unterminated.finish(),
        Err(BoundedHttpError::InvalidJson)
    ));
}

fn remote_config(url: &str, bearer_token_env: Option<&str>, call_timeout_ms: u64) -> McpConfig {
    let auth = bearer_token_env
        .map(|env| json!({"type": "bearer_env", "env": env}))
        .unwrap_or(Value::Null);
    serde_json::from_value(json!({
        "servers": {
            "remote": {
                "transport": {
                    "type": "streamable_http",
                    "url": url,
                    "auth": auth
                },
                "allow_all_tools": true,
                "call_timeout_ms": call_timeout_ms
            }
        }
    }))
    .expect("remote config parses")
}

fn first_party() -> HashMap<ProviderKind, Vec<ProviderTool>> {
    let registry = ToolRegistry::with_builtin_tools();
    [ProviderKind::OpenAi, ProviderKind::Claude]
        .into_iter()
        .map(|provider| (provider, registry.provider_tools_for_provider(provider)))
        .collect()
}

async fn select_all(manager: &McpManager, inventory: &McpInventory) -> McpSessionSnapshot {
    manager
        .select(
            &McpSessionSelection {
                inventory_revision: inventory.revision.clone(),
                servers: inventory
                    .servers
                    .iter()
                    .map(|server| McpServerSelection {
                        server: server.server.clone(),
                        tools: server
                            .tools
                            .iter()
                            .map(|tool| tool.raw_name.clone())
                            .collect(),
                    })
                    .collect(),
            },
            &first_party(),
        )
        .await
        .expect("remote selection binds")
}

fn method_count(requests: &[(String, bool, Option<String>)], method: &str) -> usize {
    requests
        .iter()
        .filter(|(candidate, _, _)| candidate == method)
        .count()
}

async fn handle_request(
    mut stream: TcpStream,
    mode: FakeMode,
    expected_bearer: Option<&str>,
    requests: &Mutex<Vec<RequestRecord>>,
) {
    let Some((request_line, headers, body)) = read_request(&mut stream).await else {
        return;
    };
    if request_line.starts_with("GET ") {
        requests
            .lock()
            .expect("request record lock")
            .push(RequestRecord {
                method: "GET".to_string(),
                authorized: authorization_matches(&headers, expected_bearer),
                session_id: headers.get("mcp-session-id").cloned(),
            });
        if matches!(mode, FakeMode::ReconnectExhaustion) {
            for _ in 0..200 {
                if requests
                    .lock()
                    .expect("request record lock")
                    .iter()
                    .any(|request| request.method == "tools/call")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            write_response(&mut stream, "200 OK", Some("text/event-stream"), None, "").await;
        } else if matches!(mode, FakeMode::StatefulListChanged) {
            for _ in 0..200 {
                if requests
                    .lock()
                    .expect("request record lock")
                    .iter()
                    .any(|request| request.method == "tools/call")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            write_response(
                &mut stream,
                "200 OK",
                Some("text/event-stream"),
                None,
                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
            )
            .await;
            requests
                .lock()
                .expect("request record lock")
                .push(RequestRecord {
                    method: "notification-sent".to_string(),
                    authorized: true,
                    session_id: headers.get("mcp-session-id").cloned(),
                });
        } else if matches!(mode, FakeMode::InitialCommonGetUnavailable) {
            write_response(
                &mut stream,
                "503 Service Unavailable",
                Some("text/plain"),
                None,
                "",
            )
            .await;
        } else {
            write_response(&mut stream, "405 Method Not Allowed", None, None, "").await;
        }
        return;
    }
    if request_line.starts_with("DELETE ") {
        requests
            .lock()
            .expect("request record lock")
            .push(RequestRecord {
                method: "DELETE".to_string(),
                authorized: authorization_matches(&headers, expected_bearer),
                session_id: headers.get("mcp-session-id").cloned(),
            });
        write_response(&mut stream, "200 OK", None, None, "").await;
        return;
    }
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return,
    };
    let method = payload["method"].as_str().unwrap_or_default().to_string();
    let session_id = headers.get("mcp-session-id").cloned();
    requests
        .lock()
        .expect("request record lock")
        .push(RequestRecord {
            method: method.clone(),
            authorized: authorization_matches(&headers, expected_bearer),
            session_id: session_id.clone(),
        });
    if !authorization_matches(&headers, expected_bearer) {
        write_response(&mut stream, "401 Unauthorized", None, None, "").await;
        return;
    }
    if method == "initialize" && matches!(mode, FakeMode::StalledHeader) {
        tokio::time::sleep(Duration::from_secs(60)).await;
        return;
    }
    if method == "initialize"
        && matches!(mode, FakeMode::ReconnectExhaustion)
        && requests
            .lock()
            .expect("request record lock")
            .iter()
            .filter(|request| request.method == "GET")
            .count()
            > SSE_RECONNECT_LIMIT
    {
        write_response(
            &mut stream,
            "503 Service Unavailable",
            Some("text/plain"),
            None,
            "",
        )
        .await;
        return;
    }
    if method == "notifications/initialized" {
        write_response(&mut stream, "202 Accepted", None, None, "").await;
        return;
    }
    if method == "tools/list"
        && matches!(
            mode,
            FakeMode::InitialCommonGetUnavailable | FakeMode::InitialCommonGetUnsupported
        )
    {
        for _ in 0..200 {
            let common_gets = requests
                .lock()
                .expect("request record lock")
                .iter()
                .filter(|request| request.method == "GET")
                .count();
            let initializations = requests
                .lock()
                .expect("request record lock")
                .iter()
                .filter(|request| request.method == "initialize")
                .count();
            if common_gets >= initializations {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    if method == "tools/call" && matches!(mode, FakeMode::StatefulSseStaleCall) {
        write_response(&mut stream, "404 Not Found", None, None, "").await;
        return;
    }
    if method == "tools/call" && matches!(mode, FakeMode::StatefulTimeoutCall) {
        tokio::time::sleep(Duration::from_secs(60)).await;
        return;
    }
    if method == "tools/call" && matches!(mode, FakeMode::StalledBody) {
        write_stalled_body(&mut stream).await;
        return;
    }
    if method == "tools/call" && payload["params"]["arguments"]["value"] == "echo-secret" {
        let body = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": payload["id"],
            "error": {
                "code": -32603,
                "message": expected_bearer.unwrap_or_default(),
                "data": {"reflected": expected_bearer.unwrap_or_default()}
            }
        }))
        .expect("fake JSON-RPC error serializes");
        write_response(
            &mut stream,
            "500 Internal Server Error",
            Some("application/json"),
            None,
            &body,
        )
        .await;
        return;
    }
    if method == "tools/call" && payload["params"]["arguments"]["value"] == "transport-secret" {
        write_response(
            &mut stream,
            "500 Internal Server Error",
            Some("text/plain"),
            None,
            expected_bearer.unwrap_or_default(),
        )
        .await;
        return;
    }
    if method == "initialize" && matches!(mode, FakeMode::OversizedInitialize) {
        write_chunked_response(
            &mut stream,
            "200 OK",
            "application/json",
            &"x".repeat(JSON_BODY_LIMIT + 1),
        )
        .await;
        return;
    }
    if method == "tools/list" && matches!(mode, FakeMode::OversizedList) {
        write_chunked_response(
            &mut stream,
            "200 OK",
            "application/json",
            &"x".repeat(JSON_BODY_LIMIT + 1),
        )
        .await;
        return;
    }
    if method == "tools/call" && matches!(mode, FakeMode::OversizedCall) {
        write_chunked_response(
            &mut stream,
            "200 OK",
            "application/json",
            &"x".repeat(JSON_BODY_LIMIT + 1),
        )
        .await;
        return;
    }
    if method == "tools/call" && matches!(mode, FakeMode::OversizedError) {
        write_chunked_response(
            &mut stream,
            "500 Internal Server Error",
            "text/plain",
            &"x".repeat(ERROR_BODY_LIMIT + 1),
        )
        .await;
        return;
    }
    if method == "tools/call" && matches!(mode, FakeMode::OversizedSse) {
        write_chunked_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            &format!("data: {}\n\n", "x".repeat(SSE_DATA_LIMIT + 1)),
        )
        .await;
        return;
    }
    if method == "tools/call" && matches!(mode, FakeMode::UnterminatedSseLine) {
        write_response(
            &mut stream,
            "200 OK",
            Some("text/event-stream"),
            None,
            &format!("data: {}", "x".repeat(SSE_LINE_LIMIT + 1)),
        )
        .await;
        return;
    }
    if method == "tools/call" && matches!(mode, FakeMode::UnterminatedSseEvent) {
        write_response(
            &mut stream,
            "200 OK",
            Some("text/event-stream"),
            None,
            "data: {\"jsonrpc\":\"2.0\"}\n",
        )
        .await;
        return;
    }
    let id = payload["id"].clone();
    let reflected = expected_bearer.unwrap_or_default();
    let list_count = requests
        .lock()
        .expect("request record lock")
        .iter()
        .filter(|request| request.method == "tools/list")
        .count();
    let initialize_count = requests
        .lock()
        .expect("request record lock")
        .iter()
        .filter(|request| request.method == "initialize")
        .count();
    let result = match method.as_str() {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {
                "tools": {
                    "listChanged": matches!(
                        mode,
                        FakeMode::StatefulListChanged | FakeMode::ReconnectExhaustion
                    ) || (matches!(
                        mode,
                        FakeMode::InitialCommonGetUnavailable
                            | FakeMode::InitialCommonGetUnsupported
                    )
                        && initialize_count > 1)
                }
            },
            "serverInfo": {
                "name": format!("remote-fixture-{reflected}"),
                "version": reflected
            },
            "instructions": format!(
                "{}{reflected}",
                "REMOTE_INSTRUCTIONS_MUST_NOT_APPEAR".repeat(4_096)
            ),
        }),
        "tools/list" => json!({
            "tools": [
                {
                    "name": "echo",
                    "description": if matches!(mode, FakeMode::StatefulListChanged) && list_count > 1 {
                        "Changed contract".to_string()
                    } else {
                        format!("Echo a value {reflected}")
                    },
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "value": {
                                "type": "string",
                                "description": reflected
                            }
                        }
                    }
                },
                {
                    "name": "alpha",
                    "description": "First after deterministic catalog ordering",
                    "inputSchema": {"type": "object"}
                }
            ]
        }),
        "tools/call" => json!({
            "content": [{
                "type": "text",
                "text": format!(
                    "{} {reflected}",
                    payload["params"]["arguments"]["value"].as_str().unwrap_or_default()
                )
            }]
        }),
        _ => json!({}),
    };
    let response = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    }))
    .expect("fake response serializes");
    let stateful = matches!(
        mode,
        FakeMode::StatefulSseStaleCall
            | FakeMode::StatefulSseSecrets
            | FakeMode::StatefulTimeoutCall
            | FakeMode::StatefulListChanged
            | FakeMode::ReconnectExhaustion
            | FakeMode::InitialCommonGetUnavailable
            | FakeMode::InitialCommonGetUnsupported
    );
    let session = stateful.then_some("test-session");
    if matches!(
        mode,
        FakeMode::StatefulSseStaleCall | FakeMode::StatefulSseSecrets
    ) {
        write_response(
            &mut stream,
            "200 OK",
            Some("text/event-stream"),
            session,
            &format!("event: message\ndata: {response}\n\n"),
        )
        .await;
    } else {
        write_response(
            &mut stream,
            "200 OK",
            Some("application/json"),
            session,
            &response,
        )
        .await;
    }
}

fn authorization_matches(headers: &HashMap<String, String>, expected_bearer: Option<&str>) -> bool {
    match expected_bearer {
        Some(expected) => {
            headers.get("authorization").map(String::as_str) == Some(&format!("Bearer {expected}"))
        }
        None => !headers.contains_key("authorization"),
    }
}

async fn read_request(
    stream: &mut TcpStream,
) -> Option<(String, HashMap<String, String>, Vec<u8>)> {
    let mut bytes = Vec::new();
    let header_end = loop {
        if bytes.len() >= MAX_TEST_REQUEST_BYTES {
            return None;
        }
        let mut chunk = [0_u8; 8_192];
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = std::str::from_utf8(&bytes[..header_end]).ok()?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?.to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect::<HashMap<_, _>>();
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if header_end.saturating_add(content_length) > MAX_TEST_REQUEST_BYTES {
        return None;
    }
    while bytes.len() < header_end + content_length {
        let mut chunk = [0_u8; 8_192];
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Some((
        request_line,
        headers,
        bytes[header_end..header_end + content_length].to_vec(),
    ))
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: Option<&str>,
    session_id: Option<&str>,
    body: &str,
) {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(content_type) = content_type {
        response.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    if let Some(session_id) = session_id {
        response.push_str(&format!("Mcp-Session-Id: {session_id}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(body);
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn write_chunked_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) {
    let head = format!(
        "HTTP/1.1 {status}\r\nTransfer-Encoding: chunked\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for chunk in body.as_bytes().chunks(8_192) {
        let size = format!("{:x}\r\n", chunk.len());
        if stream.write_all(size.as_bytes()).await.is_err()
            || stream.write_all(chunk).await.is_err()
            || stream.write_all(b"\r\n").await.is_err()
        {
            return;
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n").await;
}

async fn write_stalled_body(stream: &mut TcpStream) {
    let response =
        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{";
    if stream.write_all(response.as_bytes()).await.is_ok() {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
