//! Durable session context and async runner atop the core FSM.
//!
//! `AgentSession` owns an `AgentCoreLoop` and a `Context` (append-only DAG
//! of entries with branch-aware navigation). It is the sole owner of durable
//! records — every transcript record flows from the core into the context via
//! `session.drive()`. History-edit operations (compact, rewind, fork,
//! replace_transcript) live behind `ContextEdit<'_>`, obtained via
//! `session.edit_history(pending)?`. See `rust/docs/architecture.md`.

#![forbid(unsafe_code)]

mod action_queue;
mod context;
mod fork;
mod runner;
mod session;
mod transcript;

pub use crate::context::{
    branch_summary, compaction_summary, Context, ContextEdit, ContextError, HistoryEditError,
    PendingWork, SessionEntry, KIND_BRANCH_SUMMARY, KIND_COMPACTION_SUMMARY,
};
pub use crate::fork::{CompactionPlan, CompactionSettings};
pub use crate::runner::{AgentInputHandle, AgentInputReceiver, AgentRunner};
pub use crate::session::AgentSession;
pub use crate::transcript::Transcript;

// Re-export core-owned types so downstream callers have a single import home.
pub use agent_core::{
    AgentAction, AgentInput, CustomMessage, TranscriptRecord, TurnId, TurnOutcome,
};
