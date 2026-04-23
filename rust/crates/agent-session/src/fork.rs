use agent_core::TranscriptRecord;

use crate::context::{
    compaction_first_kept_entry_id, is_compaction_summary, Context, ContextEdit, HistoryEditError,
    SessionEntry,
};
use crate::session::AgentSession;
use crate::transcript::Transcript;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub keep_recent_tokens: usize,
}

/// Describes a compaction the caller may apply to a session context.
///
/// A plan captures everything needed to turn a summary string back into a
/// durable compaction entry: the anchor (`first_kept_entry_id`), the records
/// the summary replaces (`records_to_summarize`), the surviving suffix
/// (`records_to_keep`), the pre-compaction token estimate (`tokens_before`),
/// and the immediate previous summary to thread through when summarizing.
/// `leaf_id` + `entry_count` are staleness-check hooks: the boundary
/// operation refuses to apply a plan if the context's shape has changed since
/// the plan was prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPlan {
    pub first_kept_entry_id: String,
    pub records_to_summarize: Vec<TranscriptRecord>,
    pub records_to_keep: Vec<TranscriptRecord>,
    pub tokens_before: usize,
    pub previous_summary: Option<String>,
    pub leaf_id: Option<String>,
    pub entry_count: usize,
}

impl Context {
    pub fn prepare_compaction(&self, settings: CompactionSettings) -> Option<CompactionPlan> {
        let path = self.branch_entries(None);
        if path
            .last()
            .map(|entry| is_compaction_summary(&entry.record))
            .unwrap_or(false)
        {
            return None;
        }

        let (boundary_start, previous_entry) = boundary_start_index(&path);
        let previous_summary = previous_entry.and_then(|entry| match &entry.record {
            TranscriptRecord::Custom(cm) => Some(cm.content.clone()),
            _ => None,
        });

        let tokens_before = estimate_records_tokens(self.transcript().records());
        let cut_index =
            find_boundary_cut_index(&path, boundary_start, settings.keep_recent_tokens)?;
        if cut_index <= boundary_start {
            return None;
        }

        let first_kept_entry = path.get(cut_index)?;
        let records_to_summarize = transcript_records_in(&path[boundary_start..cut_index]);
        if records_to_summarize.is_empty() {
            return None;
        }
        let records_to_keep = transcript_records_in(&path[cut_index..]);

        Some(CompactionPlan {
            first_kept_entry_id: first_kept_entry.id.clone(),
            records_to_summarize,
            records_to_keep,
            tokens_before,
            previous_summary,
            leaf_id: self.leaf_id().map(str::to_string),
            entry_count: self.entries().len(),
        })
    }
}

/// Compute the starting index for the boundary-cut search.
///
/// If a previous compaction exists on the active branch, we skip everything up
/// to and including its `first_kept_entry_id` — records before that were
/// already evicted under the earlier summary.
fn boundary_start_index(path: &[SessionEntry]) -> (usize, Option<&SessionEntry>) {
    let previous_compaction_index = path
        .iter()
        .rposition(|entry| is_compaction_summary(&entry.record));

    let start = match previous_compaction_index {
        Some(index) => compaction_first_kept_entry_id(&path[index].record)
            .and_then(|fk| path.iter().position(|e| e.id == fk))
            .or(Some(index + 1))
            .unwrap_or(0),
        None => 0,
    };
    let previous_entry = previous_compaction_index.map(|i| &path[i]);
    (start, previous_entry)
}

fn find_boundary_cut_index(
    path: &[SessionEntry],
    boundary_start: usize,
    keep_recent_tokens: usize,
) -> Option<usize> {
    let mut accumulated_tokens = 0;

    for index in (boundary_start..path.len()).rev() {
        // Custom entries live between turns; they don't count toward the
        // keep-recent window, and the cut must land on a turn boundary.
        if matches!(path[index].record, TranscriptRecord::Custom(_)) {
            continue;
        }
        accumulated_tokens += estimate_record_tokens(&path[index].record);
        if accumulated_tokens >= keep_recent_tokens {
            return turn_start_at_or_before(path, index, boundary_start);
        }
    }

    None
}

