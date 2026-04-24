//! Durable session context and async runner atop the core FSM.
//!
//! `AgentSession` owns an `AgentCoreLoop` and a `Context` (append-only DAG
//! of entries with branch-aware navigation). It is the sole owner of durable
//! records — every transcript record flows from the core into the context via
//! `session.drive()`. History-edit operations are individual op structs
//! (`SummarizeSpan`, `Compact`, `Rewind`, `ReplaceTranscript`) that implement
//! the `ContextEdit` trait; `session.edit(pending, op)` runs the quiescence
//! check once and dispatches to the op. `session.fork(pending, leaf)` stays as
//! a direct method because it produces a new session rather than mutating in
//! place.
//! See `rust/docs/architecture.md`.

#![forbid(unsafe_code)]

mod action_queue;
mod context;
mod runner;
mod session;
mod transcript;

pub use crate::context::compaction::compaction_summary;
pub use crate::context::{
    Compact, CompactionPlan, CompactionSettings, Context, ContextEdit, ContextError,
    HistoryEditError, PendingWork, ReplaceTranscript, Rewind, SessionEntry, SummarizeSpan,
    SummarySpanPlan, KIND_COMPACTION_SUMMARY,
};
pub use crate::runner::{AgentInputHandle, AgentInputHandleError, AgentInputReceiver, AgentRunner};
pub use crate::session::AgentSession;
pub use crate::transcript::Transcript;

// Re-export core-owned types so downstream callers have a single import home.
pub use agent_core::{
    ActionId, AgentAction, AgentInput, AgentInputError, CustomMessage, ToolCallId,
    TranscriptRecord, TurnId, TurnOutcome,
};
