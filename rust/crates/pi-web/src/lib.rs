#![forbid(unsafe_code)]

mod assets;
mod git;
mod staging;

use std::path::PathBuf;
use std::sync::Arc;

use agent_store::{PostgresAgentStore, SessionGitConfig};
use async_trait::async_trait;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{header, uri::Authority, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::Semaphore;

pub use git::GitExecutables;

const DEFAULT_LIMIT: usize = 12;
const MAX_LIMIT: usize = 100;
const MAX_SESSION_ID_BYTES: usize = 256;

#[async_trait]
pub trait SessionGitStore: Send + Sync {
    async fn load_git_config(&self, session_id: &str) -> anyhow::Result<Option<SessionGitConfig>>;
}

async fn spa(
    axum::Extension(assets): axum::Extension<assets::StaticAssets>,
    request: Request,
) -> Response {
    if request.method() != axum::http::Method::GET && request.method() != axum::http::Method::HEAD {
        return method_not_allowed().await.into_response();
    }
    assets.index_response(request.method())
}

async fn static_asset(
    axum::Extension(assets): axum::Extension<assets::StaticAssets>,
    request: Request,
) -> Response {
    assets.response(request.uri(), request.method())
}

async fn api_not_found() -> ApiError {
    ApiError::not_found("api_not_found", "API endpoint not found")
}

async fn method_not_allowed() -> ApiError {
    ApiError::method_not_allowed("method_not_allowed", "HTTP method not allowed")
}

#[async_trait]
impl SessionGitStore for PostgresAgentStore {
    async fn load_git_config(&self, session_id: &str) -> anyhow::Result<Option<SessionGitConfig>> {
        self.load_session_git_config(session_id).await
    }
}

#[derive(Clone)]
struct AppState {
    store: Arc<dyn SessionGitStore>,
    git_inspections: Arc<Semaphore>,
    git_executables: GitExecutables,
}

#[derive(Clone)]
struct AllowedHosts(Arc<[String]>);

pub fn router(
    store: Arc<dyn SessionGitStore>,
    web_root: PathBuf,
    allowed_hosts: Vec<String>,
) -> anyhow::Result<Router> {
    router_with_executables(
        store,
        web_root,
        allowed_hosts,
        GitExecutables::resolve(None, None)?,
    )
}

pub fn router_with_executables(
    store: Arc<dyn SessionGitStore>,
    web_root: PathBuf,
    allowed_hosts: Vec<String>,
    git_executables: GitExecutables,
) -> anyhow::Result<Router> {
    let assets = assets::StaticAssets::stage(&web_root)?;
    Ok(Router::new()
        .route("/healthz", get(health))
        .route("/api", get(api_not_found).fallback(method_not_allowed))
        .route("/api/", get(api_not_found).fallback(method_not_allowed))
        .route(
            "/api/sessions/{session_id}/git",
            get(session_git).fallback(method_not_allowed),
        )
        .route(
            "/api/{*path}",
            get(api_not_found).fallback(method_not_allowed),
        )
        .route("/w", any(spa))
        .route("/w/", any(spa))
        .route("/w/{*path}", any(spa))
        .fallback(static_asset)
        .layer(middleware::from_fn_with_state(
            AllowedHosts(allowed_hosts.into()),
            validate_host,
        ))
        .with_state(AppState {
            store,
            git_inspections: Arc::new(Semaphore::new(git::MAX_CONCURRENT_INSPECTIONS)),
            git_executables,
        })
        .layer(axum::Extension(assets)))
}

async fn health() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        "ok\n",
    )
}

