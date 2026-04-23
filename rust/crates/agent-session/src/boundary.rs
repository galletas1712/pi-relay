use crate::session::AgentSession;
use crate::session_log::{CompactionPlan, CompactionSettings, SessionLog, SessionLogError};
use crate::transcript::Transcript;

/// Proven-at-boundary borrow of an `AgentSession` that permits boundary ops.
///
/// Obtained via [`AgentSession::boundary`]. Each op validates only its own
/// preconditions (plan staleness, entry-not-found, not-at-boundary for the
/// replacement, etc.); the mailbox / outbox / external-work check happened
/// once when the view was created.
pub struct SessionBoundary<'a> {
    session: &'a mut AgentSession,
}

impl<'a> SessionBoundary<'a> {
    pub(crate) fn new(session: &'a mut AgentSession) -> Self {
        Self { session }
    }

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

        let previous = self.session.log.context();
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

        self.session.log.append_compaction_summary(
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

/// Tally of outstanding work external to the session (model calls in
/// flight, tool executions, background tasks). Callers supply this when
/// opening a boundary so the session can refuse boundary ops while any of
/// those are still pending.
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
pub enum SessionBoundaryError {
    /// The session is not at a boundary (core still running, durable leaf
    /// mid-turn, mailbox non-empty, or external work outstanding).
    Busy,
    /// A transcript supplied to `replace_transcript` did not itself end at a
    /// turn boundary.
    ReplacementNotAtBoundary,
    /// An underlying session-log error: entry not found, not at a turn
    /// boundary, or a stale compaction plan.
    Log(SessionLogError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{
        AgentInput, AssistantItem, AssistantMessage, TranscriptRecord, TurnId, TurnOutcome,
    };

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
    fn transcript_replacement_is_only_allowed_at_boundary() {
        let mut session = AgentSession::new();
        session.enqueue_input(AgentInput::FollowUp("hello".to_string()));

        let busy = session
            .boundary(ExternalWork::NONE)
            .err()
            .expect("running sessions cannot open a boundary");
        assert_eq!(busy, SessionBoundaryError::Busy);

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
    fn compaction_requires_boundary_and_keeps_a_turn_boundary_suffix() {
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
            .expect("session is at boundary")
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("old turn should be compactable");

        session
            .boundary(ExternalWork::NONE)
            .expect("session is still at boundary")
            .compact(&plan, "summary")
            .expect("boundary can compact");

        let transcript = session.transcript();
        assert_eq!(transcript.latest_compaction_summary(), Some("summary"));
        assert_eq!(session.transcript().last_turn_id(), TurnId(2));
        assert!(matches!(
            transcript.records().first(),
            Some(TranscriptRecord::Custom(_))
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
                .expect("session is at boundary")
                .rewind(Some(&mid_turn_id)),
            Err(SessionBoundaryError::Log(SessionLogError::NotTurnBoundary))
        );
        assert_eq!(
            session
                .boundary(ExternalWork::NONE)
                .expect("session is at boundary")
                .fork(Some(&mid_turn_id))
                .map(|_| ()),
            Err(SessionBoundaryError::Log(SessionLogError::NotTurnBoundary))
        );

        session
            .boundary(ExternalWork::NONE)
            .expect("session is at boundary")
            .rewind(Some(&turn_one_end_id))
            .expect("turn end is a valid rewind point");
        assert_eq!(session.transcript().last_turn_id(), TurnId(1));

        let fork = session
            .boundary(ExternalWork::NONE)
            .expect("session is at boundary")
            .fork(Some(&turn_one_end_id))
            .expect("turn end is a valid fork point");
        assert_eq!(fork.transcript().last_turn_id(), TurnId(1));
    }
}
