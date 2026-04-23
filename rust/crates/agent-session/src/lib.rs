#![forbid(unsafe_code)]

mod runner;
mod session_log;
mod transcript;

use agent_core::AgentCoreLoop;

pub use crate::runner::{AgentInputHandle, AgentInputReceiver, AgentRunner};
pub use crate::session_log::{
    CompactionPlan, CompactionSettings, InjectedKind, InjectedMessage, SessionContext,
    SessionEntry, SessionEntryKind, SessionLog, SessionLogError,
};
pub use crate::transcript::Transcript;

// Re-export core-owned types so downstream callers have a single import home.
pub use agent_core::{AgentAction, AgentInput, TranscriptRecord, TurnId, TurnOutcome};

/// Session shell around the pure core loop.
///
/// `agent-core` owns deterministic state transitions. `agent-session` owns the
/// boundary where durable transcript state can be safely replaced, forked,
/// rewound, or resumed after consulting external model/tool work. The session
/// log is the sole owner of durable records; the core only buffers records
/// produced in the current run until the session absorbs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    core: AgentCoreLoop,
    log: SessionLog,
}

impl Default for AgentSession {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentSession {
    pub fn new() -> Self {
        Self {
            core: AgentCoreLoop::resume_at_boundary(TurnId::default()),
            log: SessionLog::new(),
        }
    }

    pub fn from_records(records: Vec<TranscriptRecord>) -> Self {
        Self::from_transcript(Transcript::from_records(records))
    }

    pub fn from_transcript(transcript: Transcript) -> Self {
        let last_turn_id = transcript.last_turn_id();
        let log = SessionLog::from_transcript(&transcript);
        Self {
            core: AgentCoreLoop::resume_at_boundary(last_turn_id),
            log,
        }
    }

    pub fn from_session_log(log: SessionLog) -> Result<Self, SessionBoundaryError> {
        if !log.is_turn_boundary() {
            return Err(SessionBoundaryError::Log(SessionLogError::NotTurnBoundary));
        }

        let context = log.context();
        let last_turn_id = context.transcript.last_turn_id();
        Ok(Self {
            core: AgentCoreLoop::resume_at_boundary(last_turn_id),
            log,
        })
    }

    /// Enqueue a new input into the underlying core loop.
    ///
    /// This is the only supported way to feed the core from outside the
    /// session; the core itself is not exposed so log absorption in `drive`
    /// cannot be bypassed.
    pub fn enqueue_input(&mut self, input: AgentInput) {
        self.core.enqueue_input(input);
    }

    /// The most recent turn id observed by the core loop.
    pub fn last_turn_id(&self) -> TurnId {
        self.core.last_turn_id()
    }

    /// True when the core loop is between turns and has no in-flight work.
    pub fn is_idle(&self) -> bool {
        self.core.is_idle()
    }

    /// True when the core loop's mailbox still has queued inputs.
    pub fn has_pending_work(&self) -> bool {
        self.core.has_pending_work()
    }

    /// Materialized view of the session history derived from the log.
    pub fn transcript(&self) -> Transcript {
        self.log.context().transcript
    }

    pub fn session_log(&self) -> &SessionLog {
        &self.log
    }

    pub fn model_context(&self) -> SessionContext {
        self.log.context()
    }

    /// Drive the core to quiescence and append any records it emitted to the
    /// session log. This is the only supported way to advance a session; the
    /// log remains the sole owner of durable history.
    pub fn drive(&mut self) {
        self.core.drive();
        self.absorb_core_records();
    }

    /// Drain pending actions the core produced during the last `drive`.
    ///
    /// Exposed so the async runner can flush actions to its handler without
    /// reaching into the core. Records are absorbed into the log inside
    /// `drive`, so there is no analogous `drain_records` on the session.
    pub fn drain_actions(&mut self) -> Vec<AgentAction> {
        self.core.drain_actions()
    }

    pub fn quiescence(&self, external_work: ExternalWork) -> SessionQuiescence {
        SessionQuiescence {
            core_idle: self.core.is_idle(),
            durable_turn_boundary: self.log.is_turn_boundary(),
            mailbox_empty: !self.core.has_pending_work(),
            external_work_empty: external_work.is_empty(),
        }
    }

    pub fn is_quiescent(&self, external_work: ExternalWork) -> bool {
        self.quiescence(external_work).is_quiescent()
    }

    /// Validate quiescence once; return a view that permits boundary ops.
    ///
    /// The returned `SessionBoundary` is the only surface for compact, rewind,
    /// fork, and replace_transcript — the guard ran once here, so individual
    /// ops do not repeat it.
    pub fn boundary(
        &mut self,
        external_work: ExternalWork,
    ) -> Result<SessionBoundary<'_>, SessionBoundaryError> {
        let quiescence = self.quiescence(external_work);
        if !quiescence.is_quiescent() {
            return Err(SessionBoundaryError::Busy(quiescence));
        }
        Ok(SessionBoundary { session: self })
    }

