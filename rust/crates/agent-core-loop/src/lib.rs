#![forbid(unsafe_code)]

mod action;
#[path = "loop.rs"]
mod core_loop;
mod event;
mod ids;
mod mailbox;
mod message;
mod runner;
mod state;
mod transcript;
mod transcript_record;

pub use crate::action::AgentAction;
pub use crate::core_loop::AgentCoreLoop;
pub use crate::event::AgentInput;
pub use crate::ids::{ToolCallId, TurnId};
pub use crate::mailbox::Mailbox;
pub use crate::message::{
    AssistantItem, AssistantMessage, CompactMessage, ToolCall, ToolResultMessage, ToolResultStatus,
};
pub use crate::runner::{AgentInputHandle, AgentInputReceiver, AgentRunner};
pub use crate::state::AgentState;
pub use crate::transcript::Transcript;
pub use crate::transcript_record::{TranscriptRecord, TurnOutcome};
