use agent_core::TranscriptItem;

use crate::action::SessionAction;

/// Ephemeral session activity for live observers.
///
/// These events are intentionally not transcript-store entries. `ModelContext`
/// remains the model-visible view; events explain what the session is doing
/// around that view while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    TranscriptItemAppended {
        entry_id: String,
        item: TranscriptItem,
    },
    ActionRequested {
        action: SessionAction,
    },
    ActionCompleted {
        kind: SessionActionKind,
        id: String,
    },
    ActionFailed {
        kind: SessionActionKind,
        id: String,
        error: String,
    },
    HistoryEdited {
        kind: HistoryEditKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActionKind {
    Model,
    Tool,
    TurnCancellation,
    ModelStateless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryEditKind {
    HistoryEdit,
    Compact,
}
