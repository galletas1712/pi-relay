use std::time::Duration;

use crate::{ProviderError, ProviderResult};

const PROVIDER_RESPONSE_HEADER_TIMEOUT_SECS: u64 = 45;

pub(crate) async fn send_provider_generation_request(
    request: reqwest::RequestBuilder,
    request_name: &str,
) -> ProviderResult<reqwest::Response> {
    record_provider_send(&request)?;
    let timeout = Duration::from_secs(PROVIDER_RESPONSE_HEADER_TIMEOUT_SECS);
    let _phase = agent_perf::phase(agent_perf::Phase::ProviderRequestWait);
    match tokio::time::timeout(timeout, request.send()).await {
        Ok(response) => response.map_err(ProviderError::Http),
        Err(_) => Err(ProviderError::Timeout(format!(
            "{request_name} response headers were not received within {} seconds",
            timeout.as_secs()
        ))),
    }
}

pub(crate) fn record_provider_send(request: &reqwest::RequestBuilder) -> ProviderResult<()> {
    if agent_perf::is_recording() {
        if let Some(measured) = request.try_clone() {
            let measured = measured.build()?;
            if !measured
                .headers()
                .contains_key(reqwest::header::CONTENT_ENCODING)
            {
                if let Some(body) = measured.body().and_then(reqwest::Body::as_bytes) {
                    agent_perf::provider_body_serialized(body.len());
                }
            }
        }
    }
    agent_perf::physical_provider_send();
    Ok(())
}
