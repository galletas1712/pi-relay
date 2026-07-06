use std::time::Duration;

use serde_json::Value;

use crate::{ProviderError, ProviderResult};

const PROVIDER_SSE_STREAM_IDLE_TIMEOUT_SECS: u64 = 5 * 60;

/// Maximum logical bytes in one SSE frame, excluding its empty-line delimiter.
const PROVIDER_SSE_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Maximum logical pending bytes: one maximum frame and one complete delimiter.
const PROVIDER_SSE_MAX_PENDING_BYTES: usize =
    PROVIDER_SSE_MAX_FRAME_BYTES + SSE_MAX_SEPARATOR_BYTES;
/// Maximum logical bytes retained by the decoder: one maximum pending event
/// plus up to 64 KiB of consumed prefix awaiting amortized compaction.
///
/// The backing `Vec` may retain geometrically larger allocator capacity. That
/// capacity remains constant-bounded by this fixed logical limit, but this
/// constant is not a byte-exact heap residency contract.
const PROVIDER_SSE_MAX_RETAINED_LOGICAL_BYTES: usize =
    PROVIDER_SSE_MAX_PENDING_BYTES + SSE_COMPACT_PREFIX_BYTES;
/// Maximum logical joined `data:` bytes in a multiline SSE event.
///
/// As with the decoder buffer, allocator capacity may geometrically exceed the
/// logical length while remaining constant-bounded by this fixed limit.
const PROVIDER_SSE_MAX_MULTILINE_DATA_BYTES: usize = 8 * 1024 * 1024;
/// Maximum logical non-success body bytes copied for provider diagnostics.
///
/// Transport chunks and the backing `Vec`'s allocator capacity are outside
/// this logical content limit.
const PROVIDER_ERROR_MAX_BODY_BYTES: usize = 64 * 1024;

const SSE_COMPACT_PREFIX_BYTES: usize = 64 * 1024;
const SSE_MAX_SEPARATOR_BYTES: usize = 4;

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
        let (body, exceeded_limit) = read_bounded_error_body(&mut response).await?;
        let text = String::from_utf8_lossy(&body);
        let mut message = response_error_message(&text);
        if exceeded_limit {
            message.push_str(&format!(
                " (provider error body exceeded the {PROVIDER_ERROR_MAX_BODY_BYTES}-byte limit)"
            ));
        }
        return Err(ProviderError::Status {
            status: status.as_u16(),
            message,
        });
    }

    let idle_timeout = Duration::from_secs(PROVIDER_SSE_STREAM_IDLE_TIMEOUT_SECS);
    let idle_error_message =
        format!("{stream_name} was idle for {PROVIDER_SSE_STREAM_IDLE_TIMEOUT_SECS} seconds");
    let mut decoder = SseDecoder::new();
    loop {
        let chunk = match tokio::time::timeout(idle_timeout, response.chunk()).await {
            Ok(chunk) => chunk?,
            Err(_) => return Err(ProviderError::Timeout(idle_error_message)),
        };
        let Some(chunk) = chunk else {
            break;
        };
        if decoder.push(&chunk, &mut on_event)? == SseControl::Stop {
            return Ok(SseStreamEnd::Terminal);
        }
    }
    decoder.finish()?;
    Ok(SseStreamEnd::Eof)
}

async fn read_bounded_error_body(
    response: &mut reqwest::Response,
) -> ProviderResult<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if append_bounded(&mut body, &chunk, PROVIDER_ERROR_MAX_BODY_BYTES) {
            return Ok((body, true));
        }
    }
    Ok((body, false))
}

/// Appends at most `limit` bytes and returns whether any input was rejected.
fn append_bounded(target: &mut Vec<u8>, input: &[u8], limit: usize) -> bool {
    let remaining = limit.saturating_sub(target.len());
    let accepted = remaining.min(input.len());
    target.extend_from_slice(&input[..accepted]);
    accepted < input.len()
}