fn turn_start_at_or_before(
    path: &[SessionEntry],
    index: usize,
    boundary_start: usize,
) -> Option<usize> {
    for candidate in (boundary_start..=index).rev() {
        if matches!(path[candidate].record, TranscriptRecord::TurnStarted { .. }) {
            return Some(candidate);
        }
    }
    Some(boundary_start)
}

fn transcript_records_in(entries: &[SessionEntry]) -> Vec<TranscriptRecord> {
    entries
        .iter()
        .filter_map(|entry| match &entry.record {
            // Custom records are appended between turns; they are not part of
            // the raw-model transcript the summarizer replays.
            TranscriptRecord::Custom(_) => None,
            other => Some(other.clone()),
        })
        .collect()
}

fn estimate_records_tokens(records: &[TranscriptRecord]) -> usize {
    records.iter().map(estimate_record_tokens).sum()
}

fn estimate_record_tokens(record: &TranscriptRecord) -> usize {
    use agent_core::AssistantItem;

    let chars = match record {
        TranscriptRecord::TurnStarted { .. } | TranscriptRecord::TurnFinished { .. } => 0,
        TranscriptRecord::UserMessage(content) => content.len(),
        TranscriptRecord::AssistantMessage(message) => message
            .items
            .iter()
            .map(|item| match item {
                AssistantItem::Text(text) => text.len(),
                AssistantItem::ToolCall(tool_call) => {
                    tool_call.tool_name.len() + tool_call.args_json.len()
                }
            })
            .sum(),
        TranscriptRecord::ToolCallStarted { tool_call, .. } => {
            tool_call.tool_name.len() + tool_call.args_json.len()
        }
        TranscriptRecord::ToolResult(result) => result.tool_name.len() + result.output.len(),
        TranscriptRecord::Custom(cm) => cm.content.len(),
    };
    chars.div_ceil(4)
}

/// Materialize the model-visible transcript for a session-context path.
///
/// Under fork-based compaction the materialized order is identical to the
/// chronological order on the active branch: when `compact()` ran, it forked
/// the leaf at the pre-cut boundary, appended the compaction summary on the
/// new branch, and re-appended the kept records as descendants of the
/// summary. The entries past the latest compaction summary on this path are
/// therefore already in the exact order the model should see them.
///
/// So this materialization is a plain slice: find the latest `CompSum` on the
/// path (if any) and take records from there to the leaf. With no compaction
/// on the path, take the whole path.
pub(crate) fn materialize_context(path: &[SessionEntry]) -> Transcript {
    let start = path
        .iter()
        .rposition(|e| is_compaction_summary(&e.record))
        .unwrap_or(0);
    let records = path[start..].iter().map(|e| e.record.clone()).collect();
    Transcript::from_records(records)
}

impl<'a> ContextEdit<'a> {
    /// Plan a compaction against the current context. Returns `None` when no
    /// entries are old enough to evict under `settings`.
    pub fn prepare_compaction(&self, settings: CompactionSettings) -> Option<CompactionPlan> {
        self.session.context.prepare_compaction(settings)
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

        let previous = self.session.context.transcript();
        self.session.context = Context::from_transcript(&replacement);
        self.session.rehydrate_core_from_context();
        Ok(previous)
    }

    /// Apply a previously prepared compaction using the fork-based strategy:
    /// navigate the durable leaf back to the pre-cut boundary (parent of
    /// `first_kept_entry_id`), append `summary` there as a new branch, then
    /// re-append copies of the kept records as descendants of the summary.
    ///
    /// The new branch's chronological order (summary, then kept records) *is*
    /// the model's semantic view. The pre-compaction entries stay in the
    /// context as an orphaned branch so the audit trail is preserved.
    pub fn compact(
        &mut self,
        plan: &CompactionPlan,
        summary: impl Into<String>,
    ) -> Result<(), HistoryEditError> {
        self.session
            .context
            .validate_plan_matches(plan)
            .map_err(HistoryEditError::Context)?;

        // Find the pre-cut parent (the entry immediately before first_kept).
        let first_kept = self
            .session
            .context
            .get_entry(&plan.first_kept_entry_id)
            .expect("validate_plan_matches guarantees the entry exists");
        let pre_cut_parent_id = first_kept.parent_id.clone();

        // Navigate the leaf to the pre-cut boundary.
        match pre_cut_parent_id.as_deref() {
            Some(id) => self
                .session
                .context
                .branch_at_turn_boundary(id)
                .map_err(HistoryEditError::Context)?,
            None => self.session.context.reset_leaf(),
        }

        // Append the compaction summary on the new branch.
        self.session.context.append_compaction_summary(
            summary,
            plan.first_kept_entry_id.clone(),
            plan.tokens_before,
        );

        // Re-append copies of the kept records as descendants of the summary.
        self.session
            .context
            .append_transcript_records(plan.records_to_keep.iter().cloned());

        self.session.rehydrate_core_from_context();
        Ok(())
    }

