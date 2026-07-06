use std::time::Duration;
use std::time::Instant;

use serde_json::Value;

use crate::{ProviderError, ProviderResult};

const PROVIDER_SSE_STREAM_IDLE_TIMEOUT_SECS: u64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SseEvent {
    Json(Value),
    MalformedJson,
    Done,
}

#[derive(Default)]
struct LocalSseMetrics {
    enabled: bool,
    metrics: agent_perf::SseMetrics,
}

impl LocalSseMetrics {
    fn new() -> Self {
        Self {
            enabled: agent_perf::is_recording(),
            metrics: agent_perf::SseMetrics::default(),
        }
    }

    fn received(&mut self, bytes: usize) {
        if self.enabled {
            self.metrics.received_bytes = self
                .metrics
                .received_bytes
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        }
    }

    fn retained(&mut self, bytes: usize) {
        if self.enabled {
            self.metrics.peak_retained_bytes = self
                .metrics
                .peak_retained_bytes
                .max(u64::try_from(bytes).unwrap_or(u64::MAX));
        }
    }

    fn frame(&mut self) {
        if self.enabled {
            self.metrics.frames = self.metrics.frames.saturating_add(1);
        }
    }

    fn stream_wait(&mut self) -> LocalStreamWait<'_> {
        LocalStreamWait {
            started: self.enabled.then(Instant::now),
            stream_wait_ns: &mut self.metrics.stream_wait_ns,
        }
    }
}

struct LocalStreamWait<'a> {
    started: Option<Instant>,
    stream_wait_ns: &'a mut u64,
}

impl Drop for LocalStreamWait<'_> {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            *self.stream_wait_ns = self
                .stream_wait_ns
                .saturating_add(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }
    }
}

impl Drop for LocalSseMetrics {
    fn drop(&mut self) {
        if self.enabled {
            agent_perf::publish_sse(self.metrics);
        }
    }
}

