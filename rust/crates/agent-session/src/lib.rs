//! Durable session history and async runner atop the core FSM.
//!
//! `AgentSession` owns an `AgentCoreLoop` and a `TranscriptStore` (append-only
//! forest of entries with branch-aware navigation). It is the sole owner of
//! durable transcript items — every item flows from the core into the store via
//! `session.drive()`. The session exposes two immediate history edits,
//! `compact` and `rewind`, plus `request_compaction` for scheduled compaction
//! at the next safe model-context barrier. `session.fork(leaf)` stays as a
//! direct method because it produces a new session rather than mutating in
//! place.
//! See `rust/docs/architecture.md`.

#![forbid(unsafe_code)]

mod action;
mod action_queue;
mod auto_compaction;
mod event;
mod input;
mod model_context;
mod runner;
mod session;
mod transcript_store;

pub use crate::action::{SessionAction, StatelessModelRequestId};
pub use crate::auto_compaction::{
    AutoCompactionSettings, ImageInput, ModelContentBlock, StatelessModelRequest,
};
pub use crate::event::{SessionActionKind, SessionEvent};
pub use crate::input::{SessionInput, SessionInputError};
pub use crate::model_context::ModelContext;
pub use crate::runner::{AgentInputHandle, AgentInputHandleError, AgentInputReceiver, AgentRunner};
pub use crate::session::{AgentSession, HistoryOperationError};
pub use crate::transcript_store::{
    compaction_summary, CompactionPlan, CompactionSettings, TranscriptStorageNode, TranscriptStore,
    TranscriptStoreError, KIND_COMPACTION_SUMMARY,
};

// Re-export core-owned types so downstream callers have a single import home.
pub use agent_core::{
    ActionId, AgentAction, AgentInput, AgentInputError, AssistantItem, AssistantMessage,
    InjectedMessage, ToolCall, ToolCallId, ToolResultMessage, ToolResultStatus, TranscriptItem,
    TurnId, TurnOutcome,
};
