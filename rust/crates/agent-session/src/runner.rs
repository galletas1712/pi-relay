use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures_core::Stream;

use agent_core::{AgentAction, AgentInput};

use crate::AgentSession;

#[derive(Debug, Clone)]
pub struct AgentInputHandle {
    inputs: UnboundedSender<AgentInput>,
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

    pub fn enqueue_input(&self, input: AgentInput) -> Result<(), AgentInput> {
        self.inputs
            .unbounded_send(input)
            .map_err(|error| error.into_inner())
    }
}

/// Receive side of the agent input queue.
pub struct AgentInputReceiver {
    inputs: UnboundedReceiver<AgentInput>,
}

/// Async integration shell around `AgentSession`.
///
/// AgentRunner owns the proactive run loop: it receives inputs, drives the
/// session until quiescent, and forwards any requested actions to the
/// registered handler. Records flow automatically into the session log via
/// `AgentSession::drive`, so callers observing durable history read it off
/// the session's transcript rather than through a record callback.
pub struct AgentRunner<HandleAction> {
    session: AgentSession,
    inputs: AgentInputReceiver,
    handle_action: HandleAction,
}

impl<HandleAction> AgentRunner<HandleAction> {
    pub fn new(
        session: AgentSession,
        inputs: AgentInputReceiver,
        handle_action: HandleAction,
    ) -> Self {
        Self {
            session,
            inputs,
            handle_action,
        }
    }

    pub fn session(&self) -> &AgentSession {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut AgentSession {
        &mut self.session
    }
}

impl<HandleAction, HandleActionFuture> AgentRunner<HandleAction>
where
    HandleAction: FnMut(AgentAction) -> HandleActionFuture,
    HandleActionFuture: Future<Output = ()>,
{
    pub async fn run(&mut self) {
        self.drive_and_flush_actions().await;

        while let Some(input) = next_input(&mut self.inputs.inputs).await {
            self.session.enqueue_input(input);
            self.drive_and_flush_actions().await;
        }
    }

    async fn drive_and_flush_actions(&mut self) {
        self.session.drive();
        for action in self.session.drain_actions() {
            (self.handle_action)(action).await;
        }
    }
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
    use std::cell::RefCell;
    use std::future::{ready, Future};
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    use agent_core::{AssistantItem, AssistantMessage, TranscriptRecord, TurnId, TurnOutcome};

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
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            })
            .expect("runner should accept model completion");
        drop(input_handle);

        block_on_ready(runner.run());

        assert_eq!(
            actions.borrow().as_slice(),
            &[AgentAction::RequestModel { turn_id: TurnId(1) }]
        );
        assert_eq!(
            runner.session().transcript().records(),
            &[
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage("hello".to_string()),
                TranscriptRecord::AssistantMessage(assistant),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
            ]
        );
        assert!(runner.session().is_idle());
    }
}