    fn absorb_core_records(&mut self) {
        let records = self.core.drain_records();
        if records.is_empty() {
            return;
        }
        self.log.append_transcript_records(records);
    }

    fn rehydrate_core_from_log(&mut self) {
        let last_turn_id = self.log.context().transcript.last_turn_id();
        self.core = AgentCoreLoop::resume_at_boundary(last_turn_id);
    }
}

/// Proven-quiescent borrow of an `AgentSession` that permits boundary ops.
///
/// Obtained via [`AgentSession::boundary`]. Each op validates only its own
/// preconditions (plan staleness, entry-not-found, not-at-boundary for the
/// replacement, etc.); the mailbox / outbox / external-work check happened
/// once when the view was created.
pub struct SessionBoundary<'a> {
    session: &'a mut AgentSession,
}

impl SessionBoundary<'_> {
    /// Plan a compaction against the current log. Returns `None` when no
    /// entries are old enough to evict under `settings`.
    pub fn prepare_compaction(&self, settings: CompactionSettings) -> Option<CompactionPlan> {
        self.session.log.prepare_compaction(settings)
    }

    /// Replace the durable transcript with `replacement`.
    ///
    /// `replacement` must itself be at a turn boundary. Returns the previous
    /// transcript so callers can persist it out-of-band if needed.
    pub fn replace_transcript(
        &mut self,
        replacement: Transcript,
    ) -> Result<Transcript, SessionBoundaryError> {
        if !replacement.is_turn_boundary() {
            return Err(SessionBoundaryError::ReplacementNotAtBoundary);
        }

        let previous = self.session.log.context().transcript;
        self.session.log = SessionLog::from_transcript(&replacement);
        self.session.rehydrate_core_from_log();
        Ok(previous)
    }

    /// Apply a previously prepared compaction, appending `summary` as the new
    /// compaction entry and dropping history before `plan.first_kept_entry_id`.
    pub fn compact(
        &mut self,
        plan: &CompactionPlan,
        summary: impl Into<String>,
    ) -> Result<(), SessionBoundaryError> {
        let log = &self.session.log;
        if !log.contains_entry(&plan.first_kept_entry_id) {
            return Err(SessionBoundaryError::Log(SessionLogError::EntryNotFound));
        }
        if log.leaf_id() != plan.leaf_id.as_deref() || log.entries().len() != plan.entry_count {
            return Err(SessionBoundaryError::Log(SessionLogError::StalePlan));
        }
        if !log.is_turn_boundary() {
            return Err(SessionBoundaryError::Log(SessionLogError::NotTurnBoundary));
        }

        self.session.log.append_compaction(
            summary,
            plan.first_kept_entry_id.clone(),
            plan.tokens_before,
        );
        self.session.rehydrate_core_from_log();
        Ok(())
    }

    /// Rewind the durable leaf to `leaf_id`, or reset to the root when `None`.
    /// `leaf_id` must point at a `TurnFinished` entry.
    pub fn rewind(&mut self, leaf_id: Option<&str>) -> Result<(), SessionBoundaryError> {
        match leaf_id {
            Some(leaf_id) => self
                .session
                .log
                .branch_at_turn_boundary(leaf_id)
                .map_err(SessionBoundaryError::Log)?,
            None => self.session.log.reset_leaf(),
        }
        self.session.rehydrate_core_from_log();
        Ok(())
    }

    /// Produce an unregistered `AgentSession` whose log branches from
    /// `leaf_id` (or the root when `None`). The source session is unchanged;
    /// the caller is responsible for registering the fork if desired.
    pub fn fork(&self, leaf_id: Option<&str>) -> Result<AgentSession, SessionBoundaryError> {
        let log = self
            .session
            .log
            .create_branched_log_at_turn_boundary(leaf_id)
            .map_err(SessionBoundaryError::Log)?;
        AgentSession::from_session_log(log)
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
    pub external_work_empty: bool,
}

