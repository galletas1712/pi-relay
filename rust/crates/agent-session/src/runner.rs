use std::fmt;
use std::future::{ready, Future, Ready};
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures_core::Stream;

use crate::action::SessionAction;
use crate::event::SessionEvent;
use crate::input::{SessionInput, SessionInputError};
use crate::AgentSession;

#[derive(Debug, Clone)]
pub struct AgentInputHandle {
    inputs: UnboundedSender<SessionInput>,
}

impl AgentInputHandle {
    /// Create the input side of an agent run loop.
    ///
    /// The handle is cloneable so orchestrator, model, and tool tasks can all
    /// enqueue completions or user input back into the same session.
    pub fn channel() -> (Self, AgentInputReceiver) {
        let (inputs, input_rx) = unbounded();
        (Self { inputs }, AgentInputReceiver { inputs: input_rx })
    }

    pub fn enqueue_input(
        &self,
        input: impl Into<SessionInput>,
    ) -> Result<(), AgentInputHandleError> {
        let input = input.into();
        input.validate().map_err(AgentInputHandleError::Invalid)?;
        self.inputs
            .unbounded_send(input)
            .map_err(|error| AgentInputHandleError::Closed(error.into_inner()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentInputHandleError {
    Invalid(SessionInputError),
    Closed(SessionInput),
}

impl fmt::Display for AgentInputHandleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => write!(f, "invalid agent input: {error}"),
            Self::Closed(_) => write!(f, "agent input channel is closed"),
        }
    }
}

impl std::error::Error for AgentInputHandleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(error) => Some(error),
            Self::Closed(_) => None,
        }
    }
}

/// Receive side of the agent input queue.
pub struct AgentInputReceiver {
    inputs: UnboundedReceiver<SessionInput>,
}

/// Async integration shell around `AgentSession`.
///
/// AgentRunner owns the proactive run loop: it receives inputs, drives the
/// session until quiescent, and forwards any requested actions to the
/// registered handler. Records flow automatically into the session log via
/// `AgentSession::drive`, so callers observing durable history read it off
/// the session's model_context rather than through a item callback.
///
/// Action handlers are dispatch hooks, not long-running workers. A handler may
/// register or spawn model/tool work and enqueue the eventual completion
/// through an `AgentInputHandle`, but it should return promptly so the runner
/// can keep draining actions and processing interrupts.
pub struct AgentRunner<HandleAction, HandleEvent = fn(SessionEvent) -> Ready<()>> {
    session: AgentSession,
    inputs: AgentInputReceiver,
    handle_action: HandleAction,
    handle_event: HandleEvent,
}

impl<HandleAction> AgentRunner<HandleAction, fn(SessionEvent) -> Ready<()>> {
    pub fn new(
        session: AgentSession,
        inputs: AgentInputReceiver,
        handle_action: HandleAction,
    ) -> Self {
        Self {
            session,
            inputs,
            handle_action,
            handle_event: ignore_session_event,
        }
    }
}

impl<HandleAction, HandleEvent> AgentRunner<HandleAction, HandleEvent> {
    pub fn new_with_events(
        session: AgentSession,
        inputs: AgentInputReceiver,
        handle_action: HandleAction,
        handle_event: HandleEvent,
    ) -> Self {
        Self {
            session,
            inputs,
            handle_action,
            handle_event,
        }
    }

    pub fn session(&self) -> &AgentSession {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut AgentSession {
        &mut self.session
    }
}

impl<HandleAction, HandleActionFuture, HandleEvent, HandleEventFuture>
    AgentRunner<HandleAction, HandleEvent>
where
    HandleAction: FnMut(SessionAction) -> HandleActionFuture,
    HandleActionFuture: Future<Output = ()>,
    HandleEvent: FnMut(SessionEvent) -> HandleEventFuture,
    HandleEventFuture: Future<Output = ()>,
{
    pub async fn run(&mut self) {
        self.drive_and_flush().await;

        while let Some(input) = next_input(&mut self.inputs.inputs).await {
            if self.session.enqueue_session_input(input).is_err() {
                continue;
            }
            self.drive_and_flush().await;
        }
    }

    async fn drive_and_flush(&mut self) {
        self.session.drive();
        for event in self.session.drain_events() {
            (self.handle_event)(event).await;
        }
        for action in self.session.drain_actions() {
            (self.handle_action)(action).await;
        }
    }
}

fn ignore_session_event(_: SessionEvent) -> Ready<()> {
    ready(())
}

struct NextInput<'a, InputStream> {
    inputs: &'a mut InputStream,
}

fn next_input<InputStream>(inputs: &mut InputStream) -> NextInput<'_, InputStream>
where
    InputStream: Stream + Unpin,
{
    NextInput { inputs }
}

