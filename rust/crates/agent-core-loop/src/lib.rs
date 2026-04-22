#![forbid(unsafe_code)]

mod action;
#[path = "loop.rs"]
mod core_loop;
mod ids;
mod mailbox;
mod message;
mod state;
mod transcript;
mod transcript_record;

pub use crate::action::AgentAction;
pub use crate::core_loop::{AgentCoreLoop, AgentInput};
pub use crate::ids::{ToolCallId, TurnId};
pub use crate::mailbox::{Mailbox, MailboxEntry, MailboxNotification};
pub use crate::message::{
    AssistantItem, AssistantMessage, CompactMessage, ToolCall, ToolResultMessage, ToolResultStatus,
    UserInput, UserMessage,
};
pub use crate::state::AgentState;
pub use crate::transcript::Transcript;
pub use crate::transcript_record::{TranscriptRecord, TurnOutcome};
