use agent_core::{AgentAction, AgentCoreLoop, AgentInput, TranscriptRecord, TurnId};

use crate::boundary::{ExternalWork, SessionBoundary, SessionBoundaryError};
use crate::session_log::{SessionLog, SessionLogError};
use crate::transcript::Transcript;

/// Session shell around the pure core loop.
///
/// `agent-core` owns deterministic state transitions. `agent-session` owns the
/// boundary where durable transcript state can be safely replaced, forked,
/// rewound, or resumed after consulting external model/tool work. The session
/// log is the sole owner of durable records; the core only buffers records
/// produced in the current run until the session absorbs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub(crate) core: AgentCoreLoop,
    pub(crate) log: SessionLog,
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

        let transcript = log.context();
        let last_turn_id = transcript.last_turn_id();
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

    /// Materialized view of the session history derived from the log. With a
    /// compaction present, the latest summary is inlined ahead of the kept
    /// suffix so downstream callers see a single ordered record stream.
    pub fn transcript(&self) -> Transcript {
        self.log.context()
    }

    pub fn session_log(&self) -> &SessionLog {
        &self.log
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

    /// True when a boundary can be opened: core idle, log at a turn boundary,
    /// mailbox empty, no external work.
    pub fn at_boundary(&self, external_work: ExternalWork) -> bool {
        self.core.is_idle()
            && self.log.is_turn_boundary()
            && !self.core.has_pending_work()
            && external_work.is_empty()
    }

    /// Validate boundary preconditions once; return a view that permits
    /// boundary ops.
    ///
    /// The returned `SessionBoundary` is the only surface for compact,
    /// rewind, fork, and replace_transcript — the guard ran once here, so
    /// individual ops do not repeat it.
    pub fn boundary(
        &mut self,
        work: ExternalWork,
    ) -> Result<SessionBoundary<'_>, SessionBoundaryError> {
        if self.at_boundary(work) {
            Ok(SessionBoundary::new(self))
        } else {
            Err(SessionBoundaryError::Busy)
        }
    }

    fn absorb_core_records(&mut self) {
        let records = self.core.drain_records();
        if records.is_empty() {
            return;
        }
        self.log.append_transcript_records(records);
    }

    pub(crate) fn rehydrate_core_from_log(&mut self) {
        let last_turn_id = self.log.context().last_turn_id();
        self.core = AgentCoreLoop::resume_at_boundary(last_turn_id);
    }
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
    fn at_boundary_requires_idle_core_empty_queues_and_no_external_work() {
        let mut session = AgentSession::new();

        session.enqueue_input(AgentInput::FollowUp("hello".to_string()));
        assert!(!session.at_boundary(ExternalWork::NONE));
        assert!(!session.at_boundary(ExternalWork {
            model_requests: 1,
            ..ExternalWork::NONE
        }));
    }

    #[test]
    fn session_log_tracks_core_turn_records() {
        let session = AgentSession::from_transcript(finished_transcript("hello"));

        assert_eq!(session.session_log().entries().len(), 3);
        assert!(session.session_log().is_turn_boundary());
        assert_eq!(session.transcript().last_turn_id(), TurnId(1));
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
