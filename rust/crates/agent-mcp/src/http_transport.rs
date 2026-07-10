use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::redirect::Policy;
use rmcp::model::{
    ClientJsonRpcMessage, JsonRpcMessage, RequestId, ServerJsonRpcMessage, ServerNotification,
    ServerResult,
};
use rmcp::transport::common::client_side_sse::SseRetryPolicy;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpClientTransport,
    StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
};
use serde_json::Value;
use sse_stream::Sse;
use thiserror::Error;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::client::{ClientLiveness, ClientNotifications};
use crate::config::McpStreamableHttpTransportConfig;

#[path = "http_sse.rs"]
mod http_sse;

use http_sse::{bounded_sse_stream, SseDispatch};
#[cfg(test)]
use http_sse::{
    BoundedSseParser, SSE_DATA_LIMIT, SSE_EVENTS_PER_RESPONSE_LIMIT, SSE_EVENT_LIMIT,
    SSE_FIELD_LIMIT, SSE_LINE_LIMIT,
};

const JSON_BODY_LIMIT: usize = 2 * 1024 * 1024;
const ERROR_BODY_LIMIT: usize = 64 * 1024;
const SESSION_ID_LIMIT: usize = 4 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(not(test))]
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const HEADER_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(not(test))]
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(1);
const SSE_RECONNECT_LIMIT: usize = 2;
#[cfg(not(test))]
const SSE_RECONNECT_DELAY: Duration = Duration::from_millis(250);
#[cfg(test)]
const SSE_RECONNECT_DELAY: Duration = Duration::from_millis(20);

const SESSION_ID_HEADER: &str = "mcp-session-id";
const LAST_EVENT_ID_HEADER: &str = "last-event-id";

pub(crate) type HttpTransport = StreamableHttpClientTransport<BoundedHttpClient>;
pub(crate) type BearerResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub(crate) struct HttpConnection {
    pub(crate) transport: HttpTransport,
    pub(crate) control: HttpRequestControl,
}

pub(crate) fn build(
    config: &McpStreamableHttpTransportConfig,
    notifications: Arc<ClientNotifications>,
    liveness: Arc<ClientLiveness>,
) -> Result<HttpConnection> {
    build_with_bearer_resolver(config, notifications, liveness, &|name| {
        std::env::var(name).ok()
    })
}

pub(crate) fn build_with_bearer_resolver(
    config: &McpStreamableHttpTransportConfig,
    notifications: Arc<ClientNotifications>,
    liveness: Arc<ClientLiveness>,
    resolve: &dyn Fn(&str) -> Option<String>,
) -> Result<HttpConnection> {
    let scrubber = resolve_scrubber_with(config, resolve)?;
    let client = build_reqwest_client(reqwest::Client::builder())?;
    let requests = Arc::new(RequestRegistry::default());
    let stream_attempts = Arc::new(Mutex::new(CommonStreams::default()));
    let control = HttpRequestControl {
        requests: requests.clone(),
        stream_attempts: stream_attempts.clone(),
        #[cfg(test)]
        before_apply_negotiated_capabilities: None,
    };
    let client = BoundedHttpClient {
        client,
        scrubber: scrubber.clone(),
        requests,
        stream_attempts,
        notifications,
        liveness,
    };
    let mut transport_config = StreamableHttpClientTransportConfig::with_uri(config.url.clone());
    // A possibly delivered tools/call is never replayed after a stale session.
    transport_config.reinit_on_expired_session = false;
    transport_config.retry_config = Arc::new(BoundedRetryPolicy);
    Ok(HttpConnection {
        transport: StreamableHttpClientTransport::with_client(client, transport_config),
        control,
    })
}

fn build_reqwest_client(builder: reqwest::ClientBuilder) -> Result<reqwest::Client> {
    builder
        // Authorization must always travel directly to the configured MCP
        // origin, never through an ambient or explicitly inherited proxy.
        .no_proxy()
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .pool_max_idle_per_host(0)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(BODY_IDLE_TIMEOUT)
        .build()
        .context("build Streamable HTTP client")
}

