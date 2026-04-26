use agent_core::{ActionId, AgentAction, TranscriptItem, TurnId};

use crate::action::CompactionRequestId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompactionState {
    Idle,
    Requested,
    Running {
        request_id: CompactionRequestId,
        blocked_model_request: Option<CompactionBarrierModelRequest>,
        requested_again: bool,
    },
}

impl CompactionState {
    pub(crate) fn request(&mut self) {
        match self {
            Self::Idle => *self = Self::Requested,
            Self::Requested => {}
            Self::Running {
                requested_again, ..
            } => *requested_again = true,
        }
    }

    pub(crate) fn is_requested(&self) -> bool {
        matches!(self, Self::Requested)
    }

    pub(crate) fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    pub(crate) fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::Idle;
    }

    pub(crate) fn start(
        &mut self,
        request_id: CompactionRequestId,
        blocked_model_request: Option<CompactionBarrierModelRequest>,
    ) {
        *self = Self::Running {
            request_id,
            blocked_model_request,
            requested_again: false,
        };
    }

    pub(crate) fn take_running(
        &mut self,
        request_id: CompactionRequestId,
    ) -> Option<RunningCompaction> {
        if !matches!(self, Self::Running { request_id: active, .. } if *active == request_id) {
            return None;
        }

        let Self::Running {
            request_id,
            blocked_model_request,
            requested_again,
        } = std::mem::replace(self, Self::Idle)
        else {
            unreachable!("checked running compaction above");
        };
        if requested_again {
            *self = Self::Requested;
        }
        Some(RunningCompaction {
            request_id,
            blocked_model_request,
        })
    }

    pub(crate) fn abandon(&mut self) -> Option<RunningCompaction> {
        match std::mem::replace(self, Self::Idle) {
            Self::Running {
                request_id,
                blocked_model_request,
                ..
            } => Some(RunningCompaction {
                request_id,
                blocked_model_request,
            }),
            Self::Idle | Self::Requested => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningCompaction {
    pub(crate) request_id: CompactionRequestId,
    pub(crate) blocked_model_request: Option<CompactionBarrierModelRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactionBarrierModelRequest {
    action_id: ActionId,
    turn_id: TurnId,
    required_turn_suffix: Vec<TranscriptItem>,
}

impl CompactionBarrierModelRequest {
    pub(crate) fn new(
        action_id: ActionId,
        turn_id: TurnId,
        required_turn_suffix: Vec<TranscriptItem>,
    ) -> Self {
        Self {
            action_id,
            turn_id,
            required_turn_suffix,
        }
    }

    pub(crate) fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub(crate) fn required_turn_suffix(&self) -> &[TranscriptItem] {
        &self.required_turn_suffix
    }

    pub(crate) fn into_agent_action(self) -> AgentAction {
        AgentAction::RequestModel {
            action_id: self.action_id,
            turn_id: self.turn_id,
        }
    }
}