impl SessionQuiescence {
    pub fn is_quiescent(self) -> bool {
        self.core_idle
            && self.durable_turn_boundary
            && self.mailbox_empty
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
        Transcript::from_records(vec![
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

        session.enqueue_input(AgentInput::FollowUp("hello".to_string()));
        assert!(!session.is_quiescent(ExternalWork::NONE));
        assert!(!session
            .quiescence(ExternalWork {
                model_requests: 1,
                ..ExternalWork::NONE
            })
            .is_quiescent());
    }

    #[test]
    fn transcript_replacement_is_only_allowed_at_quiescent_boundaries() {
        let mut session = AgentSession::new();
        session.enqueue_input(AgentInput::FollowUp("hello".to_string()));

        let busy = session
            .boundary(ExternalWork::NONE)
            .err()
            .expect("running sessions cannot open a boundary");
        assert!(matches!(busy, SessionBoundaryError::Busy(_)));

        let mut session = AgentSession::from_transcript(finished_transcript("hello"));

        let old = session
            .boundary(ExternalWork::NONE)
            .expect("idle boundary opens")
            .replace_transcript(finished_transcript("compact"))
            .expect("idle boundary can swap transcript");

        assert_eq!(old.last_turn_id(), TurnId(1));
        assert_eq!(
            session.transcript().records()[1],
            TranscriptRecord::UserMessage("compact".to_string())
        );
    }

    #[test]
    fn session_log_tracks_core_turn_records() {
        let session = AgentSession::from_transcript(finished_transcript("hello"));

        assert_eq!(session.session_log().entries().len(), 3);
        assert!(session.session_log().is_turn_boundary());
        assert_eq!(session.model_context().transcript.last_turn_id(), TurnId(1));
    }

    #[test]
    fn drive_absorbs_core_records_into_the_session_log() {
        let mut session = AgentSession::new();
        let assistant = AssistantMessage {
            items: vec![AssistantItem::Text("hi".to_string())],
        };

        session.enqueue_input(AgentInput::FollowUp("hello".to_string()));
        session.drive();
        session.enqueue_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: assistant.clone(),
        });
        session.drive();

        assert_eq!(
            session.transcript().records(),
            &[
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage("hello".to_string()),
                TranscriptRecord::AssistantMessage(assistant),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
            ]
        );
        // Driving again absorbs nothing new; the core buffer was drained.
        session.drive();
        assert_eq!(session.transcript().records().len(), 4);
    }

    #[test]
    fn compaction_requires_quiescence_and_keeps_a_turn_boundary_suffix() {
        let mut session = AgentSession::from_transcript(Transcript::from_records(vec![
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
            .boundary(ExternalWork::NONE)
            .expect("session is quiescent")
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("old turn should be compactable");

        session
            .boundary(ExternalWork::NONE)
            .expect("session is still quiescent")
            .compact(&plan, "summary")
            .expect("quiescent boundary can compact");

        let context = session.model_context();
        assert_eq!(
            context.latest_compaction().map(|msg| msg.content.as_str()),
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
        let mut session = AgentSession::from_transcript(Transcript::from_records(vec![
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
            session
                .boundary(ExternalWork::NONE)
                .expect("session is quiescent")
                .rewind(Some(&mid_turn_id)),
            Err(SessionBoundaryError::Log(SessionLogError::NotTurnBoundary))
        );
        assert_eq!(
            session
                .boundary(ExternalWork::NONE)
                .expect("session is quiescent")
                .fork(Some(&mid_turn_id))
                .map(|_| ()),
            Err(SessionBoundaryError::Log(SessionLogError::NotTurnBoundary))
        );

        session
            .boundary(ExternalWork::NONE)
            .expect("session is quiescent")
            .rewind(Some(&turn_one_end_id))
            .expect("turn end is a valid rewind point");
        assert_eq!(session.transcript().last_turn_id(), TurnId(1));

        let fork = session
            .boundary(ExternalWork::NONE)
            .expect("session is quiescent")
            .fork(Some(&turn_one_end_id))
            .expect("turn end is a valid fork point");
        assert_eq!(fork.transcript().last_turn_id(), TurnId(1));
    }

    #[test]
    fn rehydrating_an_incomplete_transcript_patches_a_crashed_finish() {
        let transcript = vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(7) },
            TranscriptRecord::UserMessage("hello".to_string()),
        ];

        let session = AgentSession::from_records(transcript);

        assert_eq!(
            session.transcript().records(),
            &[
                TranscriptRecord::TurnStarted { turn_id: TurnId(7) },
                TranscriptRecord::UserMessage("hello".to_string()),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(7),
                    outcome: TurnOutcome::Crashed,
                },
            ]
        );
        assert!(session.is_idle());
        assert_eq!(session.last_turn_id(), TurnId(7));
    }

    #[test]
    fn rehydrating_a_graceful_boundary_restores_idle_state() {
        let transcript = vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
            TranscriptRecord::UserMessage("hello".to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ];

        let session = AgentSession::from_records(transcript.clone());

        assert_eq!(session.transcript().records(), transcript.as_slice());
        assert!(session.is_idle());
        assert_eq!(session.last_turn_id(), TurnId(2));
    }
}
