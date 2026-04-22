#![forbid(unsafe_code)]

#[path = "loop.rs"]
mod core_loop;
mod event;
mod ids;
mod mailbox;
mod message;

pub use crate::core_loop::{AgentCoreLoop, CoreTransition, LoopSignal, Phase};
pub use crate::event::LoopAction;
pub use crate::ids::{Epoch, MessageId, ToolCallId};
pub use crate::mailbox::{Mailbox, MailboxCommand};
pub use crate::message::{
    AssistantItem, AssistantMessage, CoreMessage, ToolCall, ToolResultMessage, ToolResultStatus,
    UserInput, UserMessage,
};
