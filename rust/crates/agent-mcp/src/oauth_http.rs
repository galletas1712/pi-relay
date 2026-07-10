use std::time::Duration;

use reqwest::header::HeaderMap;
use reqwest::redirect::Policy;
use tokio::time::timeout;

use rmcp::transport::auth::OAuthHttpClient;
use rmcp::transport::auth::OAuthHttpClientError;
use rmcp::transport::auth::OAuthHttpClientFuture;
use rmcp::transport::auth::OAuthHttpRedirectPolicy;
use rmcp::transport::auth::OAuthHttpRequest;

const BODY_LIMIT: usize = 1024 * 1024;
const HEADER_COUNT_LIMIT: usize = 128;
const HEADER_BYTES_LIMIT: usize = 32 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(not(test))]
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const HEADER_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(not(test))]
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(not(test))]
const TOTAL_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(test)]
const TOTAL_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct DirectOAuthClient {
    follow_redirects: reqwest::Client,
    stop_redirects: reqwest::Client,
}

impl DirectOAuthClient {
    pub(crate) fn build() -> Result<Self, OAuthHttpClientError> {
        Ok(Self {
            follow_redirects: build_client(Policy::limited(10))?,
            stop_redirects: build_client(Policy::none())?,
        })
    }

    async fn execute_request(
        &self,
        request: OAuthHttpRequest,
    ) -> Result<http::Response<Vec<u8>>, OAuthHttpClientError> {
        let client = match request.redirect_policy {
            OAuthHttpRedirectPolicy::Follow => &self.follow_redirects,
            OAuthHttpRedirectPolicy::Stop => &self.stop_redirects,
            _ => {
                return Err(OAuthHttpClientError::new(
                    "unsupported OAuth HTTP redirect policy",
                ));
            }
        };
        let operation_timeout = request.timeout.unwrap_or(TOTAL_TIMEOUT).min(TOTAL_TIMEOUT);
        let request = reqwest::Request::try_from(request.request)
            .map_err(|_| OAuthHttpClientError::new("invalid OAuth HTTP request"))?;
        timeout(operation_timeout, async {
            let response = timeout(HEADER_TIMEOUT, client.execute(request))
                .await
                .map_err(|_| OAuthHttpClientError::new("OAuth HTTP request timed out"))?
                .map_err(|_| OAuthHttpClientError::new("OAuth HTTP request failed"))?;
            validate_headers(response.headers())?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = read_bounded(response).await?;
            let mut builder = http::Response::builder().status(status);
            for (name, value) in &headers {
                builder = builder.header(name, value);
            }
            builder
                .body(body)
                .map_err(|_| OAuthHttpClientError::new("invalid OAuth HTTP response"))
        })
        .await
        .map_err(|_| OAuthHttpClientError::new("OAuth HTTP request timed out"))?
    }
}

impl OAuthHttpClient for DirectOAuthClient {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(self.execute_request(request))
    }
}

fn build_client(redirect: Policy) -> Result<reqwest::Client, OAuthHttpClientError> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(redirect)
        .retry(reqwest::retry::never())
        .pool_max_idle_per_host(0)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(BODY_IDLE_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .build()
        .map_err(|_| OAuthHttpClientError::new("failed to build OAuth HTTP client"))
}

fn validate_headers(headers: &HeaderMap) -> Result<(), OAuthHttpClientError> {
    if headers.len() > HEADER_COUNT_LIMIT
        || headers
            .iter()
            .try_fold(0usize, |total, (name, value)| {
                total.checked_add(name.as_str().len() + value.as_bytes().len())
            })
            .is_none_or(|total| total > HEADER_BYTES_LIMIT)
    {
        return Err(OAuthHttpClientError::new(
            "OAuth HTTP response headers are too large",
        ));
    }
    Ok(())
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, OAuthHttpClientError> {
    if response
        .content_length()
        .is_some_and(|length| length > BODY_LIMIT as u64)
    {
        return Err(OAuthHttpClientError::new(
            "OAuth HTTP response body is too large",
        ));
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(BODY_LIMIT as u64) as usize,
    );
    loop {
        let chunk = timeout(BODY_IDLE_TIMEOUT, response.chunk())
            .await
            .map_err(|_| OAuthHttpClientError::new("OAuth HTTP response body timed out"))?
            .map_err(|_| OAuthHttpClientError::new("OAuth HTTP response body failed"))?;
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len().saturating_add(chunk.len()) > BODY_LIMIT {
            return Err(OAuthHttpClientError::new(
                "OAuth HTTP response body is too large",
            ));
        }
        body.extend_from_slice(&chunk);
    }
}
