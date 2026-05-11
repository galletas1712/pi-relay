//! Durable session history atop the core FSM.
//!
//! `AgentSession` owns an `AgentCoreLoop` and a `TranscriptStore` (append-only
//! forest of entries with branch-aware navigation). It is the sole owner of
//! durable transcript items — every item flows from the core into the store via
//! `session.drive()`. The session exposes `compact` for remote compaction,
//! `rewind` for immediate history navigation, and `fork(entry)` for creating a
//! new unregistered session from any existing transcript entry.
//! See `rust/docs/architecture.md`.

#![forbid(unsafe_code)]

mod action;
mod compaction_state;
mod event;
mod external_work;
mod input;
mod model_context;
mod session;
mod storage;
mod transcript_store;

pub use crate::action::{CompactionRequestId, SessionAction};
pub use crate::event::{SessionActionKind, SessionEvent};
pub use crate::input::{SessionInput, SessionInputError};
pub use crate::model_context::ModelContext;
pub use crate::session::{AgentSession, HistoryOperationError};
pub use crate::storage::{StoredSession, StoredTranscriptEntry};
pub use crate::transcript_store::{TranscriptStorageNode, TranscriptStore, TranscriptStoreError};

// Re-export core-owned types so downstream callers have a single import home.
pub use agent_core::{
    ActionId, AgentAction, AgentInput, AgentInputError, AssistantItem, AssistantMessage,
    InjectedMessage, ToolCall, ToolCallId, ToolResultMessage, ToolResultStatus, TranscriptItem,
    TurnId, TurnOutcome, UserMessage,
};
