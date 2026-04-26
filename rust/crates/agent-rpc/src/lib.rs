//! RPC-shaped per-session host for headless harnesses and future control planes.
//!
//! This crate deliberately starts with the smallest useful boundary: typed
//! request/response frames around one `AgentSession`, plus a synchronous
//! headless runner for tests. Transport is intentionally absent here. Stdio,
//! TCP, WebSocket, or a process supervisor can serialize these frames later
//! without changing session semantics.

#![forbid(unsafe_code)]

use std::fmt;

use agent_session::{
    AgentSession, ModelContext, SessionAction, SessionEvent, SessionInput, SessionInputError,
    TranscriptStorageNode,
};
use serde::{Deserialize, Serialize};

/// Client request sent to one session host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRpcRequest {
    /// Enqueue a core/session input. The caller usually follows this with
    /// `Drive` and handles returned actions/events.
    Enqueue { input: SessionInput },
    /// Drive the session until it cannot make local progress.
    Drive,
    /// Return a non-mutating snapshot of the current model-visible context.
    Snapshot,
}

/// Host response for one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRpcResponse {
    Ack,
    Snapshot {
        snapshot: SessionSnapshot,
    },
    Driven {
        actions: Vec<SessionAction>,
        events: Vec<SessionEvent>,
        status: SessionStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub model_context: ModelContext,
    pub transcript_entries: Vec<TranscriptStorageNode>,
    pub active_leaf_id: Option<String>,
    pub context_tokens: Option<usize>,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub quiescent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRpcError {
    InvalidInput(String),
    Handler(String),
    NotQuiescent(String),
}

impl fmt::Display for SessionRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(error) => write!(f, "invalid session input: {error}"),
            Self::Handler(error) => write!(f, "headless action handler failed: {error}"),
            Self::NotQuiescent(error) => write!(f, "session is not quiescent: {error}"),
        }
    }
}

impl std::error::Error for SessionRpcError {}

impl From<SessionInputError> for SessionRpcError {
    fn from(error: SessionInputError) -> Self {
        Self::InvalidInput(error.to_string())
    }
}

/// One attachable session endpoint.
#[derive(Debug, Default)]
pub struct SessionRpcHost {
    session: AgentSession,
}

impl SessionRpcHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_session(session: AgentSession) -> Self {
        Self { session }
    }

    pub fn session(&self) -> &AgentSession {
        &self.session
    }

    pub fn handle(
        &mut self,
        request: SessionRpcRequest,
    ) -> Result<SessionRpcResponse, SessionRpcError> {
        match request {
            SessionRpcRequest::Enqueue { input } => {
                self.session.enqueue_session_input(input)?;
                Ok(SessionRpcResponse::Ack)
            }
            SessionRpcRequest::Drive => {
                self.session.drive();
                let actions = self.session.drain_actions();
                let events = self.session.drain_events();
                Ok(SessionRpcResponse::Driven {
                    actions,
                    events,
                    status: self.status(),
                })
            }
            SessionRpcRequest::Snapshot => Ok(SessionRpcResponse::Snapshot {
                snapshot: self.snapshot(),
            }),
        }
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            model_context: self.session.model_context(),
            transcript_entries: self.session.transcript_store().entries(),
            active_leaf_id: self.session.context_leaf_id().map(str::to_string),
            context_tokens: self.session.context_tokens(),
            status: self.status(),
        }
    }

    pub fn status(&self) -> SessionStatus {
        SessionStatus {
            quiescent: self.session.is_quiescent(),
        }
    }
}

/// A deterministic headless action handler used by tests and future CLI smoke
/// checks. Real harnesses will execute actions asynchronously over RPC; this
/// trait keeps the first end-to-end path synchronous and tiny.
pub trait HeadlessActionHandler {
    fn handle_action(
        &mut self,
        action: SessionAction,
    ) -> Result<Vec<SessionInput>, SessionRpcError>;
}

/// Runs one `SessionRpcHost` against a local deterministic action handler.
pub struct HeadlessSession<H> {
    host: SessionRpcHost,
    handler: H,
}

