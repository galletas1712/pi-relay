use crate::context::{Context, ContextError};

/// Operations that mutate a quiescent `Context`.
///
/// Each history-editing operation is its own struct (`Compact`, `Rewind`,
/// `ReplaceTranscript`) implementing this trait. The caller obtains the right
/// to edit via [`crate::AgentSession::edit`], which runs the quiescence check
/// once and then dispatches to [`ContextEdit::apply`] on the provided op.
///
/// `apply` takes `&mut Context` directly — op impls do not see the
/// `AgentSession`. Core-loop rehydration happens once in `AgentSession::edit`
/// after `apply` returns `Ok`, so each op only needs to worry about its own
/// context mutation and its own per-op preconditions.
pub trait ContextEdit {
    type Output;

    fn apply(self, ctx: &mut Context) -> Result<Self::Output, HistoryEditError>;
}

/// Caller-tracked work the session cannot observe (worklog forks, background
/// summarization calls, etc.). The session tracks its own in-flight model and
/// tool requests internally via the action queue, so those are not represented
/// here.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PendingWork {
    pub background_tasks: usize,
}

impl PendingWork {
    pub const NONE: Self = Self {
        background_tasks: 0,
    };

    pub fn is_empty(self) -> bool {
        self.background_tasks == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryEditError {
    /// The session cannot currently edit its history (core still running,
    /// durable leaf mid-turn, mailbox non-empty, or pending work outstanding).
    Busy,
    /// A transcript supplied to `ReplaceTranscript` did not itself end at a
    /// turn boundary.
    ReplacementNotAtTurnBoundary,
    /// An underlying context error: entry not found, not at a turn boundary,
    /// or a stale compaction plan.
    Context(ContextError),
}