pub(crate) fn resolve_scrubber_with(
    config: &McpStreamableHttpTransportConfig,
    resolve: impl FnOnce(&str) -> Option<String>,
) -> Result<Option<SecretScrubber>> {
    let Some(name) = config
        .auth
        .as_ref()
        .and_then(crate::config::McpHttpAuthConfig::bearer_env)
    else {
        return Ok(None);
    };
    let value = resolve(name)
        .ok_or_else(|| anyhow!("bearer token environment variable {name} is not set"))?;
    if value.is_empty() {
        return Err(anyhow!(
            "bearer token environment variable {name} must not be empty"
        ));
    }
    Ok(Some(SecretScrubber::new(value)))
}

#[derive(Clone)]
pub(crate) struct SecretScrubber(Arc<str>);

impl SecretScrubber {
    fn new(secret: String) -> Self {
        Self(secret.into())
    }

    pub(crate) fn scrub(&self, value: &str) -> String {
        value.replace(self.0.as_ref(), "<redacted>")
    }

    fn scrub_json_text(&self, value: &str) -> String {
        match serde_json::from_str::<Value>(value) {
            Ok(mut value) => {
                self.scrub_value(&mut value);
                serde_json::to_string(&value)
                    .unwrap_or_else(|_| self.scrub(value.to_string().as_str()))
            }
            Err(_) => self.scrub(value),
        }
    }

    fn scrub_value(&self, value: &mut Value) {
        match value {
            Value::String(value) => *value = self.scrub(value),
            Value::Array(values) => {
                for value in values {
                    self.scrub_value(value);
                }
            }
            Value::Object(values) => {
                let mut scrubbed = serde_json::Map::new();
                for (key, mut value) in std::mem::take(values) {
                    self.scrub_value(&mut value);
                    scrubbed.insert(self.scrub(&key), value);
                }
                *values = scrubbed;
            }
            Value::Null => {
                if self.0.as_ref() == "null" {
                    *value = Value::String("<redacted>".to_string());
                }
            }
            Value::Bool(scalar) => {
                if self.0.as_ref() == scalar.to_string() {
                    *value = Value::String("<redacted>".to_string());
                }
            }
            Value::Number(scalar) => {
                if self.0.as_ref() == scalar.to_string() {
                    *value = Value::String("<redacted>".to_string());
                }
            }
        }
    }
}

impl std::fmt::Debug for SecretScrubber {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretScrubber(<redacted>)")
    }
}

#[cfg(test)]
#[path = "http_transport_tests.rs"]
mod tests;

#[derive(Clone)]
pub(crate) struct HttpRequestControl {
    requests: Arc<RequestRegistry>,
    stream_attempts: Arc<Mutex<CommonStreams>>,
    #[cfg(test)]
    before_apply_negotiated_capabilities: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl HttpRequestControl {
    pub(crate) fn abort(&self, request_id: &RequestId) {
        self.requests.abort(request_id);
    }

    pub(crate) fn abort_all(&self) {
        self.requests.abort_all();
    }

    pub(crate) fn apply_negotiated_capabilities(
        &self,
        notifications: &ClientNotifications,
        liveness: &ClientLiveness,
    ) {
        #[cfg(test)]
        if let Some(hook) = &self.before_apply_negotiated_capabilities {
            hook();
        }
        let mut streams = self
            .stream_attempts
            .lock()
            .expect("HTTP stream attempt lock");
        apply_terminal_tools_fence(&mut streams, notifications, liveness);
    }
}

#[derive(Default)]
struct RequestRegistry {
    inner: Mutex<RequestRegistryInner>,
}

#[derive(Default)]
struct RequestRegistryInner {
    generation: u64,
    requests: HashMap<RequestId, (u64, CancellationToken)>,
}

#[derive(Debug)]
struct BoundedRetryPolicy;

impl SseRetryPolicy for BoundedRetryPolicy {
    fn retry(&self, current_times: usize) -> Option<Duration> {
        (current_times < SSE_RECONNECT_LIMIT).then_some(SSE_RECONNECT_DELAY)
    }
}

impl RequestRegistry {
    fn register(self: &Arc<Self>, request_id: RequestId) -> RequestRegistration {
        let mut inner = self.inner.lock().expect("HTTP request registry lock");
        inner.generation = inner.generation.wrapping_add(1);
        let generation = inner.generation;
        let cancellation = inner
            .requests
            .remove(&request_id)
            .map_or_else(CancellationToken::new, |(_, cancellation)| cancellation);
        inner
            .requests
            .insert(request_id.clone(), (generation, cancellation.clone()));
        RequestRegistration {
            registry: self.clone(),
            request_id,
            generation,
            cancellation,
        }
    }

