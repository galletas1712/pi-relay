use agent_core::{InjectedMessage, TranscriptItem};

use crate::transcript_store::edit::{HistoryEdit, HistoryEditError};
use crate::transcript_store::span::{
    summary_span_plan_from_indices, transcript_items_in, SummarizeSpan, SummarySpanPlan,
};
use crate::transcript_store::tokens::{estimate_item_tokens, estimate_items_tokens};
use crate::transcript_store::{TranscriptStorageNode, TranscriptStore, TranscriptStoreError};

/// Well-known `InjectedMessage::kind` for compaction summaries.
pub const KIND_COMPACTION_SUMMARY: &str = "compaction_summary";

/// Build an `InjectedMessage` tagged as a compaction summary with the standard
/// `first_kept_entry_id` + `tokens_before` metadata.
pub fn compaction_summary(
    content: impl Into<String>,
    first_kept_entry_id: impl Into<String>,
    tokens_before: usize,
) -> InjectedMessage {
    InjectedMessage::new(KIND_COMPACTION_SUMMARY, content)
        .with_metadata("first_kept_entry_id", first_kept_entry_id)
        .with_metadata("tokens_before", tokens_before.to_string())
}

/// True if the transcript item is injected context with kind = `compaction_summary`.
pub(crate) fn is_compaction_summary(item: &TranscriptItem) -> bool {
    matches!(item, TranscriptItem::Injected(cm) if cm.kind == KIND_COMPACTION_SUMMARY)
}

