use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures_core::Stream;

use crate::action::AgentAction;
use crate::core_loop::AgentCoreLoop;
use crate::event::AgentInput;
use crate::record::TranscriptRecord;

#[derive(Debug, Clone)]
pub struct AgentInputHandle {
    inputs: UnboundedSender<AgentInput>,
}

impl AgentInputHandle {
    /// Create the input side of an agent run loop.
    ///
    /// The handle is cloneable so orchestrator, model, and tool tasks can all
    /// enqueue completions or user input back into the same core loop.
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

/// Async integration shell around the pure AgentCoreLoop.
///
/// AgentRunner owns the proactive run loop: it receives inputs, drives the
/// core until quiescent, and forwards produced records and requested actions
/// to the registered handlers. Records flow first so observers see durable
/// transcript updates before any side effects are triggered.
pub struct AgentRunner<HandleRecord, HandleAction> {
    core: AgentCoreLoop,
    inputs: AgentInputReceiver,
    handle_record: HandleRecord,
    handle_action: HandleAction,
}

impl<HandleRecord, HandleAction> AgentRunner<HandleRecord, HandleAction> {
    pub fn new(
        core: AgentCoreLoop,
        inputs: AgentInputReceiver,
        handle_record: HandleRecord,
        handle_action: HandleAction,
    ) -> Self {
        Self {
            core,
            inputs,
            handle_record,
            handle_action,
        }
    }

    pub fn core(&self) -> &AgentCoreLoop {
        &self.core
    }

    pub fn core_mut(&mut self) -> &mut AgentCoreLoop {
        &mut self.core
    }
}

impl<HandleRecord, HandleRecordFuture, HandleAction, HandleActionFuture>
    AgentRunner<HandleRecord, HandleAction>
where
    HandleRecord: FnMut(TranscriptRecord) -> HandleRecordFuture,
    HandleRecordFuture: Future<Output = ()>,
    HandleAction: FnMut(AgentAction) -> HandleActionFuture,
    HandleActionFuture: Future<Output = ()>,
{
    pub async fn run(&mut self) {
        self.drive_and_flush().await;

        while let Some(input) = next_input(&mut self.inputs.inputs).await {
            self.core.enqueue_input(input);
            self.drive_and_flush().await;
        }
    }

    async fn drive_and_flush(&mut self) {
        self.core.drive();

        for record in self.core.drain_records() {
            (self.handle_record)(record).await;
        }
        for action in self.core.drain_actions() {
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

    use crate::ids::TurnId;
    use crate::message::{AssistantItem, AssistantMessage};
    use crate::record::TurnOutcome;
    use crate::state::AgentState;

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
        let records = Rc::new(RefCell::new(Vec::new()));
        let recorded_records = records.clone();
        let actions = Rc::new(RefCell::new(Vec::new()));
        let recorded_actions = actions.clone();
        let assistant = AssistantMessage {
            items: vec![AssistantItem::Text("hi".to_string())],
        };
        let (input_handle, input_rx) = AgentInputHandle::channel();
        let mut runner = AgentRunner::new(
            AgentCoreLoop::new(),
            input_rx,
            move |record| {
                recorded_records.borrow_mut().push(record);
                ready(())
            },
            move |action| {
                recorded_actions.borrow_mut().push(action);
                ready(())
            },
        );

        input_handle
            .enqueue_input(AgentInput::FollowUp("hello".to_string()))
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
            records.borrow().as_slice(),
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
        assert_eq!(runner.core().state, AgentState::Idle);
    }
}
