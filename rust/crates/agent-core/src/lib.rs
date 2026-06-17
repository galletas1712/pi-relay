//! Deterministic FSM kernel for agent turns.
//!
//! Accepts `AgentInput` on a priority mailbox and produces two drained
//! outputs per transition: `TranscriptItem`s (durable model-visible items) and
//! `AgentAction`s (requests for the outside world to perform — model calls,
//! tool executions, cancellations). No I/O; internals are
//! private. See `rust/docs/architecture.md` for the full layer stack.

#![forbid(unsafe_code)]

mod action;
#[path = "loop.rs"]
mod core_loop;
mod event;
mod mailbox;
mod state;

pub use crate::action::AgentAction;
pub use crate::core_loop::AgentCoreLoop;
pub use crate::event::{AgentInput, TurnInput};

// `AgentState` and `Mailbox` are intentionally not re-exported: they are
// implementation details of the core loop. Callers observe liveness via
// `AgentCoreLoop::is_idle`.