impl<InputStream> Future for NextInput<'_, InputStream>
where
    InputStream: Stream + Unpin,
{
    type Output = Option<InputStream::Item>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut *self.inputs).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::SessionInputError;
    use std::cell::RefCell;
    use std::future::{ready, Future};
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    use agent_core::{
        ActionId, AgentInput, AssistantItem, AssistantMessage, TranscriptItem, TurnId, TurnOutcome,
    };

    fn block_on_ready<F: Future>(future: F) -> F::Output {
        let mut future = Box::pin(future);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }
        }
    }

    #[test]
    fn runner_registers_handler_and_drives_enqueued_inputs() {
        let actions = Rc::new(RefCell::new(Vec::new()));
        let recorded_actions = actions.clone();
        let assistant = AssistantMessage {
            items: vec![AssistantItem::Text("hi".to_string())],
        };
        let (input_handle, input_rx) = AgentInputHandle::channel();
        let mut runner = AgentRunner::new(AgentSession::new(), input_rx, move |action| {
            recorded_actions.borrow_mut().push(action);
            ready(())
        });

        input_handle
            .enqueue_input(AgentInput::follow_up("hello"))
            .expect("runner should accept user input");
        input_handle
            .enqueue_input(AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            })
            .expect("runner should accept model completion");
        drop(input_handle);

        block_on_ready(runner.run());

        let SessionAction::RequestModel {
            action_id,
            turn_id,
            model_context,
        } = &actions.borrow()[0]
        else {
            panic!("expected RequestModel action");
        };
        assert_eq!((*action_id, *turn_id), (ActionId(1), TurnId(1)));
        assert_eq!(
            model_context.transcript_items(),
            &[
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("hello".to_string()),
            ]
        );
        assert_eq!(
            runner.session().model_context().transcript_items(),
            &[
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("hello".to_string()),
                TranscriptItem::AssistantMessage(assistant),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
            ]
        );
        assert!(runner.session().is_idle());
    }

    #[test]
    fn runner_request_model_action_can_drive_model_completion_from_handler() {
        let actions = Rc::new(RefCell::new(Vec::new()));
        let recorded_actions = actions.clone();
        let (input_handle, input_rx) = AgentInputHandle::channel();
        let completion_handle = Rc::new(RefCell::new(Some(input_handle.clone())));
        let captured_completion_handle = completion_handle.clone();
        let assistant = AssistantMessage {
            items: vec![AssistantItem::Text("hi from handler".to_string())],
        };
        let assistant_for_handler = assistant.clone();
        let mut runner = AgentRunner::new(
            AgentSession::new(),
            input_rx,
            move |action: SessionAction| {
                recorded_actions.borrow_mut().push(action.clone());
                if let SessionAction::RequestModel {
                    action_id,
                    turn_id,
                    model_context,
                } = action
                {
                    assert!(model_context.transcript_items().iter().any(
                        |item| matches!(item, TranscriptItem::UserMessage(text) if text == "hello")
                    ));
                    if let Some(handle) = captured_completion_handle.borrow_mut().take() {
                        handle
                            .enqueue_input(AgentInput::ModelCompleted {
                                action_id,
                                turn_id,
                                assistant: assistant_for_handler.clone(),
                            })
                            .expect("handler should enqueue model completion");
                    }
                }
                ready(())
            },
        );

        input_handle
            .enqueue_input(AgentInput::follow_up("hello"))
            .expect("runner should accept user input");
        drop(input_handle);

        block_on_ready(runner.run());

        assert_eq!(actions.borrow().len(), 1);
        assert_eq!(
            runner.session().model_context().transcript_items(),
            &[
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("hello".to_string()),
                TranscriptItem::AssistantMessage(assistant),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
            ]
        );
        assert!(runner.session().is_idle());
    }

    #[test]
    fn runner_can_forward_runtime_events() {
        let actions = Rc::new(RefCell::new(Vec::new()));
        let events = Rc::new(RefCell::new(Vec::new()));
        let recorded_actions = actions.clone();
        let recorded_events = events.clone();
        let (input_handle, input_rx) = AgentInputHandle::channel();
        let mut runner = AgentRunner::new_with_events(
            AgentSession::new(),
            input_rx,
            move |action| {
                recorded_actions.borrow_mut().push(action);
                ready(())
            },
            move |event| {
                recorded_events.borrow_mut().push(event);
                ready(())
            },
        );

        input_handle
            .enqueue_input(AgentInput::follow_up("hello"))
            .expect("runner should accept user input");
        drop(input_handle);

        block_on_ready(runner.run());

        assert!(actions
            .borrow()
            .iter()
            .any(|action| matches!(action, SessionAction::RequestModel { .. })));
        assert!(events.borrow().iter().any(|event| matches!(
            event,
            SessionEvent::TranscriptItemAppended {
                item: TranscriptItem::UserMessage(text),
                ..
            } if text == "hello"
        )));
        assert!(events.borrow().iter().any(|event| matches!(
            event,
            SessionEvent::ActionRequested {
                action: SessionAction::RequestModel { .. }
            }
        )));
    }

    #[test]
    fn input_handle_rejects_invalid_inputs_before_queueing() {
        let (input_handle, mut input_rx) = AgentInputHandle::channel();

        let error = input_handle
            .enqueue_input(AgentInput::FollowUp {
                from: None,
                kind: Some("child_report".to_string()),
                content: "half tagged".to_string(),
            })
            .expect_err("invalid input should be rejected before channel send");

        assert_eq!(
            error,
            AgentInputHandleError::Invalid(SessionInputError::Agent(
                agent_core::AgentInputError::UnpairedOriginTags
            ))
        );
        drop(input_handle);
        assert!(block_on_ready(next_input(&mut input_rx.inputs)).is_none());
    }
}
