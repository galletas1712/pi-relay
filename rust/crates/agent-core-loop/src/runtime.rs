use std::collections::VecDeque;

use crate::event::AgentAction;
use crate::ids::TurnId;

// Small volatile runtime primitives. These are deliberately not persisted:
// crash recovery rebuilds from Transcript and drops queued mailbox work.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ActionOutbox {
    actions: VecDeque<AgentAction>,
}

impl ActionOutbox {
    pub(crate) fn push(&mut self, action: AgentAction) {
        self.actions.push_back(action);
    }

    pub(crate) fn drain(&mut self) -> Vec<AgentAction> {
        self.actions.drain(..).collect()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InterruptLatch {
    requested: bool,
}

impl InterruptLatch {
    pub(crate) fn request(&mut self) {
        self.requested = true;
    }

    pub(crate) fn take(&mut self) -> bool {
        let requested = self.requested;
        self.requested = false;
        requested
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnCursor {
    last_turn_id: TurnId,
}

impl TurnCursor {
    pub(crate) fn from_last(last_turn_id: TurnId) -> Self {
        Self { last_turn_id }
    }

    pub(crate) fn last(self) -> TurnId {
        self.last_turn_id
    }

    pub(crate) fn allocate(&mut self) -> TurnId {
        self.last_turn_id = self.last_turn_id.next();
        self.last_turn_id
    }
}
