//! Durable session history and async runner atop the core FSM.
//!
//! `AgentSession` owns an `AgentCoreLoop` and a `TranscriptStore` (append-only
//! forest of entries with branch-aware navigation). It is the sole owner of
//! durable transcript items — every item flows from the core into the store via
//! `session.drive()`. History-edit operations are individual op structs
//! (`SummarizeSpan`, `Compact`, `Rewind`, `ReplaceModelContext`) that implement
//! the `HistoryEdit` trait; `session.edit(pending, op)` runs the quiescence
//! check once and dispatches to the op. `session.fork(pending, leaf)` stays as
//! a direct method because it produces a new session rather than mutating in
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
    AutoCompactionSettings, ImageInput, ModelContentBlock, StatelessModelOutput,
    StatelessModelOutputSpec, StatelessModelRequest,
};
pub use crate::event::{HistoryEditKind, SessionActionKind, SessionEvent};
pub use crate::input::{SessionInput, SessionInputError};
pub use crate::model_context::ModelContext;
pub use crate::runner::{AgentInputHandle, AgentInputHandleError, AgentInputReceiver, AgentRunner};
pub use crate::session::AgentSession;
pub use crate::transcript_store::{
    compaction_summary, Compact, CompactionPlan, CompactionSettings, HistoryEdit, HistoryEditError,
    PendingWork, ReplaceModelContext, Rewind, SummarizeSpan, SummarySpanPlan,
    TranscriptStorageNode, TranscriptStore, TranscriptStoreError, KIND_COMPACTION_SUMMARY,
};

// Re-export core-owned types so downstream callers have a single import home.
pub use agent_core::{
    ActionId, AgentAction, AgentInput, AgentInputError, AssistantItem, AssistantMessage,
    InjectedMessage, ToolCall, ToolCallId, ToolResultMessage, ToolResultStatus, TranscriptItem,
    TurnId, TurnOutcome,
};