fn sse_frame_boundary_counted(
    buffer: &[u8],
    metrics: &mut LocalSseMetrics,
) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    if metrics.enabled {
        let lf_windows = lf.map_or_else(
            || buffer.len().saturating_sub(1),
            |position| position.saturating_add(1),
        );
        let crlf_windows = crlf.map_or_else(
            || buffer.len().saturating_sub(3),
            |position| position.saturating_add(1),
        );
        metrics.metrics.scan_windows = metrics
            .metrics
            .scan_windows
            .saturating_add(u64::try_from(lf_windows).unwrap_or(u64::MAX))
            .saturating_add(u64::try_from(crlf_windows).unwrap_or(u64::MAX));
    }
    sse_boundary_from_positions(lf, crlf)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseControl {
    Continue,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseStreamEnd {
    Terminal,
    Eof,
}

pub(crate) async fn read_provider_json_sse_response(
    mut response: reqwest::Response,
    stream_name: &str,
    response_error_message: fn(&str) -> String,
    mut on_event: impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseStreamEnd> {
    let mut metrics = LocalSseMetrics::new();
    let status = response.status();
    if !status.is_success() {
        let bytes = {
            let _wait = metrics.stream_wait();
            response.bytes().await
        }?;
        metrics.received(bytes.len());
        metrics.retained(bytes.len());
        let text = String::from_utf8_lossy(&bytes).into_owned();
        return Err(ProviderError::Status {
            status: status.as_u16(),
            message: response_error_message(&text),
        });
    }

    let idle_timeout = Duration::from_secs(PROVIDER_SSE_STREAM_IDLE_TIMEOUT_SECS);
    let idle_error_message =
        format!("{stream_name} was idle for {PROVIDER_SSE_STREAM_IDLE_TIMEOUT_SECS} seconds");
    let mut buffer = Vec::new();
    loop {
        let chunk_result = {
            let _wait = metrics.stream_wait();
            tokio::time::timeout(idle_timeout, response.chunk()).await
        };
        let chunk = match chunk_result {
            Ok(chunk) => chunk?,
            Err(_) => return Err(ProviderError::Timeout(idle_error_message)),
        };
        let Some(chunk) = chunk else {
            break;
        };
        metrics.received(chunk.len());
        buffer.extend_from_slice(&chunk);
        metrics.retained(buffer.len());
        if process_complete_sse_frames(&mut buffer, &mut metrics, &mut on_event)?
            == SseControl::Stop
        {
            return Ok(SseStreamEnd::Terminal);
        }
    }
    if process_final_sse_frame(&buffer, &mut metrics, &mut on_event)? == SseControl::Stop {
        return Ok(SseStreamEnd::Terminal);
    }
    Ok(SseStreamEnd::Eof)
}

#[cfg(test)]
fn read_json_sse_chunks(
    chunks: impl IntoIterator<Item = Vec<u8>>,
    mut on_event: impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseStreamEnd> {
    let mut metrics = LocalSseMetrics::new();
    let mut buffer = Vec::new();
    for chunk in chunks {
        metrics.received(chunk.len());
        buffer.extend_from_slice(&chunk);
        metrics.retained(buffer.len());
        if process_complete_sse_frames(&mut buffer, &mut metrics, &mut on_event)?
            == SseControl::Stop
        {
            return Ok(SseStreamEnd::Terminal);
        }
    }
    if process_final_sse_frame(&buffer, &mut metrics, &mut on_event)? == SseControl::Stop {
        return Ok(SseStreamEnd::Terminal);
    }
    Ok(SseStreamEnd::Eof)
}

#[cfg(test)]
pub(crate) fn read_json_sse_text(
    text: &str,
    mut on_event: impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseStreamEnd> {
    let mut metrics = LocalSseMetrics::new();
    let mut buffer = text.as_bytes().to_vec();
    metrics.retained(buffer.len());
    if process_complete_sse_frames(&mut buffer, &mut metrics, &mut on_event)? == SseControl::Stop {
        return Ok(SseStreamEnd::Terminal);
    }
    if process_final_sse_frame(&buffer, &mut metrics, &mut on_event)? == SseControl::Stop {
        return Ok(SseStreamEnd::Terminal);
    }
    Ok(SseStreamEnd::Eof)
}

fn process_complete_sse_frames(
    buffer: &mut Vec<u8>,
    metrics: &mut LocalSseMetrics,
    on_event: &mut impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseControl> {
    while let Some((frame_end, separator_len)) = sse_frame_boundary_counted(buffer, metrics) {
        metrics.frame();
        let frame = buffer[..frame_end].to_vec();
        buffer.drain(..frame_end + separator_len);
        if process_sse_frame(&frame, on_event)? == SseControl::Stop {
            return Ok(SseControl::Stop);
        }
    }
    Ok(SseControl::Continue)
}

fn process_final_sse_frame(
    frame: &[u8],
    metrics: &mut LocalSseMetrics,
    on_event: &mut impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseControl> {
    if frame.iter().all(u8::is_ascii_whitespace) {
        return Ok(SseControl::Continue);
    }
    metrics.frame();
    process_sse_frame(frame, on_event)
}

fn process_sse_frame(
    frame: &[u8],
    on_event: &mut impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseControl> {
    match sse_frame_data(frame) {
        Some(SseFrame::Json(data)) => {
            let Ok(event) = serde_json::from_str::<Value>(&data) else {
                // Keep framing separate from provider semantics. Adapters
                // decide whether this explicit marker is fatal.
                return on_event(SseEvent::MalformedJson);
            };
            on_event(SseEvent::Json(event))
        }
        Some(SseFrame::Done) => on_event(SseEvent::Done),
        None => Ok(SseControl::Continue),
    }
}

enum SseFrame {
    Json(String),
    Done,
}

fn sse_frame_data(frame: &[u8]) -> Option<SseFrame> {
    let frame = String::from_utf8_lossy(frame);
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim))
        .collect::<Vec<_>>()
        .join("\n");
    let data = data.trim();
    if data.is_empty() {
        None
    } else if data == "[DONE]" {
        Some(SseFrame::Done)
    } else {
        Some(SseFrame::Json(data.to_string()))
    }
}

fn sse_boundary_from_positions(lf: Option<usize>, crlf: Option<usize>) -> Option<(usize, usize)> {
    match (lf, crlf) {
        (Some(lf), Some(crlf)) if lf < crlf => Some((lf, 2)),
        (Some(_), Some(crlf)) => Some((crlf, 4)),
        (Some(lf), None) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_multiline_data_and_done_sentinel() {
        let mut events = Vec::new();
        let end = read_json_sse_text(
            "event: message\n\
             data: {\"a\":\n\
             data: 1}\n\n\
             data: [DONE]\n\n\
             data: {\"ignored\": true}\n\n",
            |event| {
                let should_stop = matches!(event, SseEvent::Done);
                events.push(event);
                if should_stop {
                    Ok(SseControl::Stop)
                } else {
                    Ok(SseControl::Continue)
                }
            },
        )
        .expect("sse parses");

        assert_eq!(end, SseStreamEnd::Terminal);
        assert_eq!(
            events,
            vec![SseEvent::Json(json!({"a": 1})), SseEvent::Done]
        );
    }

    #[test]
    fn caller_can_stop_stream_on_terminal_event() {
        let mut events = Vec::new();
        let end = read_json_sse_text(
            "data: {\"type\":\"message_stop\"}\n\n\
             data: {\"ignored\": true}\n\n",
            |event| {
                let should_stop = matches!(
                    &event,
                    SseEvent::Json(value)
                        if value.get("type").and_then(Value::as_str) == Some("message_stop")
                );
                events.push(event);
                if should_stop {
                    Ok(SseControl::Stop)
                } else {
                    Ok(SseControl::Continue)
                }
            },
        )
        .expect("sse parses");

        assert_eq!(end, SseStreamEnd::Terminal);
        assert_eq!(
            events,
            vec![SseEvent::Json(json!({"type": "message_stop"}))]
        );
    }

    #[test]
    fn instrumentation_aggregates_one_byte_chunks() {
        let input = b"data: {\"value\":1}\n\n";
        let chunks = input.iter().map(|byte| vec![*byte]);
        let metrics = agent_perf::Metrics::for_test(agent_perf::Operation::ModelAction);
        let mut events = Vec::new();

        let end = tokio_test_scope(&metrics, || {
            read_json_sse_chunks(chunks, |event| {
                events.push(event);
                Ok(SseControl::Continue)
            })
        });

        assert_eq!(end.expect("sse parses"), SseStreamEnd::Eof);
        assert_eq!(events, vec![SseEvent::Json(json!({"value": 1}))]);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.sse_received_bytes, input.len() as u64);
        assert_eq!(snapshot.sse_frames, 1);
        assert_eq!(snapshot.sse_peak_retained_bytes, input.len() as u64);
        assert_eq!(snapshot.sse_scan_windows, 307);
    }

    #[test]
    #[ignore = "1 MiB adversarial fragmentation complexity probe"]
    fn instrumentation_aggregates_large_fragmented_frame() {
        let payload = "x".repeat(1024 * 1024);
        let input = format!("data: {{\"payload\":\"{payload}\"}}\n\n");
        let chunks = input
            .as_bytes()
            .chunks(4093)
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        let metrics = agent_perf::Metrics::for_test(agent_perf::Operation::ModelAction);
        let mut frames = 0;

        tokio_test_scope(&metrics, || {
            read_json_sse_chunks(chunks, |_| {
                frames += 1;
                Ok(SseControl::Continue)
            })
        })
        .expect("sse parses");

        assert_eq!(frames, 1);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.sse_received_bytes, input.len() as u64);
        assert_eq!(snapshot.sse_frames, 1);
        assert_eq!(snapshot.sse_peak_retained_bytes, input.len() as u64);
        assert_eq!(snapshot.sse_scan_windows, 271_382_824);
    }

    #[test]
    fn instrumentation_aggregates_many_frames_in_one_backlog() {
        const FRAMES: usize = 1_000;
        let frame = b"data: {\"ok\":true}\n\n";
        let chunks = [frame.repeat(FRAMES)];
        let metrics = agent_perf::Metrics::for_test(agent_perf::Operation::ModelAction);
        let mut frames = 0;

        tokio_test_scope(&metrics, || {
            read_json_sse_chunks(chunks, |_| {
                frames += 1;
                Ok(SseControl::Continue)
            })
        })
        .expect("sse parses");

        assert_eq!(frames, FRAMES);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.sse_received_bytes, (frame.len() * FRAMES) as u64);
        assert_eq!(snapshot.sse_frames, FRAMES as u64);
        assert_eq!(snapshot.sse_scan_windows, 9_524_500);
    }

    fn tokio_test_scope<T>(metrics: &agent_perf::Metrics, operation: impl FnOnce() -> T) -> T {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime builds");
        runtime.block_on(metrics.scope(async { operation() }))
    }
}