impl<H> HeadlessSession<H>
where
    H: HeadlessActionHandler,
{
    pub fn new(host: SessionRpcHost, handler: H) -> Self {
        Self { host, handler }
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        self.host.snapshot()
    }

    pub fn status(&self) -> SessionStatus {
        self.host.status()
    }

    pub fn enqueue(&mut self, input: SessionInput) -> Result<(), SessionRpcError> {
        self.host.handle(SessionRpcRequest::Enqueue { input })?;
        Ok(())
    }

    pub fn run_until_quiescent(
        &mut self,
        max_cycles: usize,
    ) -> Result<HeadlessRun, SessionRpcError> {
        let mut run = HeadlessRun::default();
        if self.host.status().quiescent {
            return Ok(run);
        }

        for _ in 0..max_cycles {
            let response = self.host.handle(SessionRpcRequest::Drive)?;
            let SessionRpcResponse::Driven {
                actions, events, ..
            } = response
            else {
                unreachable!("drive returns a driven response");
            };

            let made_progress = !actions.is_empty() || !events.is_empty();
            run.actions.extend(actions.iter().cloned());
            run.events.extend(events);

            let mut inputs = Vec::new();
            for action in actions {
                inputs.extend(self.handler.handle_action(action)?);
            }
            let had_inputs = !inputs.is_empty();
            for input in inputs {
                self.host.handle(SessionRpcRequest::Enqueue { input })?;
            }

            if self.host.status().quiescent {
                return Ok(run);
            }

            if !made_progress && !had_inputs {
                return Err(SessionRpcError::NotQuiescent(
                    "no local progress remains, but the session is still waiting on work"
                        .to_string(),
                ));
            }
        }

        Err(SessionRpcError::NotQuiescent(format!(
            "headless session did not become quiescent within {max_cycles} cycles"
        )))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HeadlessRun {
    pub actions: Vec<SessionAction>,
    pub events: Vec<SessionEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use agent_core::{
        ActionId, AgentInput, AssistantItem, AssistantMessage, ToolCall, ToolCallId,
        ToolResultMessage, ToolResultStatus, TranscriptItem, TurnId, TurnOutcome,
    };

    #[derive(Debug, Default)]
    struct ScriptedActionHandler {
        model_replies: VecDeque<AssistantMessage>,
        tool_results: VecDeque<ToolResultMessage>,
        compaction_replacements: VecDeque<ModelContext>,
        cancelled: bool,
    }

    impl ScriptedActionHandler {
        fn new() -> Self {
            Self::default()
        }

        fn push_model_reply(&mut self, reply: AssistantMessage) {
            self.model_replies.push_back(reply);
        }

        fn push_tool_result(&mut self, result: ToolResultMessage) {
            self.tool_results.push_back(result);
        }
    }

    impl HeadlessActionHandler for ScriptedActionHandler {
        fn handle_action(
            &mut self,
            action: SessionAction,
        ) -> Result<Vec<SessionInput>, SessionRpcError> {
            match action {
                SessionAction::RequestModel {
                    action_id, turn_id, ..
                } => {
                    let Some(assistant) = self.model_replies.pop_front() else {
                        return Err(SessionRpcError::Handler(
                            "missing scripted model reply".to_string(),
                        ));
                    };
                    Ok(vec![SessionInput::ModelCompleted {
                        action_id,
                        turn_id,
                        assistant,
                        context_tokens: None,
                    }])
                }
                SessionAction::RequestTool {
                    action_id, turn_id, ..
                } => {
                    let Some(result) = self.tool_results.pop_front() else {
                        return Err(SessionRpcError::Handler(
                            "missing scripted tool result".to_string(),
                        ));
                    };
                    Ok(vec![SessionInput::Agent(AgentInput::ToolCompleted {
                        action_id,
                        turn_id,
                        result,
                    })])
                }
                SessionAction::RequestCompaction { request_id, .. } => {
                    let Some(replacement) = self.compaction_replacements.pop_front() else {
                        return Err(SessionRpcError::Handler(
                            "missing scripted compaction replacement".to_string(),
                        ));
                    };
                    Ok(vec![SessionInput::CompactionCompleted {
                        request_id,
                        replacement,
                        context_tokens: None,
                    }])
                }
                SessionAction::CancelSessionWork => {
                    self.cancelled = true;
                    Ok(Vec::new())
                }
            }
        }
    }

    #[test]
    fn rpc_host_can_drive_a_turn() {
        let mut host = SessionRpcHost::new();
        host.handle(SessionRpcRequest::Enqueue {
            input: AgentInput::follow_up("hello").into(),
        })
        .expect("enqueue succeeds");

        let response = host
            .handle(SessionRpcRequest::Drive)
            .expect("drive succeeds");
        let SessionRpcResponse::Driven {
            actions,
            events,
            status,
        } = response
        else {
            panic!("expected driven response");
        };
        assert_eq!(actions.len(), 1);
        assert!(!status.quiescent);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                SessionEvent::TranscriptItemAppended {
                    item: TranscriptItem::UserMessage(text),
                    ..
                } if text == "hello"
            )
        }));
    }

    #[test]
    fn headless_session_runs_model_turn_to_completion() {
        let mut handler = ScriptedActionHandler::new();
        handler.push_model_reply(AssistantMessage {
            items: vec![AssistantItem::Text("hi".to_string())],
        });
        let mut session = HeadlessSession::new(SessionRpcHost::new(), handler);

        session
            .enqueue(AgentInput::follow_up("hello").into())
            .expect("enqueue succeeds");
        session
            .run_until_quiescent(8)
            .expect("headless run succeeds");

        let snapshot = session.snapshot();
        assert_eq!(
            snapshot.model_context.transcript_items().last(),
            Some(&TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            })
        );
    }

    #[test]
    fn headless_session_succeeds_on_exact_quiescence_cycle() {
        let mut handler = ScriptedActionHandler::new();
        handler.push_model_reply(AssistantMessage {
            items: vec![AssistantItem::Text("hi".to_string())],
        });
        let mut session = HeadlessSession::new(SessionRpcHost::new(), handler);

        session
            .enqueue(AgentInput::follow_up("hello").into())
            .expect("enqueue succeeds");
        session
            .run_until_quiescent(2)
            .expect("two cycles are enough: request model, then complete it");
    }

    #[test]
    fn already_quiescent_headless_session_needs_no_cycles() {
        let mut session = HeadlessSession::new(SessionRpcHost::new(), ScriptedActionHandler::new());

        session
            .run_until_quiescent(0)
            .expect("new session is already quiescent");
    }

    #[test]
    fn headless_session_runs_tool_cycle() {
        let tool_call = ToolCall {
            id: ToolCallId(1),
            tool_name: "echo".to_string(),
            args_json: "{}".to_string(),
        };
        let mut handler = ScriptedActionHandler::new();
        handler.push_model_reply(AssistantMessage {
            items: vec![AssistantItem::ToolCall(tool_call.clone())],
        });
        handler.push_tool_result(ToolResultMessage {
            tool_call_id: tool_call.id,
            tool_name: tool_call.tool_name.clone(),
            output: "done".to_string(),
            status: ToolResultStatus::Success,
        });
        handler.push_model_reply(AssistantMessage {
            items: vec![AssistantItem::Text("finished".to_string())],
        });
        let mut session = HeadlessSession::new(SessionRpcHost::new(), handler);

        session
            .enqueue(AgentInput::follow_up("use a tool").into())
            .expect("enqueue succeeds");
        session
            .run_until_quiescent(16)
            .expect("headless run succeeds");

        let snapshot = session.snapshot();
        let items = snapshot.model_context.transcript_items();
        assert!(items.iter().any(|item| matches!(
            item,
            TranscriptItem::ToolResult(result)
                if result.tool_name == "echo" && result.output == "done"
        )));
        assert!(matches!(
            items.last(),
            Some(TranscriptItem::TurnFinished {
                outcome: TurnOutcome::Graceful,
                ..
            })
        ));
    }

    #[test]
    fn headless_session_fails_when_scripted_work_is_missing() {
        let tool_call = ToolCall {
            id: ToolCallId(1),
            tool_name: "echo".to_string(),
            args_json: "{}".to_string(),
        };
        let mut handler = ScriptedActionHandler::new();
        handler.push_model_reply(AssistantMessage {
            items: vec![AssistantItem::ToolCall(tool_call)],
        });
        let mut session = HeadlessSession::new(SessionRpcHost::new(), handler);

        session
            .enqueue(AgentInput::follow_up("use a tool").into())
            .expect("enqueue succeeds");

        assert!(matches!(
            session.run_until_quiescent(8),
            Err(SessionRpcError::Handler(error)) if error.contains("tool result")
        ));
    }

    #[test]
    fn snapshot_includes_attach_metadata() {
        let mut host = SessionRpcHost::new();
        host.handle(SessionRpcRequest::Enqueue {
            input: AgentInput::follow_up("hello").into(),
        })
        .expect("enqueue succeeds");
        host.handle(SessionRpcRequest::Drive)
            .expect("drive succeeds");

        let response = host
            .handle(SessionRpcRequest::Snapshot)
            .expect("snapshot succeeds");
        let SessionRpcResponse::Snapshot { snapshot } = response else {
            panic!("expected snapshot");
        };
        assert_eq!(snapshot.transcript_entries.len(), 2);
        assert!(snapshot.active_leaf_id.is_some());
        assert!(!snapshot.status.quiescent);
    }

    #[test]
    fn json_wire_shape_is_pinned_for_ts_clients() {
        let request = SessionRpcRequest::Enqueue {
            input: AgentInput::follow_up("hello").into(),
        };

        assert_eq!(
            serde_json::to_value(request).expect("request serializes"),
            serde_json::json!({
                "type": "enqueue",
                "input": {
                    "type": "agent",
                    "payload": {
                        "type": "follow_up",
                        "payload": {
                            "from": null,
                            "kind": null,
                            "content": "hello"
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn driven_response_action_shape_is_pinned_for_ts_clients() {
        let response = SessionRpcResponse::Driven {
            actions: vec![SessionAction::RequestModel {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                model_context: ModelContext::new(),
                context_leaf_id: None,
                context_tokens: None,
            }],
            events: Vec::new(),
            status: SessionStatus { quiescent: false },
        };

        assert_eq!(
            serde_json::to_value(response).expect("response serializes"),
            serde_json::json!({
                "type": "driven",
                "actions": [{
                    "type": "request_model",
                    "payload": {
                        "action_id": 1,
                        "turn_id": 1,
                        "model_context": {
                            "transcript_items": []
                        },
                        "context_leaf_id": null,
                        "context_tokens": null
                    }
                }],
                "events": [],
                "status": {
                    "quiescent": false
                }
            })
        );
    }
}
