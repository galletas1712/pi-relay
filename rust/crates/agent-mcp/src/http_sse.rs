use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, BoxStream};
use futures_util::StreamExt;
use rmcp::model::{JsonRpcMessage, ServerJsonRpcMessage, ServerNotification, ServerResult};
use rmcp::transport::streamable_http_client::SseError;
use sse_stream::Sse;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::{
    BoundedHttpError, ClientNotifications, RequestRegistration, SecretScrubber, BODY_IDLE_TIMEOUT,
};

pub(super) const SSE_LINE_LIMIT: usize = 256 * 1024;
pub(super) const SSE_EVENT_LIMIT: usize = 2 * 1024 * 1024;
pub(super) const SSE_DATA_LIMIT: usize = 2 * 1024 * 1024;
pub(super) const SSE_FIELD_LIMIT: usize = 16 * 1024;
pub(super) const SSE_EVENTS_PER_RESPONSE_LIMIT: usize = 1_024;
const MAX_SERVER_RETRY: Duration = Duration::from_secs(5);

struct SseReadState {
    response: reqwest::Response,
    registration: Option<RequestRegistration>,
    parser: BoundedSseParser,
    pending: VecDeque<Sse>,
    finished: bool,
}

pub(super) fn bounded_sse_stream(
    response: reqwest::Response,
    registration: Option<RequestRegistration>,
    scrubber: Option<SecretScrubber>,
    notifications: Arc<ClientNotifications>,
    dispatch: SseDispatch,
) -> BoxStream<'static, Result<Sse, SseError>> {
    stream::unfold(
        SseReadState {
            response,
            registration,
            parser: BoundedSseParser::new(scrubber, notifications, dispatch),
            pending: VecDeque::new(),
            finished: false,
        },
        |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), state));
                }
                if state.finished {
                    return None;
                }
                let cancellation = state
                    .registration
                    .as_ref()
                    .map(|registration| &registration.cancellation);
                match read_sse_chunk(&mut state.response, cancellation).await {
                    Ok(Some(chunk)) => {
                        if let Err(error) = state.parser.push(&chunk, &mut state.pending) {
                            state.finished = true;
                            return Some((Err(sse_error(error)), state));
                        }
                    }
                    Ok(None) => {
                        state.finished = true;
                        if let Err(error) = state.parser.finish() {
                            return Some((Err(sse_error(error)), state));
                        }
                        return None;
                    }
                    Err(error) => {
                        state.finished = true;
                        return Some((Err(sse_error(error)), state));
                    }
                }
            }
        },
    )
    .boxed()
}

async fn read_sse_chunk(
    response: &mut reqwest::Response,
    cancellation: Option<&CancellationToken>,
) -> Result<Option<Vec<u8>>, BoundedHttpError> {
    let chunk = timeout(BODY_IDLE_TIMEOUT, response.chunk());
    tokio::pin!(chunk);
    let result = if let Some(cancellation) = cancellation {
        tokio::select! {
            result = &mut chunk => result,
            () = cancellation.cancelled() => return Err(BoundedHttpError::Aborted),
        }
    } else {
        chunk.await
    };
    match result {
        Ok(Ok(chunk)) => Ok(chunk.map(|chunk| chunk.to_vec())),
        Ok(Err(_)) => Err(BoundedHttpError::Request),
        Err(_) => Err(BoundedHttpError::BodyTimeout),
    }
}

fn sse_error(error: BoundedHttpError) -> SseError {
    SseError::Body(Box::new(error))
}

#[derive(Default)]
struct PartialSse {
    event: Option<String>,
    data: Option<String>,
    id: Option<String>,
    retry: Option<u64>,
    bytes: usize,
}

impl PartialSse {
    fn is_empty(&self) -> bool {
        self.event.is_none() && self.data.is_none() && self.id.is_none() && self.retry.is_none()
    }
}

pub(super) struct BoundedSseParser {
    line: Vec<u8>,
    skip_lf: bool,
    event: PartialSse,
    events: usize,
    scrubber: Option<SecretScrubber>,
    notifications: Arc<ClientNotifications>,
    dispatch: SseDispatch,
    response_seen: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SseDispatch {
    Initialize,
    Response,
    Common,
    Discard,
}

impl BoundedSseParser {
    pub(super) fn new(
        scrubber: Option<SecretScrubber>,
        notifications: Arc<ClientNotifications>,
        dispatch: SseDispatch,
    ) -> Self {
        Self {
            line: Vec::new(),
            skip_lf: false,
            event: PartialSse::default(),
            events: 0,
            scrubber,
            notifications,
            dispatch,
            response_seen: false,
        }
    }