    fn abort(&self, request_id: &RequestId) {
        let mut inner = self.inner.lock().expect("HTTP request registry lock");
        let cancellation = inner
            .requests
            .entry(request_id.clone())
            .or_insert_with(|| (0, CancellationToken::new()))
            .1
            .clone();
        cancellation.cancel();
    }

    fn abort_all(&self) {
        let cancellations = self
            .inner
            .lock()
            .expect("HTTP request registry lock")
            .requests
            .values()
            .map(|(_, cancellation)| cancellation.clone())
            .collect::<Vec<_>>();
        for cancellation in cancellations {
            cancellation.cancel();
        }
    }
}

struct RequestRegistration {
    registry: Arc<RequestRegistry>,
    request_id: RequestId,
    generation: u64,
    cancellation: CancellationToken,
}

impl Drop for RequestRegistration {
    fn drop(&mut self) {
        let mut inner = self
            .registry
            .inner
            .lock()
            .expect("HTTP request registry lock");
        if inner
            .requests
            .get(&self.request_id)
            .is_some_and(|(generation, _)| *generation == self.generation)
        {
            inner.requests.remove(&self.request_id);
        }
    }
}

#[derive(Clone)]
pub(crate) struct BoundedHttpClient {
    client: reqwest::Client,
    scrubber: Option<SecretScrubber>,
    requests: Arc<RequestRegistry>,
    stream_attempts: Arc<Mutex<CommonStreams>>,
    notifications: Arc<ClientNotifications>,
    liveness: Arc<ClientLiveness>,
}

#[derive(Default)]
struct CommonStreams {
    sessions: HashMap<String, CommonStreamState>,
    terminal_tools_fenced: bool,
    #[cfg(test)]
    terminal_transition_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Default)]
struct CommonStreamState {
    attempts: usize,
    established: bool,
    terminal: bool,
}

fn apply_terminal_tools_fence(
    streams: &mut CommonStreams,
    notifications: &ClientNotifications,
    liveness: &ClientLiveness,
) {
    if streams.terminal_tools_fenced
        || !notifications.accepts_tools_changed()
        || !streams.sessions.values().any(|stream| stream.terminal)
    {
        return;
    }
    streams.terminal_tools_fenced = true;
    notifications.mark_tools_uncertain();
    liveness.mark_closed();
}

#[derive(Debug, Error)]
pub(crate) enum BoundedHttpError {
    #[error("HTTP request failed")]
    Request,
    #[error("HTTP response headers timed out")]
    HeaderTimeout,
    #[error("HTTP response body stalled")]
    BodyTimeout,
    #[error("HTTP response body exceeds its fixed byte limit")]
    BodyTooLarge,
    #[error("HTTP response contains invalid JSON-RPC")]
    InvalidJson,
    #[error("HTTP server returned status {0}")]
    Status(u16),
    #[error("HTTP request was aborted")]
    Aborted,
    #[error("HTTP response has an invalid header")]
    InvalidHeader,
}

impl BoundedHttpClient {
    fn bearer(&self) -> Option<&str> {
        self.scrubber.as_ref().map(|scrubber| scrubber.0.as_ref())
    }

    fn request_registration(&self, message: &ClientJsonRpcMessage) -> Option<RequestRegistration> {
        match message {
            JsonRpcMessage::Request(request) => Some(self.requests.register(request.id.clone())),
            JsonRpcMessage::Response(_)
            | JsonRpcMessage::Notification(_)
            | JsonRpcMessage::Error(_) => None,
        }
    }