#[cfg(test)]
pub(crate) fn read_json_sse_text(
    text: &str,
    mut on_event: impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseStreamEnd> {
    let mut decoder = SseDecoder::new();
    if decoder.push(text.as_bytes(), &mut on_event)? == SseControl::Stop {
        return Ok(SseStreamEnd::Terminal);
    }
    decoder.finish()?;
    Ok(SseStreamEnd::Eof)
}

#[derive(Clone, Copy)]
struct SseLimits {
    frame_bytes: usize,
    retained_logical_bytes: usize,
    multiline_data_bytes: usize,
}

const PROVIDER_SSE_LIMITS: SseLimits = SseLimits {
    frame_bytes: PROVIDER_SSE_MAX_FRAME_BYTES,
    retained_logical_bytes: PROVIDER_SSE_MAX_RETAINED_LOGICAL_BYTES,
    multiline_data_bytes: PROVIDER_SSE_MAX_MULTILINE_DATA_BYTES,
};

struct SseDecoder {
    // Frame boundaries are dispatched immediately, so no boundary metadata is
    // queued. Only the byte buffer and its two offsets contribute retained
    // framing state.
    buffer: Vec<u8>,
    frame_start: usize,
    scan_from: usize,
    limits: SseLimits,
    #[cfg(test)]
    stats: SseDecoderStats,
}

impl SseDecoder {
    fn new() -> Self {
        Self::with_limits(PROVIDER_SSE_LIMITS)
    }

    fn with_limits(limits: SseLimits) -> Self {
        Self {
            buffer: Vec::new(),
            frame_start: 0,
            scan_from: 0,
            limits,
            #[cfg(test)]
            stats: SseDecoderStats::default(),
        }
    }

    fn push(
        &mut self,
        input: &[u8],
        on_event: &mut impl FnMut(SseEvent) -> ProviderResult<SseControl>,
    ) -> ProviderResult<SseControl> {
        let mut input_offset = 0;
        while input_offset < input.len() {
            self.compact_if_worthwhile();

            let pending_bytes = self.buffer.len() - self.frame_start;
            let retained_room = self
                .limits
                .retained_logical_bytes
                .saturating_sub(self.buffer.len());
            let pending_room = self
                .limits
                .frame_bytes
                .saturating_add(SSE_MAX_SEPARATOR_BYTES)
                .saturating_sub(pending_bytes);
            let appended = retained_room
                .min(pending_room)
                .min(input.len() - input_offset);
            if appended == 0 {
                if self.frame_start > 0 {
                    self.compact();
                    continue;
                }
                let (limit_name, limit) = if retained_room == 0 {
                    ("retained logical bytes", self.limits.retained_logical_bytes)
                } else {
                    ("frame bytes", self.limits.frame_bytes)
                };
                return Err(sse_limit_error(limit_name, limit));
            }

            let input_end = input_offset + appended;
            self.buffer
                .extend_from_slice(&input[input_offset..input_end]);
            input_offset = input_end;
            #[cfg(test)]
            {
                self.stats.peak_retained_logical_bytes = self
                    .stats
                    .peak_retained_logical_bytes
                    .max(self.buffer.len());
            }

            if self.process_complete_frames(on_event)? == SseControl::Stop {
                return Ok(SseControl::Stop);
            }
            self.validate_pending_frame()?;
        }
        self.compact_if_worthwhile();
        Ok(SseControl::Continue)
    }

    fn finish(&self) -> ProviderResult<()> {
        let pending = &self.buffer[self.frame_start..];
        if pending.iter().all(u8::is_ascii_whitespace) {
            Ok(())
        } else {
            Err(ProviderError::Provider(
                "provider SSE stream ended with an incomplete frame at EOF".to_string(),
            ))
        }
    }

    fn process_complete_frames(
        &mut self,
        on_event: &mut impl FnMut(SseEvent) -> ProviderResult<SseControl>,
    ) -> ProviderResult<SseControl> {
        while let Some((frame_end, separator_len)) = self.next_frame_boundary() {
            let frame_start = self.frame_start;
            let frame_bytes = frame_end - frame_start;
            if frame_bytes > self.limits.frame_bytes {
                return Err(sse_limit_error("frame bytes", self.limits.frame_bytes));
            }

            self.frame_start = frame_end + separator_len;
            self.scan_from = self.frame_start;
            if process_sse_frame(
                &self.buffer[frame_start..frame_end],
                self.limits.multiline_data_bytes,
                on_event,
            )? == SseControl::Stop
            {
                return Ok(SseControl::Stop);
            }
        }
        Ok(SseControl::Continue)
    }

    fn next_frame_boundary(&mut self) -> Option<(usize, usize)> {
        while self.scan_from < self.buffer.len() {
            let index = self.scan_from;
            #[cfg(test)]
            {
                self.stats.boundary_windows += 1;
            }
            match self.buffer[index] {
                b'\n' => {
                    let next = self.buffer.get(index + 1)?;
                    if *next == b'\n' {
                        return Some((index, 2));
                    }
                }
                b'\r' => {
                    let next = self.buffer.get(index + 1)?;
                    if *next == b'\n' {
                        let third = self.buffer.get(index + 2)?;
                        if *third == b'\r' {
                            let fourth = self.buffer.get(index + 3)?;
                            if *fourth == b'\n' {
                                return Some((index, 4));
                            }
                        }
                    }
                }
                _ => {}
            }
            self.scan_from += 1;
        }
        None
    }

    fn validate_pending_frame(&self) -> ProviderResult<()> {
        let pending_bytes = self.buffer.len() - self.frame_start;
        let possible_separator_bytes =
            partial_separator_len(&self.buffer[self.frame_start..]).min(pending_bytes);
        if pending_bytes - possible_separator_bytes > self.limits.frame_bytes {
            return Err(sse_limit_error("frame bytes", self.limits.frame_bytes));
        }
        Ok(())
    }

    fn compact_if_worthwhile(&mut self) {
        if self.frame_start == self.buffer.len() {
            self.buffer.clear();
            self.frame_start = 0;
            self.scan_from = 0;
        } else if self.frame_start >= SSE_COMPACT_PREFIX_BYTES
            && self.frame_start >= self.buffer.len() / 2
        {
            self.compact();
        }
    }

    fn compact(&mut self) {
        debug_assert!(self.scan_from >= self.frame_start);
        let pending_bytes = self.buffer.len() - self.frame_start;
        self.buffer.copy_within(self.frame_start.., 0);
        self.buffer.truncate(pending_bytes);
        self.scan_from -= self.frame_start;
        self.frame_start = 0;
        #[cfg(test)]
        {
            self.stats.compacted_bytes += pending_bytes;
            self.stats.compactions += 1;
        }
    }

    #[cfg(test)]
    fn stats(&self) -> SseDecoderStats {
        self.stats
    }
}

fn partial_separator_len(pending: &[u8]) -> usize {
    [b"\r\n\r".as_slice(), b"\r\n", b"\r", b"\n"]
        .into_iter()
        .find(|prefix| pending.ends_with(prefix))
        .map_or(0, <[u8]>::len)
}

fn sse_limit_error(limit_name: &str, limit: usize) -> ProviderError {
    ProviderError::Provider(format!(
        "provider SSE stream exceeded the {limit}-byte {limit_name} limit"
    ))
}

fn process_sse_frame(
    frame: &[u8],
    multiline_data_limit: usize,
    on_event: &mut impl FnMut(SseEvent) -> ProviderResult<SseControl>,
) -> ProviderResult<SseControl> {
    let Some(data) = sse_frame_data(frame, multiline_data_limit)? else {
        return Ok(SseControl::Continue);
    };
    if data.as_bytes() == b"[DONE]" {
        return on_event(SseEvent::Done);
    }
    let Ok(event) = serde_json::from_slice::<Value>(data.as_bytes()) else {
        // Keep framing separate from provider semantics. Adapters decide
        // whether this explicit marker is fatal.
        return on_event(SseEvent::MalformedJson);
    };
    on_event(SseEvent::Json(event))
}

enum SseFrameData<'a> {
    Single(&'a [u8]),
    Multiline(Vec<u8>),
}

impl SseFrameData<'_> {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Single(data) => data,
            Self::Multiline(data) => data,
        }
    }
}