    /// Rewind the durable leaf to `leaf_id`, or reset to the root when `None`.
    /// `leaf_id` must point at a `TurnFinished` entry.
    pub fn rewind(&mut self, leaf_id: Option<&str>) -> Result<(), HistoryEditError> {
        match leaf_id {
            Some(leaf_id) => self
                .session
                .context
                .branch_at_turn_boundary(leaf_id)
                .map_err(HistoryEditError::Context)?,
            None => self.session.context.reset_leaf(),
        }
        self.session.rehydrate_core_from_context();
        Ok(())
    }

    /// Produce an unregistered `AgentSession` whose context branches from
    /// `leaf_id` (or the root when `None`). The source session is unchanged;
    /// the caller is responsible for registering the fork if desired.
    pub fn fork(&self, leaf_id: Option<&str>) -> Result<AgentSession, HistoryEditError> {
        let context = self
            .session
            .context
            .create_branched_context_at_turn_boundary(leaf_id)
            .map_err(HistoryEditError::Context)?;
        AgentSession::from_context(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextError, HistoryEditError, PendingWork, KIND_COMPACTION_SUMMARY};
    use crate::session::AgentSession;
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
    fn compaction_plan_cuts_only_at_turn_boundaries() {
        let mut ctx = Context::new();
        let mut append_turn = |id: u64, user: &str, answer: &str| {
            ctx.append_transcript_records(vec![
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
            ]);
        };
        append_turn(1, "first user message", "first assistant message");
        append_turn(2, "second user message", "second assistant message");
        append_turn(3, "third user message", "third assistant message");

        let plan = ctx
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("old turns should be compactable");

        assert!(matches!(
            plan.records_to_keep.first(),
            Some(TranscriptRecord::TurnStarted { turn_id: TurnId(3) })
        ));
        assert!(plan
            .records_to_summarize
            .iter()
            .any(|record| matches!(record, TranscriptRecord::UserMessage(text) if text == "first user message")));
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
        // old branch lives on as an orphan in the full context entries.
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

        let entries_before = session.context().entries().len();
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

        // Context grew by: 1 (CompSum) + 4 (re-appended turn 2 records) = 5.
        assert_eq!(
            session.context().entries().len(),
            entries_before + 5,
            "fork-based compaction should add 1 summary + the kept records as new context entries"
        );

        // Materialized transcript: [CompSum, TurnStarted(2), UserMessage,
        // AssistantMessage, TurnFinished(2)].
        let transcript = session.transcript();
        let records = transcript.records();
        assert!(matches!(
            records.first(),
            Some(TranscriptRecord::Custom(cm)) if cm.kind == KIND_COMPACTION_SUMMARY
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

        // Drive a real fourth turn through the core loop.
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

        // Second compaction.
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
        let summary_count = transcript
            .records()
            .iter()
            .filter(
                |r| matches!(r, TranscriptRecord::Custom(cm) if cm.kind == KIND_COMPACTION_SUMMARY),
            )
            .count();
        assert_eq!(summary_count, 1);
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
        let mid_turn_id = session.context().entries()[1].id.clone();
        let turn_one_end_id = session.context().entries()[2].id.clone();

        assert_eq!(
            session
                .edit_history(PendingWork::NONE)
                .expect("session can edit history")
                .rewind(Some(&mid_turn_id)),
            Err(HistoryEditError::Context(ContextError::NotTurnBoundary))
        );
        assert_eq!(
            session
                .edit_history(PendingWork::NONE)
                .expect("session can edit history")
                .fork(Some(&mid_turn_id))
                .map(|_| ()),
            Err(HistoryEditError::Context(ContextError::NotTurnBoundary))
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