    fn apply_headers(
        &self,
        mut request: reqwest::RequestBuilder,
        session_id: Option<&str>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<reqwest::RequestBuilder, StreamableHttpError<BoundedHttpError>> {
        for (name, value) in custom_headers {
            if matches!(
                name.as_str(),
                "accept"
                    | "authorization"
                    | "content-type"
                    | SESSION_ID_HEADER
                    | LAST_EVENT_ID_HEADER
            ) {
                return Err(StreamableHttpError::ReservedHeaderConflict(
                    name.to_string(),
                ));
            }
            request = request.header(name, value);
        }
        if let Some(session_id) = session_id {
            request = request.header(SESSION_ID_HEADER, session_id);
        }
        if let Some(bearer) = self.bearer() {
            request = request.bearer_auth(bearer);
        }
        Ok(request)
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        registration: Option<&RequestRegistration>,
    ) -> Result<reqwest::Response, StreamableHttpError<BoundedHttpError>> {
        let send = timeout(HEADER_TIMEOUT, request.send());
        tokio::pin!(send);
        if let Some(registration) = registration {
            tokio::select! {
                result = &mut send => map_send_result(result),
                () = registration.cancellation.cancelled() => {
                    Err(StreamableHttpError::Client(BoundedHttpError::Aborted))
                }
            }
        } else {
            map_send_result(send.await)
        }
    }

    fn session_id(&self, response: &reqwest::Response) -> Result<Option<String>> {
        let Some(value) = response.headers().get(SESSION_ID_HEADER) else {
            return Ok(None);
        };
        if value.as_bytes().len() > SESSION_ID_LIMIT {
            return Err(anyhow!("MCP session id exceeds its fixed byte limit"));
        }
        let value = value
            .to_str()
            .map_err(|_| anyhow!("MCP session id is not valid text"))?;
        Ok(Some(self.scrubber.as_ref().map_or_else(
            || value.to_string(),
            |scrubber| scrubber.scrub(value),
        )))
    }

    fn parse_message(&self, bytes: &[u8]) -> Result<ServerJsonRpcMessage> {
        let mut value: Value =
            serde_json::from_slice(bytes).context("parse bounded JSON response")?;
        if let Some(scrubber) = &self.scrubber {
            scrubber.scrub_value(&mut value);
        }
        serde_json::from_value(value).context("deserialize bounded JSON-RPC response")
    }

    fn mark_tools_changed(&self, message: &ServerJsonRpcMessage) {
        if matches!(
            message,
            JsonRpcMessage::Notification(notification)
                if matches!(
                    notification.notification,
                    ServerNotification::ToolListChangedNotification(_)
                )
        ) {
            self.notifications.mark_tools_changed_received();
        }
    }

    fn observe_negotiated_capabilities(&self, message: &ServerJsonRpcMessage) {
        let JsonRpcMessage::Response(response) = message else {
            return;
        };
        let ServerResult::InitializeResult(result) = &response.result else {
            return;
        };
        self.notifications.set_accepts_tools_changed(
            result
                .capabilities
                .tools
                .as_ref()
                .is_some_and(|tools| tools.list_changed == Some(true)),
        );
    }

    fn mark_common_stream_terminal(&self, session_id: &str) {
        let mut streams = self
            .stream_attempts
            .lock()
            .expect("HTTP stream attempt lock");
        let stream = streams.sessions.entry(session_id.to_string()).or_default();
        if stream.terminal || (stream.established && stream.attempts < SSE_RECONNECT_LIMIT + 1) {
            return;
        }
        stream.terminal = true;
        #[cfg(test)]
        if let Some(hook) = &streams.terminal_transition_hook {
            hook();
        }
        apply_terminal_tools_fence(&mut streams, &self.notifications, &self.liveness);
    }
}

impl StreamableHttpClient for BoundedHttpClient {
    type Error = BoundedHttpError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let registration = self.request_registration(&message);
        let sse_dispatch = match &message {
            JsonRpcMessage::Request(request) if request.request.method() == "initialize" => {
                SseDispatch::Initialize
            }
            JsonRpcMessage::Request(_) => SseDispatch::Response,
            JsonRpcMessage::Response(_)
            | JsonRpcMessage::Notification(_)
            | JsonRpcMessage::Error(_) => SseDispatch::Discard,
        };
        let session_was_attached = session_id.is_some();
        let request = self
            .client
            .post(uri.as_ref())
            .header(ACCEPT, "application/json, text/event-stream")
            .json(&message);
        let request = self.apply_headers(request, session_id.as_deref(), custom_headers)?;
        let response = self.send(request, registration.as_ref()).await?;
        let status = response.status();
        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == reqwest::StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        let session_id = self
            .session_id(&response)
            .map_err(|_| StreamableHttpError::Client(BoundedHttpError::InvalidHeader))?;
        let content_type = content_type(&response);
        if !status.is_success() {
            let body = read_bounded(response, ERROR_BODY_LIMIT, registration.as_ref()).await?;
            if content_type == Some(ContentType::Json) {
                if let Ok(message @ JsonRpcMessage::Error(_)) = self.parse_message(&body) {
                    if matches!(sse_dispatch, SseDispatch::Response) {
                        self.mark_tools_changed(&message);
                    }
                    return Ok(StreamableHttpPostResponse::Json(message, session_id));
                }
            }
            return Err(StreamableHttpError::Client(BoundedHttpError::Status(
                status.as_u16(),
            )));
        }
        if response.content_length() == Some(0)
            && matches!(
                message,
                JsonRpcMessage::Notification(_)
                    | JsonRpcMessage::Response(_)
                    | JsonRpcMessage::Error(_)
            )
        {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        match content_type {
            Some(ContentType::Json) => {
                let body = read_bounded(response, JSON_BODY_LIMIT, registration.as_ref()).await?;
                if body.is_empty()
                    && matches!(
                        message,
                        JsonRpcMessage::Notification(_)
                            | JsonRpcMessage::Response(_)
                            | JsonRpcMessage::Error(_)
                    )
                {
                    return Ok(StreamableHttpPostResponse::Accepted);
                }
                let message = self
                    .parse_message(&body)
                    .map_err(|_| StreamableHttpError::Client(BoundedHttpError::InvalidJson))?;
                match sse_dispatch {
                    SseDispatch::Initialize => self.observe_negotiated_capabilities(&message),
                    SseDispatch::Response => self.mark_tools_changed(&message),
                    SseDispatch::Common | SseDispatch::Discard => {}
                }
                Ok(StreamableHttpPostResponse::Json(message, session_id))
            }
            Some(ContentType::Sse) => Ok(StreamableHttpPostResponse::Sse(
                bounded_sse_stream(
                    response,
                    registration,
                    self.scrubber.clone(),
                    self.notifications.clone(),
                    sse_dispatch,
                ),
                session_id,
            )),
            None => Err(StreamableHttpError::UnexpectedContentType(None)),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let request = self.client.delete(uri.as_ref());
        let request = self.apply_headers(request, Some(&session_id), custom_headers)?;
        let response = self.send(request, None).await?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportDeleteSession);
        }
        let status = response.status();
        if !status.is_success() {
            let _ = read_bounded(response, ERROR_BODY_LIMIT, None).await?;
            return Err(StreamableHttpError::Client(BoundedHttpError::Status(
                status.as_u16(),
            )));
        }
        Ok(())
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let attempts = {
            let mut attempts = self
                .stream_attempts
                .lock()
                .expect("HTTP stream attempt lock");
            let stream = attempts.sessions.entry(session_id.to_string()).or_default();
            stream.attempts += 1;
            stream.attempts
        };
        if attempts > SSE_RECONNECT_LIMIT + 1 {
            self.mark_common_stream_terminal(&session_id);
            return Err(StreamableHttpError::Client(BoundedHttpError::Request));
        }
        let mut request = self
            .client
            .get(uri.as_ref())
            .header(ACCEPT, "text/event-stream");
        if let Some(last_event_id) = last_event_id {
            request = request.header(LAST_EVENT_ID_HEADER, last_event_id);
        }
        let request = self.apply_headers(request, Some(&session_id), custom_headers)?;
        let response = match self.send(request, None).await {
            Ok(response) => response,
            Err(error) => {
                self.mark_common_stream_terminal(&session_id);
                return Err(error);
            }
        };
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            self.mark_common_stream_terminal(&session_id);
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        let status = response.status();
        if !status.is_success() {
            let body = read_bounded(response, ERROR_BODY_LIMIT, None).await;
            self.mark_common_stream_terminal(&session_id);
            let _ = body?;
            return Err(StreamableHttpError::Client(BoundedHttpError::Status(
                status.as_u16(),
            )));
        }
        if content_type(&response) != Some(ContentType::Sse) {
            self.mark_common_stream_terminal(&session_id);
            return Err(StreamableHttpError::UnexpectedContentType(None));
        }
        {
            let mut attempts = self
                .stream_attempts
                .lock()
                .expect("HTTP stream attempt lock");
            let stream = attempts.sessions.entry(session_id.to_string()).or_default();
            stream.established = true;
            stream.terminal = false;
        }
        Ok(bounded_sse_stream(
            response,
            None,
            self.scrubber.clone(),
            self.notifications.clone(),
            SseDispatch::Common,
        ))
    }
}