fn sse_frame_data(
    frame: &[u8],
    multiline_data_limit: usize,
) -> ProviderResult<Option<SseFrameData<'_>>> {
    let mut single: Option<&[u8]> = None;
    let mut multiline: Option<Vec<u8>> = None;
    for line in frame.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if let Some(joined) = multiline.as_mut() {
            let joined_bytes = joined
                .len()
                .checked_add(1)
                .and_then(|len| len.checked_add(data.len()))
                .ok_or_else(|| sse_limit_error("multiline data bytes", multiline_data_limit))?;
            if joined_bytes > multiline_data_limit {
                return Err(sse_limit_error(
                    "multiline data bytes",
                    multiline_data_limit,
                ));
            }
            joined.push(b'\n');
            joined.extend_from_slice(data);
        } else if let Some(first) = single.take() {
            let joined_bytes = first
                .len()
                .checked_add(1)
                .and_then(|len| len.checked_add(data.len()))
                .ok_or_else(|| sse_limit_error("multiline data bytes", multiline_data_limit))?;
            if joined_bytes > multiline_data_limit {
                return Err(sse_limit_error(
                    "multiline data bytes",
                    multiline_data_limit,
                ));
            }
            let mut joined = Vec::with_capacity(joined_bytes);
            joined.extend_from_slice(first);
            joined.push(b'\n');
            joined.extend_from_slice(data);
            multiline = Some(joined);
        } else {
            single = Some(data);
        }
    }

    let data = match (single, multiline) {
        (Some(data), None) if !data.is_empty() => Some(SseFrameData::Single(data)),
        (None, Some(data)) if !data.is_empty() => Some(SseFrameData::Multiline(data)),
        (None, None) | (Some(_), None) | (None, Some(_)) => None,
        (Some(_), Some(_)) => unreachable!("single data is moved into multiline scratch"),
    };
    Ok(data)
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SseDecoderStats {
    boundary_windows: usize,
    compacted_bytes: usize,
    compactions: usize,
    peak_retained_logical_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn decode_chunks<'a>(
        chunks: impl IntoIterator<Item = &'a [u8]>,
    ) -> ProviderResult<(Vec<SseEvent>, SseDecoderStats)> {
        let mut events = Vec::new();
        let mut decoder = SseDecoder::new();
        for chunk in chunks {
            decoder.push(chunk, &mut |event| {
                events.push(event);
                Ok(SseControl::Continue)
            })?;
        }
        Ok((events, decoder.stats()))
    }

    fn decode_with_limits<'a>(
        chunks: impl IntoIterator<Item = &'a [u8]>,
        limits: SseLimits,
    ) -> ProviderResult<(Vec<SseEvent>, SseDecoderStats)> {
        let mut events = Vec::new();
        let mut decoder = SseDecoder::with_limits(limits);
        for chunk in chunks {
            decoder.push(chunk, &mut |event| {
                events.push(event);
                Ok(SseControl::Continue)
            })?;
        }
        Ok((events, decoder.stats()))
    }

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
    fn handles_every_split_of_lf_and_crlf_delimiters() {
        for stream in [
            b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n".as_slice(),
            b"data: {\"a\":1}\r\n\r\ndata: {\"b\":2}\r\n\r\n".as_slice(),
        ] {
            for split in 0..=stream.len() {
                let (events, _) =
                    decode_chunks([&stream[..split], &stream[split..]]).expect("SSE parses");
                assert_eq!(
                    events,
                    vec![
                        SseEvent::Json(json!({"a": 1})),
                        SseEvent::Json(json!({"b": 2})),
                    ],
                    "split {split} of {stream:?}"
                );
            }
        }
    }

    #[test]
    fn preserves_data_bytes_except_one_optional_ascii_space() {
        for (frame, expected) in [
            (
                b"data:{\"kind\":\"none\"}".as_slice(),
                b"{\"kind\":\"none\"}".as_slice(),
            ),
            (b"data: {\"kind\":\"one\"}", b"{\"kind\":\"one\"}"),
            (b"data:  {\"kind\":\"two\"}", b" {\"kind\":\"two\"}"),
            (b"data:\t{\"kind\":\"tab\"}", b"\t{\"kind\":\"tab\"}"),
        ] {
            let data = sse_frame_data(frame, 64)
                .expect("frame data")
                .expect("data field");
            assert_eq!(data.as_bytes(), expected);
        }
    }

    #[test]
    fn ignores_comments_and_frames_without_data() {
        let stream = b": keep-alive\n\n\
                       event: ping\nid: 1\n\n\
                       \n\
                       data: {\"ok\":true}\n\n";
        let (events, _) = decode_chunks([stream.as_slice()]).expect("SSE parses");

        assert_eq!(events, vec![SseEvent::Json(json!({"ok": true}))]);
    }

    #[test]
    fn malformed_json_remains_an_explicit_event() {
        let (events, _) = decode_chunks([b"data: {not-json}\n\n".as_slice()]).expect("SSE parses");

        assert_eq!(events, vec![SseEvent::MalformedJson]);
    }

    #[test]
    fn incomplete_event_at_eof_is_rejected_without_dispatch() {
        for ending in ["", "\n", "\r\n"] {
            let mut events = Vec::new();
            let error =
                read_json_sse_text(&format!("data: {{\"executable\":true}}{ending}"), |event| {
                    events.push(event);
                    Ok(SseControl::Continue)
                })
                .expect_err("unterminated SSE frame must fail");

            match error {
                ProviderError::Provider(message) => assert_eq!(
                    message, "provider SSE stream ended with an incomplete frame at EOF",
                    "ending {ending:?}"
                ),
                other => panic!("ending {ending:?}: expected provider error, got {other:?}"),
            }
            assert!(events.is_empty(), "ending {ending:?}");
        }
    }

    #[test]
    fn whitespace_only_tail_at_eof_is_discarded() {
        let mut events = Vec::new();
        let end = read_json_sse_text(" \t\r\n", |event| {
            events.push(event);
            Ok(SseControl::Continue)
        })
        .expect("whitespace-only EOF tail is ignored");

        assert_eq!(end, SseStreamEnd::Eof);
        assert!(events.is_empty());
    }

    #[test]
    fn one_byte_chunks_are_linear_and_preserve_fragmented_frames() {
        let stream = b"data: {\"a\":1}\r\n\r\ndata: {\"b\":2}\n\ndata: [DONE]\n\n";
        let (events, stats) =
            decode_chunks(stream.iter().map(std::slice::from_ref)).expect("one-byte chunks parse");
        eprintln!(
            "one-byte SSE: input_bytes={}, boundary_windows={}",
            stream.len(),
            stats.boundary_windows
        );

        assert_eq!(
            events,
            vec![
                SseEvent::Json(json!({"a": 1})),
                SseEvent::Json(json!({"b": 2})),
                SseEvent::Done,
            ]
        );
        assert!(
            stats.boundary_windows <= stream.len() * 4,
            "{stats:?}, input bytes {}",
            stream.len()
        );
    }

    #[test]
    fn thousand_frame_backlog_scans_linearly() {
        const FRAME: &[u8] = b"data: {\"value\":1}\n\n";
        const FRAMES: usize = 1_000;
        let stream = FRAME.repeat(FRAMES);
        assert_eq!(stream.len(), 19_000);

        let (events, stats) = decode_chunks([stream.as_slice()]).expect("frame backlog parses");
        eprintln!(
            "SSE backlog: input_bytes={}, frames={FRAMES}, boundary_windows={}, \
             stage_0_boundary_windows=9524500",
            stream.len(),
            stats.boundary_windows
        );

        assert_eq!(events, vec![SseEvent::Json(json!({"value": 1})); FRAMES]);
        assert_eq!(stats.boundary_windows, 18_000);
        assert!(stats.boundary_windows <= stream.len());
    }

    #[test]
    fn fragmented_large_frame_scans_linearly() {
        let data = "x".repeat(SSE_COMPACT_PREFIX_BYTES * 3);
        let stream = format!("data: {{\"data\":\"{data}\"}}\r\n\r\n");
        let chunks = stream.as_bytes().chunks(37).collect::<Vec<_>>();
        let (events, stats) = decode_chunks(chunks).expect("fragmented large frame parses");
        eprintln!(
            "fragmented SSE: input_bytes={}, boundary_windows={}, compacted_bytes={}, \
             compactions={}",
            stream.len(),
            stats.boundary_windows,
            stats.compacted_bytes,
            stats.compactions
        );

        assert_eq!(events, vec![SseEvent::Json(json!({"data": data}))]);
        assert!(stats.boundary_windows <= stream.len() * 4);
    }

    #[test]
    fn compaction_preserves_partial_delimiter_tails() {
        let prefix = b"data: {\"value\":1}\n\n".repeat(4_000);
        assert!(prefix.len() >= SSE_COMPACT_PREFIX_BYTES);
        let tail_data = "x".repeat(SSE_COMPACT_PREFIX_BYTES);
        let frame = format!("data: {{\"tail\":\"{tail_data}\"}}");

        for tail in [b"\n".as_slice(), b"\r", b"\r\n", b"\r\n\r"] {
            let mut first_chunk = prefix.clone();
            first_chunk.extend_from_slice(frame.as_bytes());
            first_chunk.extend_from_slice(tail);
            let mut events = Vec::new();
            let mut decoder = SseDecoder::new();
            decoder
                .push(&first_chunk, &mut |event| {
                    events.push(event);
                    Ok(SseControl::Continue)
                })
                .expect("prefix and partial tail parse");
            assert_eq!(decoder.stats().compactions, 1, "tail {tail:?}");
            assert_eq!(
                decoder.buffer,
                [frame.as_bytes(), tail].concat(),
                "tail {tail:?}"
            );

            let suffix = match tail {
                b"\n" => b"\n".as_slice(),
                b"\r" => b"\n\r\n".as_slice(),
                b"\r\n" => b"\r\n",
                b"\r\n\r" => b"\n",
                _ => unreachable!("test only includes delimiter prefixes"),
            };
            decoder
                .push(suffix, &mut |event| {
                    events.push(event);
                    Ok(SseControl::Continue)
                })
                .expect("completed delimiter parses");
            assert_eq!(
                events.last(),
                Some(&SseEvent::Json(json!({"tail": tail_data}))),
                "tail {tail:?}"
            );
        }
    }

    #[test]
    fn scan_slope_is_linear_when_input_doubles() {
        const FRAME: &[u8] = b"data: {\"value\":1}\r\n\r\n";
        let small = FRAME.repeat(500);
        let large = FRAME.repeat(1_000);
        let (_, small_stats) =
            decode_chunks(small.iter().map(std::slice::from_ref)).expect("small stream parses");
        let (_, large_stats) =
            decode_chunks(large.iter().map(std::slice::from_ref)).expect("large stream parses");
        eprintln!(
            "SSE slope: small_bytes={}, small_windows={}, large_bytes={}, large_windows={}",
            small.len(),
            small_stats.boundary_windows,
            large.len(),
            large_stats.boundary_windows
        );

        assert_eq!(
            large_stats.boundary_windows,
            small_stats.boundary_windows * 2
        );
        assert!(large_stats.boundary_windows <= large.len() * 4);
    }

    #[test]
    fn rejects_oversized_unterminated_frame() {
        let limits = SseLimits {
            frame_bytes: 16,
            retained_logical_bytes: 24,
            multiline_data_bytes: 16,
        };
        let error = decode_with_limits([b"data: 01234567890".as_slice()], limits)
            .expect_err("oversized frame must fail");

        assert_eq!(
            error.to_string(),
            "provider returned an error: provider SSE stream exceeded the 16-byte frame bytes limit"
        );
    }

    #[test]
    fn exact_frame_limit_accepts_every_partial_delimiter_tail() {
        let limits = SseLimits {
            frame_bytes: 16,
            retained_logical_bytes: 24,
            multiline_data_bytes: 16,
        };
        let frame = b"data: {\"ok\":1}  ";
        assert_eq!(frame.len(), limits.frame_bytes);
        for tail in [b"\n".as_slice(), b"\r", b"\r\n", b"\r\n\r"] {
            let mut pending = frame.to_vec();
            pending.extend_from_slice(tail);
            let (events, _) =
                decode_with_limits([pending.as_slice()], limits).expect("partial tail is allowed");
            assert!(events.is_empty(), "tail {tail:?}");
        }
    }

    #[test]
    fn rejects_oversized_multiline_data_scratch() {
        let limits = SseLimits {
            frame_bytes: 64,
            retained_logical_bytes: 72,
            multiline_data_bytes: 7,
        };
        let error = decode_with_limits([b"data: 1234\ndata: 5678\n\n".as_slice()], limits)
            .expect_err("oversized multiline data must fail");

        assert_eq!(
            error.to_string(),
            "provider returned an error: provider SSE stream exceeded the 7-byte multiline data bytes limit"
        );
    }

    #[test]
    fn rejects_oversized_retained_buffer() {
        let limits = SseLimits {
            frame_bytes: 64,
            retained_logical_bytes: 8,
            multiline_data_bytes: 64,
        };
        let error = decode_with_limits([b"data: 123".as_slice()], limits)
            .expect_err("oversized retained buffer must fail");

        assert_eq!(
            error.to_string(),
            "provider returned an error: provider SSE stream exceeded the 8-byte retained logical bytes limit"
        );
    }

    #[tokio::test]
    async fn non_success_response_body_is_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let address = listener.local_addr().expect("listener has address");
        let body = vec![b'x'; PROVIDER_ERROR_MAX_BODY_BYTES + 1];
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request accepted");
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 500 Internal Server Error\r\n\
                         content-length: {}\r\n\
                         connection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("response headers write");
            let _ = socket.write_all(&body).await;
        });
        let response = reqwest::get(format!("http://{address}"))
            .await
            .expect("request succeeds");

        let error = read_provider_json_sse_response(
            response,
            "test stream",
            |body| format!("retained {} bytes", body.len()),
            |_| panic!("non-success response cannot dispatch events"),
        )
        .await
        .expect_err("non-success response fails");

        assert_eq!(
            error.to_string(),
            format!(
                "provider returned HTTP 500: retained {PROVIDER_ERROR_MAX_BODY_BYTES} bytes \
                 (provider error body exceeded the {PROVIDER_ERROR_MAX_BODY_BYTES}-byte limit)"
            )
        );
        server.await.expect("server task exits");
    }

    #[test]
    #[ignore = "1 MiB adversarial fragmentation complexity probe"]
    fn one_mib_fragmented_frame_remains_linear() {
        let payload = "x".repeat(1024 * 1024);
        let stream = format!("data: {{\"payload\":\"{payload}\"}}\n\n");
        let (_, stats) = decode_chunks(stream.as_bytes().chunks(4093)).expect("1 MiB frame parses");
        eprintln!(
            "1 MiB fragmented SSE: input_bytes={}, chunk_bytes=4093, boundary_windows={}, \
             stage_0_boundary_windows=271382824",
            stream.len(),
            stats.boundary_windows
        );

        assert!(stats.boundary_windows <= stream.len() * 4);
    }
}
