use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::{header::ORIGIN, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{accept_hdr_async_with_config, WebSocketStream};
use url::Url;

const MAX_IN_FLIGHT_HANDSHAKES: usize = 32;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const RPC_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct AllowedOrigins(Arc<BTreeSet<String>>);

impl AllowedOrigins {
    pub(crate) fn parse(values: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut origins = BTreeSet::new();
        for value in values {
            validate_origin(&value)?;
            if !origins.insert(value.clone()) {
                return Err(anyhow!("duplicate allowed origin: {value}"));
            }
        }
        if origins.is_empty() {
            return Err(anyhow!("at least one allowed origin is required"));
        }
        Ok(Self(Arc::new(origins)))
    }

    fn contains(&self, origin: &str) -> bool {
        self.0.contains(origin)
    }
}

pub(crate) struct BrowserWebSocket(WebSocketStream<TcpStream>);

impl BrowserWebSocket {
    pub(crate) fn into_inner(self) -> WebSocketStream<TcpStream> {
        self.0
    }
}

pub(crate) fn handshake_semaphore() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_IN_FLIGHT_HANDSHAKES))
}

pub(crate) async fn accept(
    stream: TcpStream,
    allowed_origins: AllowedOrigins,
    _permit: OwnedSemaphorePermit,
) -> Option<BrowserWebSocket> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(RPC_MAX_BYTES))
        .max_frame_size(Some(RPC_MAX_BYTES));
    let callback = OriginCallback(allowed_origins);
    timeout(
        HANDSHAKE_TIMEOUT,
        accept_hdr_async_with_config(stream, callback, Some(config)),
    )
    .await
    .ok()?
    .ok()
    .map(BrowserWebSocket)
}

struct OriginCallback(AllowedOrigins);

impl Callback for OriginCallback {
    fn on_request(
        self,
        request: &Request,
        response: Response,
    ) -> std::result::Result<Response, ErrorResponse> {
        if request_origin(request).is_some_and(|origin| self.0.contains(origin)) {
            Ok(response)
        } else {
            Err(rejected_origin())
        }
    }
}

fn request_origin(request: &Request) -> Option<&str> {
    let mut values = request.headers().get_all(ORIGIN).iter();
    let origin = values.next()?.to_str().ok()?;
    if values.next().is_some() || validate_origin(origin).is_err() {
        return None;
    }
    Some(origin)
}

fn validate_origin(value: &str) -> Result<()> {
    if value.is_empty() || value == "null" {
        return Err(anyhow!("origin must not be empty or null"));
    }
    let parsed = Url::parse(value).map_err(|_| anyhow!("invalid origin: {value}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(anyhow!("origin must use http or https: {value}"));
    }
    if parsed.host_str().is_some_and(|host| host.ends_with('.')) {
        return Err(anyhow!("origin host must not end with a dot: {value}"));
    }
    let canonical = parsed.origin().ascii_serialization();
    if value != canonical {
        return Err(anyhow!(
            "origin must be its canonical browser serialization ({canonical}): {value}"
        ));
    }
    Ok(())
}

fn rejected_origin() -> ErrorResponse {
    tokio_tungstenite::tungstenite::http::Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body(Some("origin rejected".to_string()))
        .expect("static handshake rejection")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn accepts_only_canonical_browser_serialized_origins() {
        for origin in [
            "https://relay.example.com",
            "https://relay.example.com:8443",
            "http://127.0.0.1:8788",
            "http://[::1]:8788",
        ] {
            validate_origin(origin).expect(origin);
        }
        for origin in [
            "",
            "null",
            "ws://relay.example.com",
            "https://user@relay.example.com",
            "https://relay.example.com/",
            "https://relay.example.com/path",
            "https://relay.example.com?query",
            "https://relay.example.com#fragment",
            "HTTPS://relay.example.com",
            "https://RELAY.example.com",
            "https://relay.example.com:443",
            "http://127.0.0.1:80",
            "https://relay.example.com.",
            "https://relay%2eexample.com",
        ] {
            assert!(validate_origin(origin).is_err(), "{origin}");
        }
    }

    #[test]
    fn configured_origins_are_required_and_unique() {
        assert!(AllowedOrigins::parse(Vec::new()).is_err());
        assert!(AllowedOrigins::parse(vec![
            "https://relay.example.com".to_string(),
            "https://relay.example.com".to_string(),
        ])
        .is_err());
    }

    #[tokio::test]
    async fn accepts_exact_production_and_local_origins() {
        for origin in [
            "https://relay.example.com",
            "http://127.0.0.1:8788",
            "http://[::1]:8788",
        ] {
            assert!(connect(Some(origin), false).await, "{origin}");
        }
    }

    #[tokio::test]
    async fn rejects_missing_null_duplicate_malformed_and_lookalike_origins() {
        for origin in [
            None,
            Some("null"),
            Some("not-an-origin"),
            Some("https://relay.example.com.evil.test"),
            Some("https://RELAY.example.com"),
            Some("https://relay.example.com:443"),
            Some("https://relay.example.com/"),
            Some("https://relay.example.com."),
        ] {
            assert!(!connect(origin, false).await, "{origin:?}");
        }
        assert!(!connect(Some("https://relay.example.com"), true).await);
    }

    #[tokio::test]
    async fn incomplete_handshake_times_out() {
        let (address, server) = server().await;
        let _client = TcpStream::connect(address).await.expect("tcp connection");
        assert!(timeout(HANDSHAKE_TIMEOUT + Duration::from_secs(1), server)
            .await
            .expect("bounded server task")
            .expect("server task")
            .is_none());
    }

    #[tokio::test]
    async fn post_upgrade_messages_are_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let address = listener.local_addr().expect("address");
        let origins = origins();
        let permit = handshake_semaphore().acquire_owned().await.expect("permit");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let accepted = accept(stream, origins, permit).await.expect("accepted");
            accepted.into_inner().next().await.expect("frame").is_err()
        });
        let mut request = format!("ws://{address}/")
            .into_client_request()
            .expect("request");
        request
            .headers_mut()
            .insert(ORIGIN, "https://relay.example.com".parse().expect("origin"));
        let (mut client, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("accepted websocket");
        let client_rejected = client
            .send(Message::Binary(vec![0; RPC_MAX_BYTES + 1].into()))
            .await
            .is_err();
        let server_rejected = server.await.expect("server task");
        assert!(client_rejected || server_rejected);
    }

    async fn connect(origin: Option<&str>, duplicate: bool) -> bool {
        let (address, server) = server().await;
        let mut request = format!("ws://{address}/")
            .into_client_request()
            .expect("request");
        if let Some(origin) = origin {
            request
                .headers_mut()
                .append(ORIGIN, origin.parse().expect("header value"));
            if duplicate {
                request.headers_mut().append(
                    ORIGIN,
                    "http://127.0.0.1:8788".parse().expect("second origin"),
                );
            }
        }
        let connected = tokio_tungstenite::connect_async(request).await.is_ok();
        assert_eq!(server.await.expect("server task").is_some(), connected);
        connected
    }

    async fn server() -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<Option<BrowserWebSocket>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let address = listener.local_addr().expect("address");
        let origins = origins();
        let permit = handshake_semaphore().acquire_owned().await.expect("permit");
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            accept(stream, origins, permit).await
        });
        (address, task)
    }

    fn origins() -> AllowedOrigins {
        AllowedOrigins::parse(vec![
            "https://relay.example.com".to_string(),
            "http://127.0.0.1:8788".to_string(),
            "http://[::1]:8788".to_string(),
        ])
        .expect("origins")
    }
}
