use crate::transcript_store::{TranscriptStore, TranscriptStoreError};

/// Operations that mutate a quiescent `TranscriptStore`.
///
/// Each history-editing operation is its own struct (`SummarizeSpan`,
/// `Compact`, `Rewind`, `ReplaceModelContext`) implementing this trait. The
/// caller obtains the right to edit via [`crate::AgentSession::edit`], which
/// runs the quiescence check once and then dispatches to
/// [`HistoryEdit::apply`] on the provided op.
///
/// `apply` takes `&mut TranscriptStore` directly — op impls do not see the
/// `AgentSession`. Core-loop rehydration happens once in `AgentSession::edit`
/// after `apply` returns `Ok`, so each op only needs to worry about its own
/// context mutation and its own per-op preconditions.
pub trait HistoryEdit {
    type Output;
    const KIND: HistoryEditKind;

    fn apply(self, ctx: &mut TranscriptStore) -> Result<Self::Output, HistoryEditError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryEditKind {
    SummarizeSpan,
    Compact,
    Rewind,
    ReplaceModelContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryEditError {
    /// The session cannot currently edit its history (core still running,
    /// durable leaf mid-turn, mailbox non-empty, undrained actions remain, or
    /// session-owned maintenance is outstanding).
    Busy,
    /// A model context supplied to `ReplaceModelContext` did not itself end at
    /// a turn boundary.
    ReplacementNotAtTurnBoundary,
    /// An underlying transcript-store error: entry not found, invalid summary
    /// span, not at a turn boundary, or a stale edit plan.
    Store(TranscriptStoreError),
}