fn map_send_result(
    result: std::result::Result<
        std::result::Result<reqwest::Response, reqwest::Error>,
        tokio::time::error::Elapsed,
    >,
) -> Result<reqwest::Response, StreamableHttpError<BoundedHttpError>> {
    match result {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => Err(StreamableHttpError::Client(BoundedHttpError::Request)),
        Err(_) => Err(StreamableHttpError::Client(BoundedHttpError::HeaderTimeout)),
    }
}

async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
    registration: Option<&RequestRegistration>,
) -> Result<Vec<u8>, StreamableHttpError<BoundedHttpError>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(StreamableHttpError::Client(BoundedHttpError::BodyTooLarge));
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(limit as u64) as usize,
    );
    loop {
        let chunk = timeout(BODY_IDLE_TIMEOUT, response.chunk());
        tokio::pin!(chunk);
        let result = if let Some(registration) = registration {
            tokio::select! {
                result = &mut chunk => result,
                () = registration.cancellation.cancelled() => {
                    return Err(StreamableHttpError::Client(BoundedHttpError::Aborted));
                }
            }
        } else {
            chunk.await
        };
        let chunk = match result {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => return Ok(body),
            Ok(Err(_)) => {
                return Err(StreamableHttpError::Client(BoundedHttpError::Request));
            }
            Err(_) => {
                return Err(StreamableHttpError::Client(BoundedHttpError::BodyTimeout));
            }
        };
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(StreamableHttpError::Client(BoundedHttpError::BodyTooLarge));
        }
        body.extend_from_slice(&chunk);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentType {
    Json,
    Sse,
}

