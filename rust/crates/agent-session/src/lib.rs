#![forbid(unsafe_code)]

mod runner;
mod session_log;

use agent_core::{
    AgentAction, AgentCoreLoop, AgentInput, AgentState, Transcript, TranscriptCheckpoint,
    TranscriptRecord,
};

pub use crate::runner::{AgentInputHandle, AgentInputReceiver, AgentRunner};
pub use crate::session_log::{
    BranchSummaryEntry, CompactionEntry, CompactionPlan, CompactionSettings, SessionContext,
    SessionEntry, SessionEntryKind, SessionLog, SessionLogError,
};

/// Session shell around the pure core loop.
///
/// `agent-core` owns deterministic state transitions. `agent-session` owns the
/// boundary where durable transcript state can be safely replaced, forked,
/// rewound, or resumed after consulting external model/tool work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    core: AgentCoreLoop,
    log: SessionLog,
    synced_checkpoint: TranscriptCheckpoint,
}

impl Default for AgentSession {
    fn default() -> Self {
        let core = AgentCoreLoop::new();
        let synced_checkpoint = core.transcript.checkpoint();
        Self {
            core,
            log: SessionLog::new(),
            synced_checkpoint,
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
        let log = SessionLog::from_transcript(&core.transcript);
        let synced_checkpoint = core.transcript.checkpoint();
        Self {
            core,
            log,
            synced_checkpoint,
        }
    }

    pub fn from_session_log(log: SessionLog) -> Result<Self, SessionBoundaryError> {
        if !log.is_turn_boundary() {
            return Err(SessionBoundaryError::Log(SessionLogError::NotTurnBoundary));
        }

        let context = log.context();
        let core = AgentCoreLoop::from_transcript(context.transcript);
        let synced_checkpoint = core.transcript.checkpoint();
        Ok(Self {
            core,
            log,
            synced_checkpoint,
        })
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

    pub fn session_log(&self) -> &SessionLog {
        &self.log
    }

    pub fn model_context(&self) -> SessionContext {
        self.log.context()
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
        self.sync_log_from_core();
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
        self.log = SessionLog::from_transcript(&self.core.transcript);
        self.synced_checkpoint = self.core.transcript.checkpoint();
        Ok(previous.transcript)
    }

    pub fn prepare_compaction(
        &self,
        settings: CompactionSettings,
        external_work: ExternalWork,
    ) -> Result<Option<CompactionPlan>, SessionBoundaryError> {
        let quiescence = self.quiescence(external_work);
        if !quiescence.is_quiescent() {
            return Err(SessionBoundaryError::Busy(quiescence));
        }
        if !self.log.is_turn_boundary() {
            return Err(SessionBoundaryError::Log(SessionLogError::NotTurnBoundary));
        }
        Ok(self.log.prepare_compaction(settings))
    }

    pub fn compact_at_boundary(
        &mut self,
        plan: &CompactionPlan,
        summary: impl Into<String>,
        external_work: ExternalWork,
    ) -> Result<(), SessionBoundaryError> {
        let quiescence = self.quiescence(external_work);
        if !quiescence.is_quiescent() {
            return Err(SessionBoundaryError::Busy(quiescence));
        }
        if !self.log.contains_entry(&plan.first_kept_entry_id) {
            return Err(SessionBoundaryError::Log(SessionLogError::EntryNotFound));
        }
        if self.log.leaf_id() != plan.leaf_id.as_deref()
            || self.log.entries().len() != plan.entry_count
        {
            return Err(SessionBoundaryError::Log(SessionLogError::StalePlan));
        }
        if !self.log.is_turn_boundary() {
            return Err(SessionBoundaryError::Log(SessionLogError::NotTurnBoundary));
        }

        self.log.append_compaction(
            summary,
            plan.first_kept_entry_id.clone(),
            plan.tokens_before,
        );
        self.rehydrate_core_from_log();
        Ok(())
    }

    pub fn rewind_to_turn_boundary(
        &mut self,
        leaf_id: Option<&str>,
        external_work: ExternalWork,
    ) -> Result<(), SessionBoundaryError> {
        let quiescence = self.quiescence(external_work);
        if !quiescence.is_quiescent() {
            return Err(SessionBoundaryError::Busy(quiescence));
        }

        match leaf_id {
            Some(leaf_id) => self
                .log
                .branch_at_turn_boundary(leaf_id)
                .map_err(SessionBoundaryError::Log)?,
            None => self.log.reset_leaf(),
        }
        self.rehydrate_core_from_log();
        Ok(())
    }

    pub fn fork_at_turn_boundary(
        &self,
        leaf_id: Option<&str>,
        external_work: ExternalWork,
    ) -> Result<Self, SessionBoundaryError> {
        let quiescence = self.quiescence(external_work);
        if !quiescence.is_quiescent() {
            return Err(SessionBoundaryError::Busy(quiescence));
        }

        let log = self
            .log
            .create_branched_log_at_turn_boundary(leaf_id)
            .map_err(SessionBoundaryError::Log)?;
        Self::from_session_log(log)
    }

    fn rehydrate_core_from_log(&mut self) {
        let context = self.log.context();
        self.core = AgentCoreLoop::from_transcript(context.transcript);
        self.synced_checkpoint = self.core.transcript.checkpoint();
    }

    fn sync_log_from_core(&mut self) {
        let Some(records) = self.core.transcript.records_since(self.synced_checkpoint) else {
            self.log = SessionLog::from_transcript(&self.core.transcript);
            self.synced_checkpoint = self.core.transcript.checkpoint();
            return;
        };

        self.log.append_transcript_records(records.iter().cloned());
        self.synced_checkpoint = self.core.transcript.checkpoint();
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
    Log(SessionLogError),
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

    #[test]
    fn session_log_tracks_core_turn_records() {
        let mut session = AgentSession::new();
        session.handle_input(AgentInput::FollowUp("hello".to_string()));
        session.handle_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        });

        assert_eq!(session.session_log().entries().len(), 4);
        assert!(session.session_log().is_turn_boundary());
        assert_eq!(session.model_context().transcript.last_turn_id(), TurnId(1));
    }

    #[test]
    fn compaction_requires_quiescence_and_keeps_a_turn_boundary_suffix() {
        let mut session = AgentSession::from_transcript(Transcript::from_records_raw(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage("first user message".to_string()),
            TranscriptRecord::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("first answer".to_string())],
            }),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
            TranscriptRecord::UserMessage("second user message".to_string()),
            TranscriptRecord::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("second answer".to_string())],
            }),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ]));

        let plan = session
            .prepare_compaction(
                CompactionSettings {
                    keep_recent_tokens: 1,
                },
                ExternalWork::NONE,
            )
            .expect("session is quiescent")
            .expect("old turn should be compactable");

        session
            .compact_at_boundary(&plan, "summary", ExternalWork::NONE)
            .expect("quiescent boundary can compact");

        let context = session.model_context();
        assert_eq!(
            context
                .compaction
                .as_ref()
                .map(|entry| entry.summary.as_str()),
            Some("summary")
        );
        assert_eq!(session.transcript().last_turn_id(), TurnId(2));
        assert!(matches!(
            session.transcript().records().first(),
            Some(TranscriptRecord::TurnStarted { turn_id: TurnId(2) })
        ));
    }

    #[test]
    fn rewind_and_fork_only_accept_turn_finished_entries() {
        let mut session = AgentSession::from_transcript(Transcript::from_records_raw(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage("first".to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
            TranscriptRecord::UserMessage("second".to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ]));
        let mid_turn_id = session.session_log().entries()[1].id.clone();
        let turn_one_end_id = session.session_log().entries()[2].id.clone();

        assert_eq!(
            session.rewind_to_turn_boundary(Some(&mid_turn_id), ExternalWork::NONE),
            Err(SessionBoundaryError::Log(SessionLogError::NotTurnBoundary))
        );
        assert_eq!(
            session
                .fork_at_turn_boundary(Some(&mid_turn_id), ExternalWork::NONE)
                .map(|_| ()),
            Err(SessionBoundaryError::Log(SessionLogError::NotTurnBoundary))
        );

        session
            .rewind_to_turn_boundary(Some(&turn_one_end_id), ExternalWork::NONE)
            .expect("turn end is a valid rewind point");
        assert_eq!(session.transcript().last_turn_id(), TurnId(1));

        let fork = session
            .fork_at_turn_boundary(Some(&turn_one_end_id), ExternalWork::NONE)
            .expect("turn end is a valid fork point");
        assert_eq!(fork.transcript().last_turn_id(), TurnId(1));
    }
}
