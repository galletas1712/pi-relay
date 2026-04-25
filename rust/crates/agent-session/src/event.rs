use agent_core::TranscriptRecord;

use crate::action::SessionAction;

/// Ephemeral session activity for live observers.
///
/// These events are intentionally not part of the transcript. The transcript
/// remains the model-visible durable context; events explain what the session
/// is doing around that context while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    RecordAppended {
        entry_id: String,
        record: TranscriptRecord,
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
    ContextEdited {
        kind: ContextEditKind,
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
pub enum ContextEditKind {
    HistoryEdit,
    Compact,
}
