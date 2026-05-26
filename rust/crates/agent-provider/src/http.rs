use std::time::Duration;

use crate::{ProviderError, ProviderResult};

const PROVIDER_RESPONSE_HEADER_TIMEOUT_SECS: u64 = 45;

pub(crate) async fn send_provider_generation_request(
    request: reqwest::RequestBuilder,
    request_name: &str,
) -> ProviderResult<reqwest::Response> {
    let timeout = Duration::from_secs(PROVIDER_RESPONSE_HEADER_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, request.send()).await {
        Ok(response) => response.map_err(ProviderError::Http),
        Err(_) => Err(ProviderError::Timeout(format!(
            "{request_name} response headers were not received within {} seconds",
            timeout.as_secs()
        ))),
    }
}
