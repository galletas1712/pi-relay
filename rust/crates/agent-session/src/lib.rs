//! Durable session history atop the core FSM.
//!
//! `AgentSession` owns an `AgentCoreLoop` and a `TranscriptStore` (append-only
//! forest of entries with branch-aware navigation). It is the sole owner of
//! durable transcript items — every item flows from the core into the store via
//! `session.drive()`. The session exposes `rewind` for immediate history
//! navigation and `fork(boundary)` for creating a new unregistered session from
//! a turn boundary.
//! See `rust/docs/architecture.md`.

#![forbid(unsafe_code)]

mod action;
mod event;
mod input;
mod model_context;
mod outstanding_actions;
mod session;
mod storage;
mod transcript_store;

pub use crate::action::SessionAction;
pub use crate::event::{SessionActionKind, SessionEvent};
pub use crate::input::{SessionInput, SessionInputError};
pub use crate::model_context::{ModelContext, ModelContextEntry};
pub use crate::session::{
    AgentSession, CompactionCheckpoint, HistoryOperationError, InstalledCompaction,
};
pub use crate::storage::{StoredSession, StoredTranscriptEntry};
pub use crate::transcript_store::{TranscriptStorageNode, TranscriptStore, TranscriptStoreError};
