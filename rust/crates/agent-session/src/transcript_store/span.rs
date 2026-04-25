use agent_core::{ContextItem, InjectedMessage};

use crate::transcript_store::edit::{HistoryEdit, HistoryEditError};
use crate::transcript_store::tokens::estimate_records_tokens;
use crate::transcript_store::{TranscriptEntry, TranscriptStore, TranscriptStoreError};

/// A stable plan to replace a contiguous span on the active branch with a
/// summary entry.
///
/// The span must start after a turn boundary and end at a turn boundary. This
/// keeps the replacement from splitting a live turn, model response, or tool
/// batch in half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarySpanPlan {
    pub first_entry_id: String,
    pub last_entry_id: String,
    pub records_to_replace: Vec<ContextItem>,
    pub records_after_span: Vec<ContextItem>,
    pub tokens_before: usize,
    pub leaf_id: Option<String>,
    pub entry_count: usize,
}

/// Replace a prepared context span with a caller-provided summary record.
///
/// The old entries remain in the append-only store as an orphaned branch. The
/// active branch is rebuilt as: prefix before the span, summary, then copies of
/// the records after the span.
pub struct SummarizeSpan {
    pub plan: SummarySpanPlan,
    pub summary: InjectedMessage,
}

impl TranscriptStore {
    /// Prepare a summary span over the active branch.
    ///
    /// `first_entry_id` and `last_entry_id` are inclusive. Both entries must be
    /// on the active branch, in that order, with the span starting immediately
    /// after a turn boundary and ending at a turn boundary.
    pub fn prepare_summary_span(
        &self,
        first_entry_id: &str,
        last_entry_id: &str,
    ) -> Result<SummarySpanPlan, TranscriptStoreError> {
        if !self.contains_entry(first_entry_id) || !self.contains_entry(last_entry_id) {
            return Err(TranscriptStoreError::EntryNotFound);
        }

        let path = self.branch_entries(None);
        let (first_index, last_index) = active_span_indices(&path, first_entry_id, last_entry_id)?;
        validate_span_boundaries(self, &path, first_index, last_index)?;
        Ok(summary_span_plan_from_indices(
            self,
            &path,
            first_index,
            last_index,
        ))
    }

    pub(crate) fn validate_summary_span_plan(
        &self,
        plan: &SummarySpanPlan,
    ) -> Result<(), TranscriptStoreError> {
        if !self.contains_entry(&plan.first_entry_id) || !self.contains_entry(&plan.last_entry_id) {
            return Err(TranscriptStoreError::EntryNotFound);
        }
        if self.leaf_id() != plan.leaf_id.as_deref() || self.entry_count() != plan.entry_count {
            return Err(TranscriptStoreError::StalePlan);
        }

        let path = self.branch_entries(None);
        let (first_index, last_index) =
            active_span_indices(&path, &plan.first_entry_id, &plan.last_entry_id)?;
        validate_span_boundaries(self, &path, first_index, last_index)
    }
}

impl HistoryEdit for SummarizeSpan {
    type Output = ();

    fn apply(self, ctx: &mut TranscriptStore) -> Result<(), HistoryEditError> {
        ctx.validate_summary_span_plan(&self.plan)
            .map_err(HistoryEditError::Store)?;

        let path = ctx.branch_entries(None);
        let (first_index, _) =
            active_span_indices(&path, &self.plan.first_entry_id, &self.plan.last_entry_id)
                .expect("validate_summary_span_plan guarantees active span indices");
        let pre_span_parent_id = path[first_index].parent_id.clone();

        match pre_span_parent_id.as_deref() {
            Some(id) => ctx
                .branch_at_turn_boundary(id)
                .map_err(HistoryEditError::Store)?,
            None => ctx.reset_leaf(),
        }

        ctx.append_injected(self.summary);
        ctx.append_context_items(self.plan.records_after_span.iter().cloned());
        Ok(())
    }
}

pub(crate) fn summary_span_plan_from_indices(
    ctx: &TranscriptStore,
    path: &[TranscriptEntry],
    first_index: usize,
    last_index: usize,
) -> SummarySpanPlan {
    SummarySpanPlan {
        first_entry_id: path[first_index].id.clone(),
        last_entry_id: path[last_index].id.clone(),
        records_to_replace: transcript_records_in(&path[first_index..=last_index]),
        records_after_span: transcript_records_in(&path[last_index + 1..]),
        tokens_before: estimate_records_tokens(ctx.model_context().records()),
        leaf_id: ctx.leaf_id().map(str::to_string),
        entry_count: ctx.entry_count(),
    }
}

