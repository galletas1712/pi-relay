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
        buffer.extend_from_slice(&chunk);
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
    while let Some((frame_end, separator_len)) = sse_frame_boundary(buffer) {
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
    process_sse_frame(frame, on_event)
}

fn process_sse_frame(
    frame: &[u8],
    on_event: &mut impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseControl> {
    match sse_frame_data(frame) {
        Some(SseFrame::Json(data)) => {
            let Ok(event) = serde_json::from_str::<Value>(&data) else {
                // Match the legacy TS parser, which skips malformed SSE data
                // chunks for ordinary generation. Callers with stricter
                // terminal contracts (native compaction) can reject this
                // explicit marker.
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
}