    pub(super) fn push(
        &mut self,
        chunk: &[u8],
        pending: &mut VecDeque<Sse>,
    ) -> Result<(), BoundedHttpError> {
        for byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if *byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => {
                    self.finish_line(pending)?;
                    self.skip_lf = true;
                }
                b'\n' => self.finish_line(pending)?,
                byte => {
                    if self.line.len() == SSE_LINE_LIMIT {
                        return Err(BoundedHttpError::BodyTooLarge);
                    }
                    self.line.push(*byte);
                }
            }
        }
        Ok(())
    }

    pub(super) fn finish(&self) -> Result<(), BoundedHttpError> {
        if self.line.is_empty() && self.event.is_empty() {
            Ok(())
        } else {
            Err(BoundedHttpError::InvalidJson)
        }
    }

    fn finish_line(&mut self, pending: &mut VecDeque<Sse>) -> Result<(), BoundedHttpError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            self.finish_event(pending)?;
            return Ok(());
        }
        self.event.bytes = self.event.bytes.saturating_add(line.len());
        if self.event.bytes > SSE_EVENT_LIMIT {
            return Err(BoundedHttpError::BodyTooLarge);
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            return Ok(());
        };
        let field = &line[..colon];
        let mut value = &line[colon + 1..];
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        if value.len() > SSE_FIELD_LIMIT && field != b"data" {
            return Err(BoundedHttpError::BodyTooLarge);
        }
        let value = std::str::from_utf8(value).map_err(|_| BoundedHttpError::InvalidJson)?;
        match field {
            b"data" => {
                let separator = usize::from(self.event.data.is_some());
                let current = self.event.data.as_ref().map_or(0, String::len);
                if current
                    .saturating_add(separator)
                    .saturating_add(value.len())
                    > SSE_DATA_LIMIT
                {
                    return Err(BoundedHttpError::BodyTooLarge);
                }
                let data = self.event.data.get_or_insert_with(String::new);
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value);
            }
            b"event" => self.event.event = Some(self.scrub(value)),
            b"id" if !value.contains('\0') => self.event.id = Some(self.scrub(value)),
            b"retry" => {
                if self
                    .scrubber
                    .as_ref()
                    .is_some_and(|scrubber| scrubber.0.as_ref() == value)
                {
                    return Err(BoundedHttpError::InvalidJson);
                }
                let retry = value
                    .parse::<u64>()
                    .map_err(|_| BoundedHttpError::InvalidJson)?;
                if Duration::from_millis(retry) > MAX_SERVER_RETRY {
                    return Err(BoundedHttpError::InvalidJson);
                }
                self.event.retry = Some(retry);
            }
            b"" | b"id" => {}
            _ => {}
        }
        Ok(())
    }

    fn finish_event(&mut self, pending: &mut VecDeque<Sse>) -> Result<(), BoundedHttpError> {
        let event = std::mem::take(&mut self.event);
        if event.is_empty() {
            return Ok(());
        }
        self.events += 1;
        if self.events > SSE_EVENTS_PER_RESPONSE_LIMIT {
            return Err(BoundedHttpError::BodyTooLarge);
        }
        let data = event.data.map(|data| {
            self.scrubber
                .as_ref()
                .map_or(data.clone(), |scrubber| scrubber.scrub_json_text(&data))
        });
        if matches!(event.event.as_deref(), None | Some("") | Some("message")) {
            let Some(message) = data
                .as_deref()
                .and_then(|data| serde_json::from_str::<ServerJsonRpcMessage>(data).ok())
            else {
                pending.push_back(Sse {
                    event: event.event,
                    data,
                    id: event.id,
                    retry: event.retry,
                });
                return Ok(());
            };
            let initialize_result = match (self.dispatch, &message) {
                (SseDispatch::Initialize, JsonRpcMessage::Response(response)) => {
                    match &response.result {
                        ServerResult::InitializeResult(result) => Some(result),
                        ServerResult::CompleteResult(_)
                        | ServerResult::GetPromptResult(_)
                        | ServerResult::ListPromptsResult(_)
                        | ServerResult::ListResourcesResult(_)
                        | ServerResult::ListResourceTemplatesResult(_)
                        | ServerResult::ReadResourceResult(_)
                        | ServerResult::ListToolsResult(_)
                        | ServerResult::CreateElicitationResult(_)
                        | ServerResult::CreateTaskResult(_)
                        | ServerResult::ListTasksResult(_)
                        | ServerResult::GetTaskResult(_)
                        | ServerResult::CancelTaskResult(_)
                        | ServerResult::CallToolResult(_)
                        | ServerResult::GetTaskPayloadResult(_)
                        | ServerResult::EmptyResult(_)
                        | ServerResult::CustomResult(_) => None,
                    }
                }
                (SseDispatch::Response | SseDispatch::Common | SseDispatch::Discard, _)
                | (
                    SseDispatch::Initialize,
                    JsonRpcMessage::Request(_)
                    | JsonRpcMessage::Notification(_)
                    | JsonRpcMessage::Error(_),
                ) => None,
            };
            if let Some(result) = initialize_result {
                self.notifications.set_accepts_tools_changed(
                    result
                        .capabilities
                        .tools
                        .as_ref()
                        .is_some_and(|tools| tools.list_changed == Some(true)),
                );
            }
            let dispatched = match self.dispatch {
                SseDispatch::Common => true,
                SseDispatch::Response => !self.response_seen,
                SseDispatch::Initialize | SseDispatch::Discard => false,
            };
            if dispatched
                && matches!(
                    &message,
                    JsonRpcMessage::Notification(notification)
                        if matches!(
                            notification.notification,
                            ServerNotification::ToolListChangedNotification(_)
                        )
                )
            {
                self.notifications.mark_tools_changed_received();
            }
            if matches!(
                message,
                JsonRpcMessage::Response(_) | JsonRpcMessage::Error(_)
            ) {
                self.response_seen = true;
            }
        }
        pending.push_back(Sse {
            event: event.event,
            data,
            id: event.id,
            retry: event.retry,
        });
        Ok(())
    }

    fn scrub(&self, value: &str) -> String {
        self.scrubber
            .as_ref()
            .map_or_else(|| value.to_string(), |scrubber| scrubber.scrub(value))
    }
}