async fn session_git(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Result<impl IntoResponse, ApiError> {
    if session_id.is_empty()
        || session_id.len() > MAX_SESSION_ID_BYTES
        || session_id.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(
            "invalid_session_id",
            "session_id must be a non-empty identifier of at most 256 bytes",
        ));
    }
    let limit = parse_limit(raw_query.as_deref())?;
    let config = state
        .store
        .load_git_config(&session_id)
        .await
        .map_err(|_| {
            ApiError::internal(
                "store_unavailable",
                "session configuration is temporarily unavailable",
            )
        })?
        .ok_or_else(|| ApiError::not_found("session_not_found", "session not found"))?;
    let response = git::session_git_status(
        &session_id,
        config,
        limit,
        state.git_inspections,
        state.git_executables,
    )
    .await;
    Ok((
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(response),
    ))
}

fn parse_limit(raw_query: Option<&str>) -> Result<usize, ApiError> {
    let Some(raw_query) = raw_query else {
        return Ok(DEFAULT_LIMIT);
    };
    let Some(value) = raw_query.strip_prefix("limit=") else {
        return Err(invalid_limit());
    };
    if value.is_empty() || value.contains('&') || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_limit());
    }
    let limit = value.parse::<usize>().map_err(|_| invalid_limit())?;
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(invalid_limit());
    }
    Ok(limit)
}

fn invalid_limit() -> ApiError {
    ApiError::bad_request(
        "invalid_limit",
        "limit must be a single integer from 1 through 100",
    )
}

async fn validate_host(
    State(allowed): State<AllowedHosts>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(normalize_host_authority);
    if !host.is_some_and(|host| allowed.0.iter().any(|allowed| allowed == &host)) {
        return ApiError::bad_request("invalid_host", "request Host is not allowed")
            .into_response();
    }
    next.run(request).await
}