fn content_type(response: &reqwest::Response) -> Option<ContentType> {
    parse_content_type(response.headers().get(CONTENT_TYPE)?.as_bytes())
}

pub(crate) fn parse_content_type(value: &[u8]) -> Option<ContentType> {
    if !value.is_ascii() {
        return None;
    }
    let value = std::str::from_utf8(value).ok()?;
    let mut index = 0;
    skip_ows(value, &mut index);
    let type_start = index;
    consume_token(value, &mut index)?;
    if value.as_bytes().get(index) != Some(&b'/') {
        return None;
    }
    let type_end = index;
    index += 1;
    let subtype_start = index;
    consume_token(value, &mut index)?;
    let subtype_end = index;
    skip_ows(value, &mut index);
    while index < value.len() {
        if value.as_bytes()[index] != b';' {
            return None;
        }
        index += 1;
        skip_ows(value, &mut index);
        consume_token(value, &mut index)?;
        if value.as_bytes().get(index) != Some(&b'=') {
            return None;
        }
        index += 1;
        if value.as_bytes().get(index) == Some(&b'"') {
            consume_quoted_string(value, &mut index)?;
        } else {
            consume_token(value, &mut index)?;
        }
        skip_ows(value, &mut index);
    }
    let essence = (
        &value[type_start..type_end],
        &value[subtype_start..subtype_end],
    );
    if essence.0.eq_ignore_ascii_case("application") && essence.1.eq_ignore_ascii_case("json") {
        Some(ContentType::Json)
    } else if essence.0.eq_ignore_ascii_case("text")
        && essence.1.eq_ignore_ascii_case("event-stream")
    {
        Some(ContentType::Sse)
    } else {
        None
    }
}

fn skip_ows(value: &str, index: &mut usize) {
    while value
        .as_bytes()
        .get(*index)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        *index += 1;
    }
}

fn consume_token(value: &str, index: &mut usize) -> Option<()> {
    let start = *index;
    while value.as_bytes().get(*index).is_some_and(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
    }) {
        *index += 1;
    }
    (*index > start).then_some(())
}

fn consume_quoted_string(value: &str, index: &mut usize) -> Option<()> {
    *index += 1;
    while let Some(byte) = value.as_bytes().get(*index) {
        match byte {
            b'"' => {
                *index += 1;
                return Some(());
            }
            b'\\' => {
                *index += 1;
                let escaped = *value.as_bytes().get(*index)?;
                if !matches!(escaped, b'\t' | b' '..=b'~') {
                    return None;
                }
                *index += 1;
            }
            b'\t' | b' ' | b'!' | b'#'..=b'[' | b']'..=b'~' => *index += 1,
            _ => return None,
        }
    }
    None
}
