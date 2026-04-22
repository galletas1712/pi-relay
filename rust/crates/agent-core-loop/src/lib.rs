#![forbid(unsafe_code)]

#[path = "loop.rs"]
mod core_loop;
mod event;
mod ids;
mod mailbox;
mod message;
mod transcript;

pub use crate::core_loop::{AgentCoreLoop, AgentInput, Phase};
pub use crate::event::{AgentAction, AgentEvent, TurnOutcome};
pub use crate::ids::{ToolCallId, TurnId};
pub use crate::mailbox::{Mailbox, MailboxEntry, MailboxEvent};
pub use crate::message::{
    AssistantItem, AssistantMessage, CompactMessage, ToolCall, ToolResultMessage, ToolResultStatus,
    UserInput, UserMessage,
};
pub use crate::transcript::Transcript;
