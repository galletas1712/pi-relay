#![forbid(unsafe_code)]

mod action;
#[path = "loop.rs"]
mod core_loop;
mod event;
mod ids;
mod mailbox;
mod message;
mod record;
mod runner;
mod state;

pub use crate::action::AgentAction;
pub use crate::core_loop::AgentCoreLoop;
pub use crate::event::AgentInput;
pub use crate::ids::{ToolCallId, TurnId};
pub use crate::message::{
    AssistantItem, AssistantMessage, ToolCall, ToolResultMessage, ToolResultStatus,
};
pub use crate::record::{TranscriptRecord, TurnOutcome};
pub use crate::runner::{AgentInputHandle, AgentInputReceiver, AgentRunner};

// `AgentState` and `Mailbox` are intentionally not re-exported: they are
// implementation details of the core loop. Callers observe liveness via
// `AgentCoreLoop::is_idle` and `AgentCoreLoop::has_pending_work`.
