use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout, Instant};
use tokio_util::sync::CancellationToken;

const MAX_HEADER_BYTES: usize = 16 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct CallbackListener {
    listener: TcpListener,
    port: u16,
    path: String,
}

pub(crate) enum CallbackRequest {
    Url(String),
    ProviderError(String),
    Invalid,
}

impl CallbackListener {
    pub(crate) async fn bind(port: Option<u16>, path: String) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            port.unwrap_or(0),
        )))
        .await?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            port,
            path,
        })
    }

    pub(crate) fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.path)
    }

    pub(crate) async fn next(
        &self,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> io::Result<Option<(TcpStream, CallbackRequest)>> {
        let (mut stream, peer) = tokio::select! {
            () = cancel.cancelled() => return Ok(None),
            accepted = tokio::time::timeout_at(deadline, self.listener.accept()) => {
                match accepted {
                    Ok(accepted) => accepted?,
                    Err(_) => return Ok(None),
                }
            }
        };
        if peer.ip() != Ipv4Addr::LOCALHOST {
            return Ok(Some((stream, CallbackRequest::Invalid)));
        }
        let request = tokio::select! {
            () = cancel.cancelled() => return Ok(None),
            request = tokio::time::timeout_at(
                deadline,
                timeout(READ_TIMEOUT, read_request(&mut stream, self.port, &self.path)),
            ) => match request {
                Ok(Ok(Ok(request))) => request,
                Ok(Ok(Err(()))) | Ok(Err(_)) | Err(_) => CallbackRequest::Invalid,
            },
        };
        Ok(Some((stream, request)))
    }
}

async fn read_request(
    stream: &mut TcpStream,
    port: u16,
    expected_path: &str,
) -> Result<CallbackRequest, ()> {
    let mut bytes = Vec::with_capacity(1024);
    loop {
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(());
        }
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await.map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_HEADER_BYTES {
            return Err(());
        }
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&bytes).map_err(|_| ())?;
    let head_end = request.find("\r\n\r\n").ok_or(())?;
    if head_end + 4 != bytes.len() {
        return Err(());
    }
    let mut lines = request[..head_end].split("\r\n");
    let mut request_line = lines.next().ok_or(())?.split(' ');
    if request_line.next() != Some("GET") {
        return Err(());
    }
    let target = request_line.next().ok_or(())?;
    if request_line.next() != Some("HTTP/1.1") || request_line.next().is_some() {
        return Err(());
    }
    let host = lines
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| name.eq_ignore_ascii_case("host").then(|| value.trim()));
    if host != Some(format!("127.0.0.1:{port}").as_str()) {
        return Err(());
    }
    let (path, query) = target.split_once('?').ok_or(())?;
    if path != expected_path || query.is_empty() {
        return Err(());
    }
    let url = format!("http://127.0.0.1:{port}{target}");
    let parsed = reqwest::Url::parse(&url).map_err(|_| ())?;
    if parsed.query_pairs().any(|(name, _)| name == "error") {
        return Ok(CallbackRequest::ProviderError(url));
    }
    Ok(CallbackRequest::Url(url))
}

pub(crate) async fn respond(stream: &mut TcpStream, success: bool) {
    let (status, body) = if success {
        (
            "200 OK",
            "Authentication complete. You may close this window.",
        )
    } else {
        ("400 Bad Request", "Invalid OAuth callback.")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}
