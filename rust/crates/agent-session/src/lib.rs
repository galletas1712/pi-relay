#![forbid(unsafe_code)]

mod runner;

use agent_core::{
    AgentAction, AgentCoreLoop, AgentInput, AgentState, Transcript, TranscriptCheckpoint,
    TranscriptRecord,
};

pub use crate::runner::{AgentInputHandle, AgentInputReceiver, AgentRunner};

/// Session shell around the pure core loop.
///
/// `agent-core` owns deterministic state transitions. `agent-session` owns the
/// boundary where durable transcript state can be safely replaced, forked,
/// rewound, or resumed after consulting external model/tool work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    core: AgentCoreLoop,
}

impl Default for AgentSession {
    fn default() -> Self {
        Self {
            core: AgentCoreLoop::new(),
        }
    }
}

impl AgentSession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_records(records: Vec<TranscriptRecord>) -> Self {
        Self::from_core(AgentCoreLoop::from_records(records))
    }

    pub fn from_transcript(transcript: Transcript) -> Self {
        Self::from_core(AgentCoreLoop::from_transcript(transcript))
    }

    pub fn from_core(core: AgentCoreLoop) -> Self {
        Self { core }
    }

    pub fn core(&self) -> &AgentCoreLoop {
        &self.core
    }

    pub fn core_mut(&mut self) -> &mut AgentCoreLoop {
        &mut self.core
    }

    pub fn transcript(&self) -> &Transcript {
        &self.core.transcript
    }

    pub fn checkpoint(&self) -> TranscriptCheckpoint {
        self.core.transcript.checkpoint()
    }

    pub fn boundary_checkpoint(&self) -> Option<TranscriptCheckpoint> {
        self.core.transcript.boundary_checkpoint()
    }

    pub fn enqueue_input(&mut self, input: AgentInput) {
        self.core.enqueue_input(input);
    }

    pub fn drive(&mut self) {
        self.core.drive();
    }

    pub fn drain_actions(&mut self) -> Vec<AgentAction> {
        self.core.drain_actions()
    }

    pub fn handle_input(&mut self, input: AgentInput) -> Vec<AgentAction> {
        self.enqueue_input(input);
        self.drive();
        self.drain_actions()
    }

    pub fn quiescence(&self, external_work: ExternalWork) -> SessionQuiescence {
        SessionQuiescence {
            core_idle: self.core.state == AgentState::Idle,
            durable_turn_boundary: self.core.transcript.is_turn_boundary(),
            mailbox_empty: self.core.mailbox.is_empty(),
            action_outbox_empty: !self.core.has_pending_actions(),
            external_work_empty: external_work.is_empty(),
        }
    }

    pub fn is_quiescent(&self, external_work: ExternalWork) -> bool {
        self.quiescence(external_work).is_quiescent()
    }

    /// Replace the durable transcript only at a fully quiescent session boundary.
    ///
    /// This is the primitive session compaction should use after it has produced
    /// a provider-safe replacement transcript. The mailbox and action outbox are
    /// required to be empty so volatile queued work is not silently migrated.
    pub fn replace_transcript_at_boundary(
        &mut self,
        replacement: Transcript,
        external_work: ExternalWork,
    ) -> Result<Transcript, SessionBoundaryError> {
        let quiescence = self.quiescence(external_work);
        if !quiescence.is_quiescent() {
            return Err(SessionBoundaryError::Busy(quiescence));
        }
        if !replacement.is_turn_boundary() {
            return Err(SessionBoundaryError::ReplacementNotAtBoundary);
        }

        let previous =
            std::mem::replace(&mut self.core, AgentCoreLoop::from_transcript(replacement));
        Ok(previous.transcript)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExternalWork {
    pub model_requests: usize,
    pub tool_requests: usize,
    pub background_tasks: usize,
}

impl ExternalWork {
    pub const NONE: Self = Self {
        model_requests: 0,
        tool_requests: 0,
        background_tasks: 0,
    };

    pub fn is_empty(self) -> bool {
        self.model_requests == 0 && self.tool_requests == 0 && self.background_tasks == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionQuiescence {
    pub core_idle: bool,
    pub durable_turn_boundary: bool,
    pub mailbox_empty: bool,
    pub action_outbox_empty: bool,
    pub external_work_empty: bool,
}

impl SessionQuiescence {
    pub fn is_quiescent(self) -> bool {
        self.core_idle
            && self.durable_turn_boundary
            && self.mailbox_empty
            && self.action_outbox_empty
            && self.external_work_empty
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBoundaryError {
    Busy(SessionQuiescence),
    ReplacementNotAtBoundary,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AssistantItem, AssistantMessage, TurnId, TurnOutcome};

    fn finished_transcript(input: &str) -> Transcript {
        Transcript::from_records_raw(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage(input.to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ])
    }

    #[test]
    fn quiescence_requires_idle_core_empty_queues_and_no_external_work() {
        let mut session = AgentSession::new();

        let actions = session.handle_input(AgentInput::FollowUp("hello".to_string()));

        assert_eq!(
            actions,
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );
        assert!(!session.is_quiescent(ExternalWork::NONE));
        assert!(!session
            .quiescence(ExternalWork {
                model_requests: 1,
                ..ExternalWork::NONE
            })
            .is_quiescent());

        let actions = session.handle_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: AssistantMessage {
                items: vec![AssistantItem::Text("hi".to_string())],
            },
        });

        assert!(actions.is_empty());
        assert!(session.is_quiescent(ExternalWork::NONE));
    }

    #[test]
    fn transcript_replacement_is_only_allowed_at_quiescent_boundaries() {
        let mut session = AgentSession::new();
        session.handle_input(AgentInput::FollowUp("hello".to_string()));

        let busy = session
            .replace_transcript_at_boundary(finished_transcript("compact"), ExternalWork::NONE)
            .expect_err("running sessions cannot be compacted");
        assert!(matches!(busy, SessionBoundaryError::Busy(_)));

        session.handle_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        });

        let old = session
            .replace_transcript_at_boundary(finished_transcript("compact"), ExternalWork::NONE)
            .expect("idle boundary can swap transcript");

        assert_eq!(old.last_turn_id(), TurnId(1));
        assert_eq!(
            session.transcript().records()[1],
            TranscriptRecord::UserMessage("compact".to_string())
        );
    }
}
