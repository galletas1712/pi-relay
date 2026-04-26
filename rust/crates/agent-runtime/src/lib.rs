//! Per-session runtime shell for the Rust agent stack.
//!
//! `SessionRuntime` owns one `AgentSession` and exposes the semantic boundary
//! the eventual host process needs: `SessionInput` goes in, `SessionEvent`
//! comes out. Transport framing, process supervision, and concrete model/tool
//! execution live above this crate.

#![forbid(unsafe_code)]

use std::fmt;

use agent_core::{AgentInput, ToolResultMessage, ToolResultStatus};
use agent_session::{AgentSession, SessionAction, SessionEvent, SessionInput, SessionInputError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRuntimeError {
    InvalidInput(String),
    ActionExecution(String),
}

impl fmt::Display for SessionRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(error) => write!(f, "invalid session input: {error}"),
            Self::ActionExecution(error) => write!(f, "session action execution failed: {error}"),
        }
    }
}

impl std::error::Error for SessionRuntimeError {}

impl From<SessionInputError> for SessionRuntimeError {
    fn from(error: SessionInputError) -> Self {
        Self::InvalidInput(error.to_string())
    }
}

/// Runtime for one agent session.
#[derive(Debug, Default)]
pub struct SessionRuntime {
    session: AgentSession,
}

impl SessionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_session(session: AgentSession) -> Self {
        Self { session }
    }

    pub fn enqueue(&mut self, input: SessionInput) -> Result<(), SessionRuntimeError> {
        self.session.enqueue_session_input(input)?;
        Ok(())
    }

    pub fn drive_with_executor<Execute, ExecuteError>(
        &mut self,
        mut execute: Execute,
    ) -> Result<Vec<SessionEvent>, SessionRuntimeError>
    where
        Execute: FnMut(SessionAction) -> Result<Vec<SessionInput>, ExecuteError>,
        ExecuteError: fmt::Display,
    {
        self.session.drive();
        let actions = self.session.drain_actions();

        for action in actions {
            let inputs = match execute(action.clone()) {
                Ok(inputs) => inputs,
                Err(error) => {
                    let error = error.to_string();
                    self.enqueue_action_failure(&action, error.clone())?;
                    return Err(SessionRuntimeError::ActionExecution(error));
                }
            };
            for input in inputs {
                if let Err(error) = self.enqueue(input) {
                    let error = error.to_string();
                    self.enqueue_action_failure(&action, error.clone())?;
                    return Err(SessionRuntimeError::InvalidInput(error));
                }
            }
        }

        Ok(observer_events(self.session.drain_events()))
    }

    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.session.is_quiescent()
    }

    fn enqueue_action_failure(
        &mut self,
        action: &SessionAction,
        error: String,
    ) -> Result<(), SessionRuntimeError> {
        let Some(input) = action_failure_input(action, error) else {
            return Ok(());
        };
        self.enqueue(input)
    }
}

fn action_failure_input(action: &SessionAction, error: String) -> Option<SessionInput> {
    match action {
        SessionAction::RequestModel {
            action_id, turn_id, ..
        } => Some(
            AgentInput::ModelFailed {
                action_id: *action_id,
                turn_id: *turn_id,
                error,
            }
            .into(),
        ),
        SessionAction::RequestTool {
            action_id,
            turn_id,
            tool_call,
        } => Some(
            AgentInput::ToolCompleted {
                action_id: *action_id,
                turn_id: *turn_id,
                result: ToolResultMessage {
                    tool_call_id: tool_call.id,
                    tool_name: tool_call.tool_name.clone(),
                    output: error,
                    status: ToolResultStatus::Crashed,
                },
            }
            .into(),
        ),
        SessionAction::RequestCompaction { request_id, .. } => {
            Some(SessionInput::CompactionFailed {
                request_id: *request_id,
                error,
            })
        }
        SessionAction::CancelSessionWork => None,
    }
}

