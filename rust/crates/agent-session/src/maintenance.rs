use agent_core::AgentAction;

use crate::action::StatelessModelRequestId;
use crate::transcript_store::{CompactionPlan, CompactionSettings};

/// Session-owned history maintenance to run at the next safe model-context
/// barrier.
///
/// Unlike [`crate::AgentSession::edit`], maintenance can be requested while a
/// session is busy. The session applies it only when doing so cannot invalidate
/// already-exposed model/tool work: either while idle at a turn boundary, or
/// after the core has requested a model call but before that request is exposed
/// to the harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMaintenance {
    Compact { settings: CompactionSettings },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueuedSessionMaintenance {
    pub(crate) maintenance: SessionMaintenance,
    pub(crate) source: MaintenanceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceSource {
    Requested,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingSessionMaintenance {
    pub(crate) request_id: StatelessModelRequestId,
    pub(crate) kind: PendingSessionMaintenanceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingSessionMaintenanceKind {
    Compact {
        plan: CompactionPlan,
        held_action: Option<AgentAction>,
        source: MaintenanceSource,
    },
}