pub fn normalize_host_authority(host: &str) -> Option<String> {
    let host = host.trim();
    let authority = host.parse::<Authority>().ok()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let hostname = authority.host();
    let suffix = authority.as_str().strip_prefix(hostname)?;
    if !suffix.is_empty() && (authority.port().is_none() || !suffix.starts_with(':')) {
        return None;
    }
    let hostname = hostname
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(hostname)
        .trim_end_matches('.');
    if hostname.is_empty() || hostname.chars().any(char::is_control) {
        return None;
    }
    Some(hostname.to_ascii_lowercase())
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl ApiError {
    fn bad_request(code: &'static str, message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message,
        }
    }

    fn method_not_allowed(code: &'static str, message: &'static str) -> Self {
        Self {
            status: StatusCode::METHOD_NOT_ALLOWED,
            code,
            message,
        }
    }

    fn not_found(code: &'static str, message: &'static str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code,
            message,
        }
    }

    fn internal(code: &'static str, message: &'static str) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code,
            message,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: &'static str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
            Json(ErrorBody {
                error: ErrorDetail {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::convert::Infallible;

    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tempfile::TempDir;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    use super::*;

    struct FakeStore {
        sessions: Mutex<HashMap<String, SessionGitConfig>>,
        fail: bool,
    }

    #[async_trait]
    impl SessionGitStore for FakeStore {
        async fn load_git_config(
            &self,
            session_id: &str,
        ) -> anyhow::Result<Option<SessionGitConfig>> {
            if self.fail {
                anyhow::bail!("injected store failure");
            }
            Ok(self.sessions.lock().await.get(session_id).cloned())
        }
    }

    #[tokio::test]
    async fn endpoint_defaults_limit_and_reports_unknown_session() {
        let temp = TempDir::new().expect("web root");
        std::fs::write(temp.path().join("index.html"), "spa").expect("index");
        let store = Arc::new(FakeStore {
            sessions: Mutex::new(HashMap::from([(
                "known".to_string(),
                SessionGitConfig {
                    outer_cwd: temp.path().display().to_string(),
                    workspaces: Vec::new(),
                },
            )])),
            fail: false,
        });
        let app = router(
            store,
            temp.path().to_path_buf(),
            vec!["example.test".to_string()],
        )
        .expect("router");

        let response = request(&app, "/api/sessions/known/git").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["session_id"], "known");
        assert_eq!(body["limit"], DEFAULT_LIMIT);

        let response = request(&app, "/api/sessions/missing/git").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await["error"]["code"],
            "session_not_found"
        );
    }

    #[tokio::test]
    async fn endpoint_strictly_rejects_invalid_limit_queries() {
        let temp = TempDir::new().expect("web root");
        std::fs::write(temp.path().join("index.html"), "spa").expect("index");
        let app = router(
            Arc::new(FakeStore {
                sessions: Mutex::new(HashMap::new()),
                fail: false,
            }),
            temp.path().to_path_buf(),
            vec!["example.test".to_string()],
        )
        .expect("router");
        for query in [
            "?",
            "?limit=",
            "?limit=null",
            "?limit=-1",
            "?limit=0",
            "?limit=1.5",
            "?limit=101",
            "?limit=12&limit=50",
            "?other=12",
            "?limit=%31",
        ] {
            let response = request(&app, &format!("/api/sessions/known/git{query}")).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "query {query}");
            assert_eq!(body_json(response).await["error"]["code"], "invalid_limit");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn endpoint_rejects_executable_repository_config_without_running_it() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let temp = TempDir::new().expect("temp");
        std::fs::write(temp.path().join("index.html"), "spa").expect("index");
        let repository = temp.path().join("repo");
        std::fs::create_dir(&repository).expect("repository");
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.name", "Test Author"],
            vec!["config", "user.email", "test@example.invalid"],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&repository)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .expect("git")
                .success());
        }
        std::fs::write(repository.join("file"), "safe").expect("file");
        for args in [vec!["add", "."], vec!["commit", "-m", "Safe commit"]] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&repository)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .expect("git")
                .success());
        }
        let marker = temp.path().join("endpoint-marker");
        let helper = temp.path().join("malicious-gpg");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\nprintf owned >'{}'\n", marker.display()),
        )
        .expect("helper");
        let mut permissions = std::fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&helper, permissions).expect("helper permissions");
        for args in [
            vec![
                "config",
                "gpg.program",
                helper.to_str().expect("utf8 helper"),
            ],
            vec!["config", "log.showSignature", "true"],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&repository)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .expect("git")
                .success());
        }

        let app = router(
            Arc::new(FakeStore {
                sessions: Mutex::new(HashMap::from([(
                    "known".to_string(),
                    SessionGitConfig {
                        outer_cwd: temp.path().display().to_string(),
                        workspaces: vec![agent_store::SessionWorkspace::git(
                            "repo", "unused", "main", "unused", "main",
                        )],
                    },
                )])),
                fail: false,
            }),
            temp.path().to_path_buf(),
            vec!["example.test".to_string()],
        )
        .expect("router");
        let response = request(&app, "/api/sessions/known/git").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["workspaces"][0]["status"], "unavailable");
        assert!(
            !marker.exists(),
            "repository-controlled endpoint helper executed"
        );
    }

    #[tokio::test]
    async fn router_serves_assets_spa_health_and_rejects_untrusted_hosts() {
        let temp = TempDir::new().expect("web root");
        std::fs::write(temp.path().join("index.html"), "spa-index").expect("index");
        std::fs::write(temp.path().join("asset.js"), "asset").expect("asset");
        let app = router(
            Arc::new(FakeStore {
                sessions: Mutex::new(HashMap::new()),
                fail: false,
            }),
            temp.path().to_path_buf(),
            vec!["example.test".to_string()],
        )
        .expect("router");

        assert_eq!(body_text(request(&app, "/asset.js").await).await, "asset");
        for path in ["/w", "/w/", "/w/project/demo"] {
            let response = request(&app, path).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(body_text(response).await, "spa-index");
        }
        let response = request(&app, "/missing.js").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_ne!(body_text(response).await, "spa-index");
        assert_eq!(body_text(request(&app, "/healthz").await).await, "ok\n");
        for path in ["/api", "/api/", "/api/unknown"] {
            let response = request(&app, path).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(body_json(response).await["error"]["code"], "api_not_found");
        }

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sessions/known/git")
                    .header("host", "example.test")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            body_json(response).await["error"]["code"],
            "method_not_allowed"
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("host", "evil.test")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["error"]["code"], "invalid_host");

        for host in ["[::1]evil", "[::1]:evil", "localhost:evil"] {
            let response = request_with_host(&app, "/healthz", host).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{host}");
            assert_eq!(body_json(response).await["error"]["code"], "invalid_host");
        }

        assert_eq!(
            body_text(request_with_host(&app, "/healthz", "example.test:8788").await).await,
            "ok\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn static_assets_reject_symlinks_and_ignore_late_source_replacements() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("web root");
        let outside = TempDir::new().expect("outside");
        std::fs::write(temp.path().join("index.html"), "immutable-spa").expect("index");
        std::fs::write(temp.path().join("asset.js"), "immutable-asset").expect("asset");
        std::fs::write(outside.path().join("secret.txt"), "outside-secret").expect("secret");
        std::fs::create_dir(outside.path().join("secret-dir")).expect("secret dir");
        std::fs::write(
            outside.path().join("secret-dir/nested.txt"),
            "nested-secret",
        )
        .expect("nested secret");

        symlink(
            outside.path().join("secret.txt"),
            temp.path().join("linked.txt"),
        )
        .expect("file symlink");
        assert!(
            router(
                Arc::new(FakeStore {
                    sessions: Mutex::new(HashMap::new()),
                    fail: false,
                }),
                temp.path().to_path_buf(),
                vec!["example.test".to_string()],
            )
            .is_err(),
            "file symlink must fail staging"
        );
        std::fs::remove_file(temp.path().join("linked.txt")).expect("remove file link");
        symlink(
            outside.path().join("secret-dir"),
            temp.path().join("linked-dir"),
        )
        .expect("directory symlink");
        assert!(
            router(
                Arc::new(FakeStore {
                    sessions: Mutex::new(HashMap::new()),
                    fail: false,
                }),
                temp.path().to_path_buf(),
                vec!["example.test".to_string()],
            )
            .is_err(),
            "directory symlink must fail staging"
        );
        std::fs::remove_file(temp.path().join("linked-dir")).expect("remove directory link");

        let app = router(
            Arc::new(FakeStore {
                sessions: Mutex::new(HashMap::new()),
                fail: false,
            }),
            temp.path().to_path_buf(),
            vec!["example.test".to_string()],
        )
        .expect("router");
        std::fs::remove_file(temp.path().join("asset.js")).expect("remove staged source");
        symlink(
            outside.path().join("secret.txt"),
            temp.path().join("asset.js"),
        )
        .expect("late file replacement");
        std::fs::remove_file(temp.path().join("index.html")).expect("remove staged index");
        symlink(
            outside.path().join("secret-dir"),
            temp.path().join("index.html"),
        )
        .expect("late directory replacement");

        let asset = request(&app, "/asset.js").await;
        assert_eq!(asset.status(), StatusCode::OK);
        assert_eq!(asset.headers()[header::CONTENT_TYPE], "text/javascript");
        assert_eq!(body_text(asset).await, "immutable-asset");
        assert_eq!(
            body_text(request(&app, "/w/project/demo").await).await,
            "immutable-spa"
        );
        for path in [
            "/../secret.txt",
            "/%2e%2e/secret.txt",
            "/%2E%2E%2Fsecret.txt",
            "/..%5csecret.txt",
        ] {
            let response = request(&app, path).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert!(!body_text(response).await.contains("outside-secret"));
        }
    }

    async fn request(app: &Router, uri: &str) -> Response {
        request_with_host(app, uri, "example.test").await
    }

    async fn request_with_host(app: &Router, uri: &str, host: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("host", host)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .unwrap_or_else(|error: Infallible| match error {})
    }

    async fn body_json(response: Response) -> serde_json::Value {
        serde_json::from_slice(
            &to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("body"),
        )
        .expect("json")
    }

    async fn body_text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("body")
                .to_vec(),
        )
        .expect("utf8")
    }
}