pub(crate) fn transcript_records_in(entries: &[TranscriptEntry]) -> Vec<ContextItem> {
    entries.iter().map(|entry| entry.record.clone()).collect()
}

fn active_span_indices(
    path: &[TranscriptEntry],
    first_entry_id: &str,
    last_entry_id: &str,
) -> Result<(usize, usize), TranscriptStoreError> {
    let first_index = path
        .iter()
        .position(|entry| entry.id == first_entry_id)
        .ok_or(TranscriptStoreError::InvalidSpan)?;
    let last_index = path
        .iter()
        .position(|entry| entry.id == last_entry_id)
        .ok_or(TranscriptStoreError::InvalidSpan)?;
    if first_index > last_index {
        return Err(TranscriptStoreError::InvalidSpan);
    }
    Ok((first_index, last_index))
}

fn validate_span_boundaries(
    ctx: &TranscriptStore,
    path: &[TranscriptEntry],
    first_index: usize,
    last_index: usize,
) -> Result<(), TranscriptStoreError> {
    if !ctx.is_turn_boundary_leaf(path[first_index].parent_id.as_deref()) {
        return Err(TranscriptStoreError::NotTurnBoundary);
    }
    if !ctx.is_turn_boundary_leaf(Some(&path[last_index].id)) {
        return Err(TranscriptStoreError::NotTurnBoundary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AssistantMessage, InjectedMessage, TurnId, TurnOutcome};

    fn turn(turn_id: u64, user: &str) -> Vec<ContextItem> {
        vec![
            ContextItem::TurnStarted {
                turn_id: TurnId(turn_id),
            },
            ContextItem::UserMessage(user.to_string()),
            ContextItem::AssistantMessage(AssistantMessage { items: Vec::new() }),
            ContextItem::TurnFinished {
                turn_id: TurnId(turn_id),
                outcome: TurnOutcome::Graceful,
            },
        ]
    }

    #[test]
    fn summarize_span_replaces_a_middle_run_and_replays_suffix() {
        let mut ctx = TranscriptStore::new();
        ctx.append_context_items(turn(1, "first"));
        let second_ids = ctx.append_context_items(turn(2, "second"));
        ctx.append_context_items(turn(3, "third"));

        let plan = ctx
            .prepare_summary_span(&second_ids[0], &second_ids[3])
            .expect("whole middle turn is a valid summary span");
        assert_eq!(plan.records_after_span.len(), 4);

        SummarizeSpan {
            plan,
            summary: InjectedMessage::new("summary", "second summarized"),
        }
        .apply(&mut ctx)
        .expect("summary span should apply");

        let records = ctx.model_context().into_records();
        assert!(records
            .iter()
            .any(|record| matches!(record, ContextItem::UserMessage(text) if text == "first")));
        assert!(records
            .iter()
            .any(|record| matches!(record, ContextItem::Injected(cm) if cm.kind == "summary")));
        assert!(!records
            .iter()
            .any(|record| matches!(record, ContextItem::UserMessage(text) if text == "second")));
        assert!(records
            .iter()
            .any(|record| matches!(record, ContextItem::UserMessage(text) if text == "third")));
    }

    #[test]
    fn summarize_span_can_replace_the_suffix() {
        let mut ctx = TranscriptStore::new();
        ctx.append_context_items(turn(1, "first"));
        let second_ids = ctx.append_context_items(turn(2, "second"));

        let plan = ctx
            .prepare_summary_span(&second_ids[0], &second_ids[3])
            .expect("suffix turn is a valid summary span");
        assert!(plan.records_after_span.is_empty());

        SummarizeSpan {
            plan,
            summary: InjectedMessage::new("summary", "second summarized"),
        }
        .apply(&mut ctx)
        .expect("suffix summary span should apply");

        let records = ctx.model_context().into_records();
        assert!(records
            .iter()
            .any(|record| matches!(record, ContextItem::UserMessage(text) if text == "first")));
        assert!(records
            .iter()
            .any(|record| matches!(record, ContextItem::Injected(cm) if cm.kind == "summary")));
        assert!(!records
            .iter()
            .any(|record| matches!(record, ContextItem::UserMessage(text) if text == "second")));
        assert!(ctx.is_turn_boundary());
    }

    #[test]
    fn summarize_span_requires_whole_turn_boundaries() {
        let mut ctx = TranscriptStore::new();
        let ids = ctx.append_context_items(turn(1, "first"));

        assert_eq!(
            ctx.prepare_summary_span(&ids[1], &ids[3]),
            Err(TranscriptStoreError::NotTurnBoundary)
        );
        assert_eq!(
            ctx.prepare_summary_span(&ids[0], &ids[2]),
            Err(TranscriptStoreError::NotTurnBoundary)
        );
    }
}
