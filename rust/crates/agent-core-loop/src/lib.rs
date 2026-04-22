#![forbid(unsafe_code)]

#[path = "loop.rs"]
mod core_loop;
mod event;
mod ids;
mod mailbox;
mod message;

pub use crate::core_loop::{AgentCoreLoop, AgentInput, AgentNotification, CoreTransition, Phase};
pub use crate::event::{AgentAction, AgentEvent};
pub use crate::ids::{EventId, TurnId};
pub use crate::mailbox::{Mailbox, MailboxCommand};
pub use crate::message::{
    AssistantItem, AssistantMessage, CoreMessage, ToolCall, ToolResultMessage, ToolResultStatus,
    UserInput, UserMessage,
};