/// Pull the `first_kept_entry_id` metadata off a compaction summary item.
pub(crate) fn compaction_first_kept_entry_id(item: &TranscriptItem) -> Option<&str> {
    match item {
        TranscriptItem::Injected(cm) if cm.kind == KIND_COMPACTION_SUMMARY => {
            cm.metadata.get("first_kept_entry_id").map(|s| s.as_str())
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub keep_recent_tokens: usize,
}

/// Describes a compaction the caller may apply to a session context.
///
/// A plan captures a prefix-oriented summary policy on top of the generic
/// `SummarizeSpan` edit: the span to replace (`summary_span`), the first
/// surviving entry (`first_kept_entry_id`), the items the summarizer should
/// read (`items_to_summarize`), the surviving suffix (`items_to_keep`),
/// the pre-compaction token estimate (`tokens_before`), and the immediate
/// previous summary to thread through when summarizing. `leaf_id` +
/// `entry_count` are staleness-check hooks: the operation refuses to apply a
/// plan if the context's shape has changed since the plan was prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPlan {
    pub summary_span: SummarySpanPlan,
    pub first_kept_entry_id: String,
    pub items_to_summarize: Vec<TranscriptItem>,
    pub items_to_keep: Vec<TranscriptItem>,
    pub tokens_before: usize,
    pub previous_summary: Option<String>,
    pub leaf_id: Option<String>,
    pub entry_count: usize,
}

impl TranscriptStore {
    /// Plan a compaction against the current context. Returns `None` when no
    /// entries are old enough to evict under `settings`.
    pub fn prepare_compaction(&self, settings: CompactionSettings) -> Option<CompactionPlan> {
        let path = self.branch_entries(None);
        if path
            .last()
            .map(|entry| is_compaction_summary(&entry.item))
            .unwrap_or(false)
        {
            return None;
        }

        let (boundary_start, previous_entry, span_start) = boundary_start_index(&path);
        let previous_summary = previous_entry.and_then(|entry| match &entry.item {
            TranscriptItem::Injected(cm) => Some(cm.content.clone()),
            _ => None,
        });

        let tokens_before = estimate_items_tokens(self.model_context().transcript_items());
        let cut_index =
            find_boundary_cut_index(&path, boundary_start, settings.keep_recent_tokens)?;
        if cut_index <= boundary_start {
            return None;
        }

        let first_kept_entry = path.get(cut_index)?;
        let span_last_index = cut_index.checked_sub(1)?;
        let summary_span = summary_span_plan_from_indices(self, &path, span_start, span_last_index);
        let items_to_summarize = transcript_items_in(&path[boundary_start..cut_index]);
        if items_to_summarize.is_empty() {
            return None;
        }
        let items_to_keep = transcript_items_in(&path[cut_index..]);

        Some(CompactionPlan {
            summary_span,
            first_kept_entry_id: first_kept_entry.id.clone(),
            items_to_summarize,
            items_to_keep,
            tokens_before,
            previous_summary,
            leaf_id: self.leaf_id().map(str::to_string),
            entry_count: self.entry_count(),
        })
    }

    /// Validate that a `CompactionPlan` still matches the context's current
    /// shape.
    ///
    /// Returns `EntryNotFound` if `plan.first_kept_entry_id` no longer exists,
    /// `StalePlan` if the context's leaf or entry count has drifted from the
    /// plan's fingerprint, or `NotTurnBoundary` if the current leaf isn't at
    /// a turn boundary.
    pub fn validate_plan_matches(&self, plan: &CompactionPlan) -> Result<(), TranscriptStoreError> {
        if !self.contains_entry(&plan.first_kept_entry_id) {
            return Err(TranscriptStoreError::EntryNotFound);
        }
        self.validate_summary_span_plan(&plan.summary_span)?;
        if self.leaf_id() != plan.leaf_id.as_deref() || self.entry_count() != plan.entry_count {
            return Err(TranscriptStoreError::StalePlan);
        }
        if !self.is_turn_boundary() {
            return Err(TranscriptStoreError::NotTurnBoundary);
        }
        Ok(())
    }
}

/// A prepared compaction operation.
///
/// Applies by converting the compaction plan into a generic `SummarizeSpan`
/// edit with a `compaction_summary` item. The replaced prefix stays in the
/// context as an orphaned branch so the audit trail is preserved.
pub struct Compact {
    pub plan: CompactionPlan,
    pub summary: String,
}

impl HistoryEdit for Compact {
    type Output = ();

    fn apply(self, ctx: &mut TranscriptStore) -> Result<(), HistoryEditError> {
        ctx.validate_plan_matches(&self.plan)
            .map_err(HistoryEditError::Store)?;

        let CompactionPlan {
            summary_span,
            first_kept_entry_id,
            tokens_before,
            ..
        } = self.plan;

        SummarizeSpan {
            plan: summary_span,
            summary: compaction_summary(self.summary, first_kept_entry_id, tokens_before),
        }
        .apply(ctx)
    }
}

/// Compute the starting index for the boundary-cut search.
///
/// If a previous compaction exists on the active branch, we skip everything up
/// to and including its `first_kept_entry_id` — items before that were
/// already evicted under the earlier summary.
fn boundary_start_index(
    path: &[TranscriptStorageNode],
) -> (usize, Option<&TranscriptStorageNode>, usize) {
    let previous_compaction_index = path
        .iter()
        .rposition(|entry| is_compaction_summary(&entry.item));

    let start = match previous_compaction_index {
        Some(index) => compaction_first_kept_entry_id(&path[index].item)
            .and_then(|fk| path.iter().position(|e| e.id == fk))
            .or(Some(index + 1))
            .unwrap_or(0),
        None => 0,
    };
    let span_start = previous_compaction_index.unwrap_or(start);
    let previous_entry = previous_compaction_index.map(|i| &path[i]);
    (start, previous_entry, span_start)
}

fn find_boundary_cut_index(
    path: &[TranscriptStorageNode],
    boundary_start: usize,
    keep_recent_tokens: usize,
) -> Option<usize> {
    let mut accumulated_tokens = 0;

    for index in (boundary_start..path.len()).rev() {
        accumulated_tokens += estimate_item_tokens(&path[index].item);
        if accumulated_tokens >= keep_recent_tokens {
            return turn_start_at_or_before(path, index, boundary_start);
        }
    }

    None
}

fn turn_start_at_or_before(
    path: &[TranscriptStorageNode],
    index: usize,
    boundary_start: usize,
) -> Option<usize> {
    for candidate in (boundary_start..=index).rev() {
        if matches!(path[candidate].item, TranscriptItem::TurnStarted { .. }) {
            return Some(candidate);
        }
    }
    Some(boundary_start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_context::ModelContext;
    use crate::session::AgentSession;
    use agent_core::{
        ActionId, AgentInput, AssistantItem, AssistantMessage, InjectedMessage, TranscriptItem,
        TurnId, TurnOutcome,
    };

    #[test]
    fn compaction_plan_cuts_only_at_turn_boundaries() {
        let mut ctx = TranscriptStore::new();
        let mut append_turn = |id: u64, user: &str, answer: &str| {
            ctx.append_transcript_items(vec![
                TranscriptItem::TurnStarted {
                    turn_id: TurnId(id),
                },
                TranscriptItem::UserMessage(user.to_string()),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text(answer.to_string())],
                }),
                TranscriptItem::TurnFinished {
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
            plan.items_to_keep.first(),
            Some(TranscriptItem::TurnStarted { turn_id: TurnId(3) })
        ));
        assert!(plan.items_to_summarize.iter().any(
            |item| matches!(item, TranscriptItem::UserMessage(text) if text == "first user message")
        ));
    }

    #[test]
    fn compaction_requires_turn_boundary_and_keeps_a_turn_boundary_suffix() {
        let mut session =
            AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("first user message".to_string()),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("first answer".to_string())],
                }),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
                TranscriptItem::TurnStarted { turn_id: TurnId(2) },
                TranscriptItem::UserMessage("second user message".to_string()),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("second answer".to_string())],
                }),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(2),
                    outcome: TurnOutcome::Graceful,
                },
            ]));

        let plan = session
            .transcript_store()
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("old turn should be compactable");

        session
            .edit(Compact {
                plan,
                summary: "summary".to_string(),
            })
            .expect("history edit can compact");

        let transcript = session.model_context();
        assert_eq!(transcript.latest_compaction_summary(), Some("summary"));
        assert_eq!(session.model_context().last_turn_id(), TurnId(2));
        assert!(matches!(
            transcript.transcript_items().first(),
            Some(TranscriptItem::Injected(_))
        ));
        // T1's items are no longer visible in the materialized view; the
        // old branch lives on as an orphan in the full context entries.
        let has_first_user = transcript
            .transcript_items()
            .iter()
            .any(|r| matches!(r, TranscriptItem::UserMessage(s) if s == "first user message"));
        assert!(!has_first_user);
    }

    #[test]
    fn compaction_plan_keeps_model_visible_injected_messages() {
        let mut ctx = TranscriptStore::new();
        ctx.append_transcript_items(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::Injected(InjectedMessage::new("agent_directive", "do first")),
            TranscriptItem::AssistantMessage(AssistantMessage { items: Vec::new() }),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptItem::TurnStarted { turn_id: TurnId(2) },
            TranscriptItem::Injected(InjectedMessage::new("agent_report", "second report")),
            TranscriptItem::AssistantMessage(AssistantMessage { items: Vec::new() }),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ]);

        let plan = ctx
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("first turn should be compactable");

        assert!(plan.items_to_summarize.iter().any(
            |item| matches!(item, TranscriptItem::Injected(cm) if cm.kind == "agent_directive")
        ));
        assert!(plan
            .items_to_keep
            .iter()
            .any(|item| matches!(item, TranscriptItem::Injected(cm) if cm.kind == "agent_report")));

        Compact {
            plan,
            summary: "summary".to_string(),
        }
        .apply(&mut ctx)
        .expect("compaction should apply");

        assert!(ctx
            .model_context()
            .transcript_items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Injected(cm) if cm.kind == "agent_report")));
    }

    #[test]
    fn fork_based_compaction_creates_new_branch_with_summary_then_kept_items() {
        let mut session =
            AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
                // turn 1
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("first".to_string()),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("ok1".to_string())],
                }),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
                // turn 2
                TranscriptItem::TurnStarted { turn_id: TurnId(2) },
                TranscriptItem::UserMessage("second".to_string()),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("ok2".to_string())],
                }),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(2),
                    outcome: TurnOutcome::Graceful,
                },
            ]));

        let entries_before = session.transcript_store().entries().len();
        let plan = session
            .transcript_store()
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("old turn should be compactable");
        session
            .edit(Compact {
                plan,
                summary: "summary".to_string(),
            })
            .expect("history edit can compact");

        // TranscriptStore grew by: 1 (CompSum) + 4 (re-appended turn 2 items) = 5.
        assert_eq!(
            session.transcript_store().entries().len(),
            entries_before + 5,
            "fork-based compaction should add 1 summary + the kept items as new context entries"
        );

        // Materialized transcript: [CompSum, TurnStarted(2), UserMessage,
        // AssistantMessage, TurnFinished(2)].
        let transcript = session.model_context();
        let items = transcript.transcript_items();
        assert!(matches!(
            items.first(),
            Some(TranscriptItem::Injected(cm)) if cm.kind == KIND_COMPACTION_SUMMARY
        ));
        assert_eq!(items.len(), 5);
        assert_eq!(transcript.last_turn_id(), TurnId(2));
        assert_eq!(transcript.latest_compaction_summary(), Some("summary"));

        // Turn 1 items are gone from the materialized view.
        let has_first = items
            .iter()
            .any(|r| matches!(r, TranscriptItem::UserMessage(s) if s == "first"));
        assert!(!has_first);
    }

    #[test]
    fn sequential_compactions_fork_from_the_prior_summary_on_the_active_branch() {
        fn turn(id: u64, user: &str, answer: &str) -> Vec<TranscriptItem> {
            vec![
                TranscriptItem::TurnStarted {
                    turn_id: TurnId(id),
                },
                TranscriptItem::UserMessage(user.to_string()),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text(answer.to_string())],
                }),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(id),
                    outcome: TurnOutcome::Graceful,
                },
            ]
        }
        let mut items = Vec::new();
        items.extend(turn(1, "first user message", "first assistant answer"));
        items.extend(turn(2, "second user message", "second assistant answer"));
        items.extend(turn(3, "third user message", "third assistant answer"));
        let mut session =
            AgentSession::from_model_context(ModelContext::from_transcript_items(items));

        // First compaction.
        let plan = session
            .transcript_store()
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("old turns should be compactable");
        session
            .edit(Compact {
                plan,
                summary: "first summary".to_string(),
            })
            .expect("first compaction should apply");
        assert_eq!(
            session.model_context().latest_compaction_summary(),
            Some("first summary")
        );

        // Drive a real fourth turn through the core loop.
        session
            .enqueue_input(AgentInput::follow_up("fourth user message"))
            .expect("plain follow-up is valid");
        session.drive();
        session.drain_actions();
        session
            .enqueue_input(AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: session.last_turn_id(),
                assistant: AssistantMessage {
                    items: vec![AssistantItem::Text("fourth assistant answer".to_string())],
                },
            })
            .expect("matching model completion is valid");
        session.drive();
        assert!(session.is_idle());

        // Second compaction.
        let plan2 = session
            .transcript_store()
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("T3 is compactable past the first summary on the active branch");
        session
            .edit(Compact {
                plan: plan2,
                summary: "second summary".to_string(),
            })
            .expect("second compaction should apply");

        let transcript = session.model_context();
        assert_eq!(
            transcript.latest_compaction_summary(),
            Some("second summary")
        );
        let summary_count = transcript
            .transcript_items()
            .iter()
            .filter(
                |r| matches!(r, TranscriptItem::Injected(cm) if cm.kind == KIND_COMPACTION_SUMMARY),
            )
            .count();
        assert_eq!(summary_count, 1);
        let has_third = transcript
            .transcript_items()
            .iter()
            .any(|r| matches!(r, TranscriptItem::UserMessage(s) if s == "third user message"));
        assert!(!has_third);
        let has_fourth = transcript
            .transcript_items()
            .iter()
            .any(|r| matches!(r, TranscriptItem::UserMessage(s) if s == "fourth user message"));
        assert!(has_fourth);
    }
}