fn observer_events(events: Vec<SessionEvent>) -> Vec<SessionEvent> {
    events
        .into_iter()
        .filter(|event| !matches!(event, SessionEvent::ActionRequested { .. }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use agent_core::{
        AgentInput, AssistantItem, AssistantMessage, ToolCall, ToolCallId, ToolResultMessage,
        ToolResultStatus, TranscriptItem, TurnId, TurnOutcome,
    };
    use agent_session::ModelContext;

    #[derive(Debug, Default)]
    struct ScriptedActionExecutor {
        model_replies: VecDeque<AssistantMessage>,
        tool_results: VecDeque<ToolResultMessage>,
        compaction_replacements: VecDeque<ModelContext>,
        cancelled: bool,
    }

    impl ScriptedActionExecutor {
        fn push_model_reply(&mut self, reply: AssistantMessage) {
            self.model_replies.push_back(reply);
        }

        fn push_tool_result(&mut self, result: ToolResultMessage) {
            self.tool_results.push_back(result);
        }

        fn handle_action(&mut self, action: SessionAction) -> Result<Vec<SessionInput>, String> {
            match action {
                SessionAction::RequestModel {
                    action_id, turn_id, ..
                } => {
                    let Some(assistant) = self.model_replies.pop_front() else {
                        return Err("missing scripted model reply".to_string());
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
                        return Err("missing scripted tool result".to_string());
                    };
                    Ok(vec![SessionInput::Agent(AgentInput::ToolCompleted {
                        action_id,
                        turn_id,
                        result,
                    })])
                }
                SessionAction::RequestCompaction { request_id, .. } => {
                    let Some(replacement) = self.compaction_replacements.pop_front() else {
                        return Err("missing scripted compaction replacement".to_string());
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

    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    struct ScriptedRun {
        events: Vec<SessionEvent>,
    }

    fn run_until_quiescent(
        runtime: &mut SessionRuntime,
        executor: &mut ScriptedActionExecutor,
        max_cycles: usize,
    ) -> Result<ScriptedRun, String> {
        let mut run = ScriptedRun::default();
        if runtime.is_quiescent() {
            return Ok(run);
        }

        for _ in 0..max_cycles {
            let events = runtime
                .drive_with_executor(|action| executor.handle_action(action))
                .map_err(|error| error.to_string())?;
            run.events.extend(events);

            if runtime.is_quiescent() {
                return Ok(run);
            }
        }

        Err(format!(
            "runtime did not become quiescent within {max_cycles} cycles"
        ))
    }

    fn appended_items(events: &[SessionEvent]) -> Vec<&TranscriptItem> {
        events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::TranscriptItemAppended { item, .. } => Some(item),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn runtime_can_drive_a_turn() {
        let mut runtime = SessionRuntime::new();
        runtime
            .enqueue(AgentInput::follow_up("hello").into())
            .expect("enqueue succeeds");

        let events = runtime
            .drive_with_executor(|_| Ok::<_, String>(Vec::new()))
            .expect("drive succeeds");

        assert!(!runtime.is_quiescent());
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
    fn scripted_runtime_runs_model_turn_to_completion() {
        let mut executor = ScriptedActionExecutor::default();
        executor.push_model_reply(AssistantMessage {
            items: vec![AssistantItem::Text("hi".to_string())],
        });
        let mut runtime = SessionRuntime::new();

        runtime
            .enqueue(AgentInput::follow_up("hello").into())
            .expect("enqueue succeeds");
        let run =
            run_until_quiescent(&mut runtime, &mut executor, 8).expect("scripted run succeeds");

        assert!(appended_items(&run.events).iter().any(|item| {
            matches!(
                item,
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                }
            )
        }));
    }

    #[test]
    fn scripted_runtime_succeeds_on_exact_quiescence_cycle() {
        let mut executor = ScriptedActionExecutor::default();
        executor.push_model_reply(AssistantMessage {
            items: vec![AssistantItem::Text("hi".to_string())],
        });
        let mut runtime = SessionRuntime::new();

        runtime
            .enqueue(AgentInput::follow_up("hello").into())
            .expect("enqueue succeeds");
        run_until_quiescent(&mut runtime, &mut executor, 2)
            .expect("two cycles are enough: request model, then complete it");
    }

    #[test]
    fn already_quiescent_runtime_needs_no_cycles() {
        let mut runtime = SessionRuntime::new();
        let mut executor = ScriptedActionExecutor::default();

        run_until_quiescent(&mut runtime, &mut executor, 0)
            .expect("new runtime is already quiescent");
    }

    #[test]
    fn scripted_runtime_runs_tool_cycle() {
        let tool_call = ToolCall {
            id: ToolCallId(1),
            tool_name: "echo".to_string(),
            args_json: "{}".to_string(),
        };
        let mut executor = ScriptedActionExecutor::default();
        executor.push_model_reply(AssistantMessage {
            items: vec![AssistantItem::ToolCall(tool_call.clone())],
        });
        executor.push_tool_result(ToolResultMessage {
            tool_call_id: tool_call.id,
            tool_name: tool_call.tool_name.clone(),
            output: "done".to_string(),
            status: ToolResultStatus::Success,
        });
        executor.push_model_reply(AssistantMessage {
            items: vec![AssistantItem::Text("finished".to_string())],
        });
        let mut runtime = SessionRuntime::new();

        runtime
            .enqueue(AgentInput::follow_up("use a tool").into())
            .expect("enqueue succeeds");
        let run =
            run_until_quiescent(&mut runtime, &mut executor, 16).expect("scripted run succeeds");
        let items = appended_items(&run.events);

        assert!(items.iter().any(|item| matches!(
            item,
            TranscriptItem::ToolResult(result)
                if result.tool_name == "echo" && result.output == "done"
        )));
        assert!(items.iter().any(|item| matches!(
            item,
            TranscriptItem::TurnFinished {
                outcome: TurnOutcome::Graceful,
                ..
            }
        )));
    }

    #[test]
    fn scripted_runtime_fails_when_work_is_missing() {
        let tool_call = ToolCall {
            id: ToolCallId(1),
            tool_name: "echo".to_string(),
            args_json: "{}".to_string(),
        };
        let mut executor = ScriptedActionExecutor::default();
        executor.push_model_reply(AssistantMessage {
            items: vec![AssistantItem::ToolCall(tool_call)],
        });
        let mut runtime = SessionRuntime::new();

        runtime
            .enqueue(AgentInput::follow_up("use a tool").into())
            .expect("enqueue succeeds");

        let error = run_until_quiescent(&mut runtime, &mut executor, 8)
            .expect_err("tool result is missing");
        assert!(error.contains("tool result"));
    }

    #[test]
    fn executor_error_is_reported_to_session_as_failed_work() {
        let mut runtime = SessionRuntime::new();
        runtime
            .enqueue(AgentInput::follow_up("hello").into())
            .expect("enqueue succeeds");

        let error = runtime
            .drive_with_executor(|_| Err::<Vec<SessionInput>, _>("provider unavailable"))
            .expect_err("executor error is surfaced");
        assert!(matches!(error, SessionRuntimeError::ActionExecution(_)));

        let events = runtime
            .drive_with_executor(|_| Ok::<_, String>(Vec::new()))
            .expect("failure input is processed");
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::TranscriptItemAppended {
                item: TranscriptItem::TurnFinished {
                    outcome: TurnOutcome::Crashed,
                    ..
                },
                ..
            }
        )));
        assert!(runtime.is_quiescent());
    }

    #[test]
    fn runtime_status_tracks_quiescence() {
        let mut executor = ScriptedActionExecutor::default();
        executor.push_model_reply(AssistantMessage {
            items: vec![AssistantItem::Text("hi".to_string())],
        });
        let mut runtime = SessionRuntime::new();

        assert!(runtime.is_quiescent());
        runtime
            .enqueue(AgentInput::follow_up("hello").into())
            .expect("enqueue succeeds");
        assert!(!runtime.is_quiescent());

        run_until_quiescent(&mut runtime, &mut executor, 8).expect("scripted run succeeds");
        assert!(runtime.is_quiescent());
    }
}
