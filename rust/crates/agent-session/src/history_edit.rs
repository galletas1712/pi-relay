use crate::session::AgentSession;
use crate::session_log::{CompactionPlan, CompactionSettings, SessionLog, SessionLogError};
use crate::transcript::Transcript;

/// Proven-safe borrow of an `AgentSession` that permits editing the session's
/// history.
///
/// Obtained via [`AgentSession::edit_history`]. Each op validates only its own
/// preconditions (plan staleness, entry-not-found, replacement not at a turn
/// boundary, etc.); the mailbox / outbox / pending-work check happened once
/// when the view was created.
pub struct SessionHistoryEdit<'a> {
    session: &'a mut AgentSession,
}

impl<'a> SessionHistoryEdit<'a> {
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
    ) -> Result<Transcript, HistoryEditError> {
        if !replacement.is_turn_boundary() {
            return Err(HistoryEditError::ReplacementNotAtTurnBoundary);
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
    ) -> Result<(), HistoryEditError> {
        let log = &self.session.log;
        if !log.contains_entry(&plan.first_kept_entry_id) {
            return Err(HistoryEditError::Log(SessionLogError::EntryNotFound));
        }
        if log.leaf_id() != plan.leaf_id.as_deref() || log.entries().len() != plan.entry_count {
            return Err(HistoryEditError::Log(SessionLogError::StalePlan));
        }
        if !log.is_turn_boundary() {
            return Err(HistoryEditError::Log(SessionLogError::NotTurnBoundary));
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
    pub fn rewind(&mut self, leaf_id: Option<&str>) -> Result<(), HistoryEditError> {
        match leaf_id {
            Some(leaf_id) => self
                .session
                .log
                .branch_at_turn_boundary(leaf_id)
                .map_err(HistoryEditError::Log)?,
            None => self.session.log.reset_leaf(),
        }
        self.session.rehydrate_core_from_log();
        Ok(())
    }

    /// Produce an unregistered `AgentSession` whose log branches from
    /// `leaf_id` (or the root when `None`). The source session is unchanged;
    /// the caller is responsible for registering the fork if desired.
    pub fn fork(&self, leaf_id: Option<&str>) -> Result<AgentSession, HistoryEditError> {
        let log = self
            .session
            .log
            .create_branched_log_at_turn_boundary(leaf_id)
            .map_err(HistoryEditError::Log)?;
        AgentSession::from_session_log(log)
    }
}

/// Tally of work pending outside the session (model calls in flight, tool
/// executions, background tasks). Callers supply this when opening a
/// history-edit view so the session can refuse to edit its history while any
/// of those are still pending.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PendingWork {
    pub model_requests: usize,
    pub tool_requests: usize,
    pub background_tasks: usize,
}

impl PendingWork {
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
pub enum HistoryEditError {
    /// The session cannot currently edit its history (core still running,
    /// durable leaf mid-turn, mailbox non-empty, or pending work outstanding).
    Busy,
    /// A transcript supplied to `replace_transcript` did not itself end at a
    /// turn boundary.
    ReplacementNotAtTurnBoundary,
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
    fn transcript_replacement_is_only_allowed_at_turn_boundary() {
        let mut session = AgentSession::new();
        session.enqueue_input(AgentInput::FollowUp("hello".to_string()));

        let busy = session
            .edit_history(PendingWork::NONE)
            .err()
            .expect("running sessions cannot edit history");
        assert_eq!(busy, HistoryEditError::Busy);

        let mut session = AgentSession::from_transcript(finished_transcript("hello"));

        let old = session
            .edit_history(PendingWork::NONE)
            .expect("idle session can edit history")
            .replace_transcript(finished_transcript("compact"))
            .expect("idle session can swap transcript");

        assert_eq!(old.last_turn_id(), TurnId(1));
        assert_eq!(
            session.transcript().records()[1],
            TranscriptRecord::UserMessage("compact".to_string())
        );
    }

    #[test]
    fn compaction_requires_turn_boundary_and_keeps_a_turn_boundary_suffix() {
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
            .edit_history(PendingWork::NONE)
            .expect("session can edit history")
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("old turn should be compactable");

        session
            .edit_history(PendingWork::NONE)
            .expect("session can still edit history")
            .compact(&plan, "summary")
            .expect("history edit can compact");

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
                .edit_history(PendingWork::NONE)
                .expect("session can edit history")
                .rewind(Some(&mid_turn_id)),
            Err(HistoryEditError::Log(SessionLogError::NotTurnBoundary))
        );
        assert_eq!(
            session
                .edit_history(PendingWork::NONE)
                .expect("session can edit history")
                .fork(Some(&mid_turn_id))
                .map(|_| ()),
            Err(HistoryEditError::Log(SessionLogError::NotTurnBoundary))
        );

        session
            .edit_history(PendingWork::NONE)
            .expect("session can edit history")
            .rewind(Some(&turn_one_end_id))
            .expect("turn end is a valid rewind point");
        assert_eq!(session.transcript().last_turn_id(), TurnId(1));

        let fork = session
            .edit_history(PendingWork::NONE)
            .expect("session can edit history")
            .fork(Some(&turn_one_end_id))
            .expect("turn end is a valid fork point");
        assert_eq!(fork.transcript().last_turn_id(), TurnId(1));
    }
}
