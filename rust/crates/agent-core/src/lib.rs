//! Deterministic FSM kernel for agent turns.
//!
//! Accepts `AgentInput` on a priority mailbox and produces two drained
//! outputs per transition: `ContextItem`s (durable model-visible items) and
//! `AgentAction`s (requests for the outside world to perform — model calls,
//! tool executions, cancellations). No I/O; internals are
//! private. See `rust/docs/architecture.md` for the full layer stack.

#![forbid(unsafe_code)]

mod action;
mod context_item;
#[path = "loop.rs"]
mod core_loop;
mod event;
mod ids;
mod mailbox;
mod message;
mod state;

pub use crate::action::AgentAction;
pub use crate::context_item::{ContextItem, InjectedMessage, TurnOutcome};
pub use crate::core_loop::AgentCoreLoop;
pub use crate::event::{AgentInput, AgentInputError};
pub use crate::ids::{ActionId, ToolCallId, TurnId};
pub use crate::message::{
    AssistantItem, AssistantMessage, ToolCall, ToolResultMessage, ToolResultStatus,
};

// `AgentState` and `Mailbox` are intentionally not re-exported: they are
// implementation details of the core loop. Callers observe liveness via
// `AgentCoreLoop::is_idle` and `AgentCoreLoop::has_pending_work`.
