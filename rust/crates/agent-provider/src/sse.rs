use std::time::Duration;

use serde_json::Value;

use crate::{ProviderError, ProviderResult};

const PROVIDER_SSE_STREAM_IDLE_TIMEOUT_SECS: u64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SseEvent {
    Json(Value),
    MalformedJson,
    Done,
}

fn sse_frame_boundary_counted(buffer: &[u8]) -> Option<(usize, usize)> {
    if !agent_perf::is_recording() {
        return sse_frame_boundary(buffer);
    }
    // The current scanner starts both delimiter searches at byte zero on every
    // pass. Count the window start positions actually examined by each search;
    // Stage 2 will replace the algorithm and keep this counter as its proof.
    let mut scanned = 0;
    let lf = buffer.windows(2).position(|window| {
        scanned += 1;
        window == b"\n\n"
    });
    let crlf = buffer.windows(4).position(|window| {
        scanned += 1;
        window == b"\r\n\r\n"
    });
    agent_perf::sse_scan_windows(scanned);
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
    let status = response.status();
    if !status.is_success() {
        let bytes = response.bytes().await?;
        agent_perf::sse_received(bytes.len());
        agent_perf::sse_retained(bytes.len());
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
        let chunk = match tokio::time::timeout(idle_timeout, response.chunk()).await {
            Ok(chunk) => chunk?,
            Err(_) => return Err(ProviderError::Timeout(idle_error_message)),
        };
        let Some(chunk) = chunk else {
            break;
        };
        agent_perf::sse_received(chunk.len());
        buffer.extend_from_slice(&chunk);
        agent_perf::sse_retained(buffer.len());
        if process_complete_sse_frames(&mut buffer, &mut on_event)? == SseControl::Stop {
            return Ok(SseStreamEnd::Terminal);
        }
    }
    if process_final_sse_frame(&buffer, &mut on_event)? == SseControl::Stop {
        return Ok(SseStreamEnd::Terminal);
    }
    Ok(SseStreamEnd::Eof)
}

#[cfg(test)]
fn read_json_sse_chunks(
    chunks: impl IntoIterator<Item = Vec<u8>>,
    mut on_event: impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseStreamEnd> {
    let mut buffer = Vec::new();
    for chunk in chunks {
        agent_perf::sse_received(chunk.len());
        buffer.extend_from_slice(&chunk);
        agent_perf::sse_retained(buffer.len());
        if process_complete_sse_frames(&mut buffer, &mut on_event)? == SseControl::Stop {
            return Ok(SseStreamEnd::Terminal);
        }
    }
    if process_final_sse_frame(&buffer, &mut on_event)? == SseControl::Stop {
        return Ok(SseStreamEnd::Terminal);
    }
    Ok(SseStreamEnd::Eof)
}

#[cfg(test)]
pub(crate) fn read_json_sse_text(
    text: &str,
    mut on_event: impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseStreamEnd> {
    let mut buffer = text.as_bytes().to_vec();
    if process_complete_sse_frames(&mut buffer, &mut on_event)? == SseControl::Stop {
        return Ok(SseStreamEnd::Terminal);
    }
    if process_final_sse_frame(&buffer, &mut on_event)? == SseControl::Stop {
        return Ok(SseStreamEnd::Terminal);
    }
    Ok(SseStreamEnd::Eof)
}

fn process_complete_sse_frames(
    buffer: &mut Vec<u8>,
    on_event: &mut impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseControl> {
    while let Some((frame_end, separator_len)) = sse_frame_boundary_counted(buffer) {
        agent_perf::sse_frame();
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
    on_event: &mut impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseControl> {
    if frame.iter().all(u8::is_ascii_whitespace) {
        return Ok(SseControl::Continue);
    }
    agent_perf::sse_frame();
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

fn sse_frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    sse_boundary_from_positions(lf, crlf)
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
        let metrics = agent_perf::Metrics::for_test(agent_perf::Operation::ModelTurn);
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
        let metrics = agent_perf::Metrics::for_test(agent_perf::Operation::ModelTurn);
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
        let metrics = agent_perf::Metrics::for_test(agent_perf::Operation::ModelTurn);
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
