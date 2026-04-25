//! Durable session context and async runner atop the core FSM.
//!
//! `AgentSession` owns an `AgentCoreLoop` and a `TranscriptStore` (append-only
//! forest of entries with branch-aware navigation). It is the sole owner of
//! durable context items — every item flows from the core into the store via
//! `session.drive()`. `Context`, `SessionEntry`, `TranscriptRecord`, and
//! `Transcript` remain as compatibility names while callers move toward the
//! `TranscriptStore` / `TranscriptEntry` / `ContextItem` / `ModelContext`
//! vocabulary. History-edit operations are individual op structs
//! (`SummarizeSpan`, `Compact`, `Rewind`, `ReplaceTranscript`) that implement
//! the `ContextEdit` trait; `session.edit(pending, op)` runs the quiescence
//! check once and dispatches to the op. `session.fork(pending, leaf)` stays as
//! a direct method because it produces a new session rather than mutating in
//! place.
//! See `rust/docs/architecture.md`.

#![forbid(unsafe_code)]

mod action;
mod action_queue;
mod auto_compaction;
mod context;
mod event;
mod input;
mod runner;
mod session;
mod transcript;

pub use crate::action::{SessionAction, StatelessModelRequestId};
pub use crate::auto_compaction::{
    AutoCompactionSettings, ImageInput, ModelContentBlock, StatelessModelOutput,
    StatelessModelOutputSpec, StatelessModelRequest,
};
pub use crate::context::{
    compaction_summary, Compact, CompactionPlan, CompactionSettings, Context, ContextEdit,
    ContextError, HistoryEditError, PendingWork, ReplaceTranscript, Rewind, SessionEntry,
    SummarizeSpan, SummarySpanPlan, TranscriptEntry, TranscriptStore, KIND_COMPACTION_SUMMARY,
};
pub use crate::event::{ContextEditKind, SessionActionKind, SessionEvent};
pub use crate::input::{SessionInput, SessionInputError};
pub use crate::runner::{AgentInputHandle, AgentInputHandleError, AgentInputReceiver, AgentRunner};
pub use crate::session::AgentSession;
pub use crate::transcript::{ModelContext, Transcript};

// Re-export core-owned types so downstream callers have a single import home.
pub use agent_core::{
    ActionId, AgentAction, AgentInput, AgentInputError, AssistantItem, AssistantMessage,
    ContextItem, InjectedMessage, ToolCall, ToolCallId, ToolResultMessage, ToolResultStatus,
    TranscriptRecord, TurnId, TurnOutcome,
};
