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

    /// Apply a previously prepared compaction using the fork-based strategy:
    /// navigate the durable leaf back to the pre-cut boundary (parent of
    /// `first_kept_entry_id`), append `summary` there as a new branch, then
    /// re-append copies of the kept records as descendants of the summary.
    ///
    /// The new branch's chronological order (summary, then kept records) *is*
    /// the model's semantic view. The pre-compaction entries stay in the log
    /// as an orphaned branch so the audit trail is preserved.
    pub fn compact(
        &mut self,
        plan: &CompactionPlan,
        summary: impl Into<String>,
    ) -> Result<(), HistoryEditError> {
        self.session
            .log
            .validate_plan_matches(plan)
            .map_err(HistoryEditError::Log)?;

        // Find the pre-cut parent (the entry immediately before first_kept).
        let first_kept = self
            .session
            .log
            .get_entry(&plan.first_kept_entry_id)
            .expect("validate_plan_matches guarantees the entry exists");
        let pre_cut_parent_id = first_kept.parent_id.clone();

        // Navigate the leaf to the pre-cut boundary.
        match pre_cut_parent_id.as_deref() {
            Some(id) => self
                .session
                .log
                .branch_at_turn_boundary(id)
                .map_err(HistoryEditError::Log)?,
            None => self.session.log.reset_leaf(),
        }

        // Append the compaction summary on the new branch.
        self.session.log.append_compaction_summary(
            summary,
            plan.first_kept_entry_id.clone(),
            plan.tokens_before,
        );

        // Re-append copies of the kept records as descendants of the summary.
        self.session
            .log
            .append_transcript_records(plan.records_to_keep.iter().cloned());

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

/// Caller-tracked work the session cannot observe (worklog forks, background
/// summarization calls, etc.). The session tracks its own in-flight model and
/// tool requests internally via the pending-action set, so those are not
/// represented here.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PendingWork {
    pub background_tasks: usize,
}

impl PendingWork {
    pub const NONE: Self = Self {
        background_tasks: 0,
    };

    pub fn is_empty(self) -> bool {
        self.background_tasks == 0
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
        // T1's records are no longer visible in the materialized view; the
        // old branch lives on as an orphan in the full log entries.
        let has_first_user = transcript
            .records()
            .iter()
            .any(|r| matches!(r, TranscriptRecord::UserMessage(s) if s == "first user message"));
        assert!(!has_first_user);
    }

    #[test]
    fn fork_based_compaction_creates_new_branch_with_summary_then_kept_records() {
        let mut session = AgentSession::from_transcript(Transcript::from_records(vec![
            // turn 1
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage("first".to_string()),
            TranscriptRecord::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("ok1".to_string())],
            }),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            // turn 2
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
            TranscriptRecord::UserMessage("second".to_string()),
            TranscriptRecord::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("ok2".to_string())],
            }),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ]));

        let entries_before = session.session_log().entries().len();
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

        // Log grew by: 1 (CompSum) + 4 (re-appended turn 2 records) = 5.
        assert_eq!(
            session.session_log().entries().len(),
            entries_before + 5,
            "fork-based compaction should add 1 summary + the kept records as new log entries"
        );

        // Materialized transcript: [CompSum, TurnStarted(2), UserMessage,
        // AssistantMessage, TurnFinished(2)].
        let transcript = session.transcript();
        let records = transcript.records();
        assert!(matches!(
            records.first(),
            Some(TranscriptRecord::Custom(cm)) if cm.kind == crate::session_log::KIND_COMPACTION_SUMMARY
        ));
        assert_eq!(records.len(), 5);
        assert_eq!(transcript.last_turn_id(), TurnId(2));
        assert_eq!(transcript.latest_compaction_summary(), Some("summary"));

        // Turn 1 records are gone from the materialized view.
        let has_first = records
            .iter()
            .any(|r| matches!(r, TranscriptRecord::UserMessage(s) if s == "first"));
        assert!(!has_first);
    }

    #[test]
    fn sequential_compactions_fork_from_the_prior_summary_on_the_active_branch() {
        fn turn(id: u64, user: &str, answer: &str) -> Vec<TranscriptRecord> {
            vec![
                TranscriptRecord::TurnStarted {
                    turn_id: TurnId(id),
                },
                TranscriptRecord::UserMessage(user.to_string()),
                TranscriptRecord::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text(answer.to_string())],
                }),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(id),
                    outcome: TurnOutcome::Graceful,
                },
            ]
        }
        // Three turns up front; long enough content that keep_recent_tokens=1
        // will summarize T1 and T2 on the first pass (keeping T3), then a
        // fourth turn runs, and the second pass summarizes T3 (keeping T4).
        let mut records = Vec::new();
        records.extend(turn(1, "first user message", "first assistant answer"));
        records.extend(turn(2, "second user message", "second assistant answer"));
        records.extend(turn(3, "third user message", "third assistant answer"));
        let mut session = AgentSession::from_transcript(Transcript::from_records(records));

        // First compaction.
        let plan = session
            .edit_history(PendingWork::NONE)
            .expect("session can edit history")
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("old turns should be compactable");
        session
            .edit_history(PendingWork::NONE)
            .expect("session can still edit history")
            .compact(&plan, "first summary")
            .expect("first compaction should apply");
        assert_eq!(
            session.transcript().latest_compaction_summary(),
            Some("first summary")
        );

        // Drive a real fourth turn through the core loop. This appends T4's
        // records as descendants of the first-compaction branch (no stale
        // id bookkeeping involved; the log is the same log).
        session.enqueue_input(AgentInput::FollowUp("fourth user message".to_string()));
        session.drive();
        session.drain_actions();
        session.enqueue_input(AgentInput::ModelCompleted {
            turn_id: session.last_turn_id(),
            assistant: AssistantMessage {
                items: vec![AssistantItem::Text("fourth assistant answer".to_string())],
            },
        });
        session.drive();
        assert!(session.is_idle());

        // Second compaction: boundary_start_index correctly steps past the
        // first summary (whose `first_kept_entry_id` anchor still resolves
        // on the active branch), so T3 is eligible to be summarized while
        // T4 remains as the recent tail.
        let plan2 = session
            .edit_history(PendingWork::NONE)
            .expect("session can edit history")
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("T3 is compactable past the first summary on the active branch");
        session
            .edit_history(PendingWork::NONE)
            .expect("session can still edit history")
            .compact(&plan2, "second summary")
            .expect("second compaction should apply");

        let transcript = session.transcript();
        assert_eq!(
            transcript.latest_compaction_summary(),
            Some("second summary")
        );
        // Only the latest CompSum is visible on the materialized active
        // branch (the slice starts at the latest CompSum).
        let summary_count = transcript
            .records()
            .iter()
            .filter(|r| matches!(r, TranscriptRecord::Custom(cm) if cm.kind == crate::session_log::KIND_COMPACTION_SUMMARY))
            .count();
        assert_eq!(summary_count, 1);
        // T3 is gone from the materialized view; T4 remains as the recent
        // tail.
        let has_third = transcript
            .records()
            .iter()
            .any(|r| matches!(r, TranscriptRecord::UserMessage(s) if s == "third user message"));
        assert!(!has_third);
        let has_fourth = transcript
            .records()
            .iter()
            .any(|r| matches!(r, TranscriptRecord::UserMessage(s) if s == "fourth user message"));
        assert!(has_fourth);
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
