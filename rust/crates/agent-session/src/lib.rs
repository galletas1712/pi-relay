//! Durable session history and async runner atop the core FSM.
//!
//! `AgentSession` owns an `AgentCoreLoop` and a `SessionLog` (append-only DAG
//! of entries with branch-aware navigation). It is the sole owner of durable
//! records — every transcript record flows from the core into the log via
//! `session.drive()`. History-edit operations (compact, rewind, fork,
//! replace_transcript) live behind `SessionHistoryEdit<'_>`, obtained via
//! `session.edit_history(pending)?`. See `rust/docs/architecture.md`.

#![forbid(unsafe_code)]

mod history_edit;
mod pending_actions;
mod runner;
mod session;
mod session_log;
mod transcript;

pub use crate::history_edit::{HistoryEditError, PendingWork, SessionHistoryEdit};
pub use crate::runner::{AgentInputHandle, AgentInputReceiver, AgentRunner};
pub use crate::session::AgentSession;
pub use crate::session_log::{
    branch_summary, compaction_summary, CompactionPlan, CompactionSettings, SessionEntry,
    SessionLog, SessionLogError, KIND_BRANCH_SUMMARY, KIND_COMPACTION_SUMMARY,
};
pub use crate::transcript::Transcript;

// Re-export core-owned types so downstream callers have a single import home.
pub use agent_core::{
    AgentAction, AgentInput, CustomMessage, TranscriptRecord, TurnId, TurnOutcome,
};
