use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_core::{CustomMessage, TranscriptRecord};
use uuid::Uuid;

use crate::transcript::Transcript;

pub mod compaction;
pub mod edit;
pub mod replace;
pub mod rewind;

pub use self::compaction::{Compact, CompactionPlan, CompactionSettings, KIND_COMPACTION_SUMMARY};
pub use self::edit::{ContextEdit, HistoryEditError, PendingWork};
pub use self::replace::ReplaceTranscript;
pub use self::rewind::{Rewind, KIND_BRANCH_SUMMARY};

/// DAG entry holding a single `TranscriptRecord`. The DAG is append-only; new
/// entries attach as children of `parent_id`. The context tracks the
/// currently-active leaf, and path-walking materializes a linear transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp_ms: u128,
    pub record: TranscriptRecord,
}

/// Append-only branching log of session records — the durable session context.
///
/// Each `SessionEntry` holds a single `TranscriptRecord` plus DAG pointers
/// (`parent_id`). The context tracks a current leaf; appends attach new
/// entries as children of that leaf, branching operations reparent the leaf
/// onto an existing entry, and `transcript()` walks the active branch and
/// materializes a `Transcript` for the model.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Context {
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,
    leaf_id: Option<String>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_transcript(transcript: &Transcript) -> Self {
        let mut ctx = Self::new();
        ctx.append_transcript_records(transcript.records().iter().cloned());
        ctx
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    pub fn contains_entry(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

    pub fn is_turn_boundary(&self) -> bool {
        self.is_turn_boundary_leaf(self.leaf_id())
    }

    /// True when `leaf_id` points at a turn boundary (either a
    /// `TurnFinished` entry directly, or the empty-log sentinel). `Custom`
    /// entries are transparent — they live between turns, so the check walks
    /// past them to find the underlying `TurnFinished`.
    pub fn is_turn_boundary_leaf<'a>(&'a self, leaf_id: Option<&'a str>) -> bool {
        let mut cursor = leaf_id;
        loop {
            let Some(id) = cursor else {
                return true;
            };
            let Some(entry) = self.get_entry(id) else {
                return false;
            };
            match &entry.record {
                TranscriptRecord::TurnFinished { .. } => return true,
                TranscriptRecord::Custom(_) => {
                    cursor = entry.parent_id.as_deref();
                }
                _ => return false,
            }
        }
    }

    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id.get(id).and_then(|&i| self.entries.get(i))
    }

    pub fn append_transcript_records(
        &mut self,
        records: impl IntoIterator<Item = TranscriptRecord>,
    ) -> Vec<String> {
        records
            .into_iter()
            .map(|record| self.append_record(record))
            .collect()
    }

    /// Append a `TranscriptRecord::Custom(custom)` entry and return its id.
    pub fn append_custom(&mut self, custom: CustomMessage) -> String {
        self.append_record(TranscriptRecord::Custom(custom))
    }

    pub fn branch(&mut self, entry_id: &str) -> Result<(), ContextError> {
        if !self.contains_entry(entry_id) {
            return Err(ContextError::EntryNotFound);
        }
        self.leaf_id = Some(entry_id.to_string());
        Ok(())
    }

    pub fn branch_at_turn_boundary(&mut self, entry_id: &str) -> Result<(), ContextError> {
        if !self.contains_entry(entry_id) {
            return Err(ContextError::EntryNotFound);
        }
        if !self.is_turn_boundary_leaf(Some(entry_id)) {
            return Err(ContextError::NotTurnBoundary);
        }
        self.leaf_id = Some(entry_id.to_string());
        Ok(())
    }

    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    pub fn branch_entries(&self, leaf_id: Option<&str>) -> Vec<SessionEntry> {
        let mut path = Vec::new();
        let mut current = leaf_id
            .or(self.leaf_id.as_deref())
            .and_then(|id| self.by_id.get(id))
            .and_then(|&i| self.entries.get(i));

        while let Some(entry) = current {
            path.push(entry.clone());
            current = entry
                .parent_id
                .as_deref()
                .and_then(|parent_id| self.by_id.get(parent_id))
                .and_then(|&i| self.entries.get(i));
        }

        path.reverse();
        path
    }

    pub fn create_branched_context(&self, leaf_id: &str) -> Result<Self, ContextError> {
        if !self.contains_entry(leaf_id) {
            return Err(ContextError::EntryNotFound);
        }

        let entries = self.branch_entries(Some(leaf_id));
        let by_id = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        Ok(Self {
            leaf_id: Some(leaf_id.to_string()),
            by_id,
            entries,
        })
    }

    pub fn create_branched_context_at_turn_boundary(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Self, ContextError> {
        if !self.is_turn_boundary_leaf(leaf_id) {
            return Err(ContextError::NotTurnBoundary);
        }

        match leaf_id {
            Some(leaf_id) => self.create_branched_context(leaf_id),
            None => Ok(Self::new()),
        }
    }

    /// Materialize the active branch into a `Transcript`.
    ///
    /// Under fork-based compaction the chronological order on the active
    /// branch already matches the model's semantic view (CompSum followed by
    /// the kept records re-appended as its descendants), so materialization
    /// is a plain slice starting at the latest CompSum entry. See
    /// `compaction::materialize_context` for details.
    pub fn transcript(&self) -> Transcript {
        let path = self.branch_entries(None);
        self::compaction::materialize_context(&path)
    }

    pub(crate) fn append_record(&mut self, record: TranscriptRecord) -> String {
        let entry = SessionEntry {
            id: Uuid::new_v4().to_string(),
            parent_id: self.leaf_id.clone(),
            timestamp_ms: now_ms(),
            record,
        };
        self.append_entry(entry)
    }

    fn append_entry(&mut self, entry: SessionEntry) -> String {
        let id = entry.id.clone();
        self.by_id.insert(id.clone(), self.entries.len());
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextError {
    EntryNotFound,
    NotTurnBoundary,
    StalePlan,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::compaction::compaction_summary;
    use crate::context::rewind::branch_summary;
    use agent_core::{AssistantItem, AssistantMessage, TurnId, TurnOutcome};

    fn turn(turn_id: u64, user: &str, assistant: &str) -> Vec<TranscriptRecord> {
        vec![
            TranscriptRecord::TurnStarted {
                turn_id: TurnId(turn_id),
            },
            TranscriptRecord::UserMessage(user.to_string()),
            TranscriptRecord::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text(assistant.to_string())],
            }),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(turn_id),
                outcome: TurnOutcome::Graceful,
            },
        ]
    }

    #[test]
    fn context_tracks_a_branch_path_from_the_active_leaf() {
        let mut ctx = Context::new();
        let first_ids = ctx.append_transcript_records(turn(1, "first", "done"));
        ctx.append_transcript_records(turn(2, "second", "done"));

        ctx.branch(&first_ids[3]).expect("turn one should exist");
        ctx.append_transcript_records(turn(3, "alternate", "done"));

        let transcript = ctx.transcript();
        assert_eq!(transcript.last_turn_id(), TurnId(3));
        assert_eq!(
            transcript.records()[1],
            TranscriptRecord::UserMessage("first".to_string())
        );
        assert_eq!(
            transcript.records()[5],
            TranscriptRecord::UserMessage("alternate".to_string())
        );
    }

    #[test]
    fn transcript_starts_at_the_latest_compaction_summary_on_the_active_branch() {
        // Simulate a fork-based compaction manually at the context level:
        // append two turns, navigate back to the T1 boundary, append a
        // CompSum there, then re-append T2's records as descendants. The
        // active branch is now [T1 records..., CompSum, T2 records...]; the
        // materialized view should start at CompSum.
        let mut ctx = Context::new();
        let first_ids = ctx.append_transcript_records(turn(1, "first", "done"));
        let second_ids = ctx.append_transcript_records(turn(2, "second", "done"));
        let kept_records = second_ids
            .iter()
            .map(|id| ctx.get_entry(id).expect("kept id exists").record.clone())
            .collect::<Vec<_>>();

        ctx.branch_at_turn_boundary(&first_ids[3])
            .expect("T1 boundary is a valid fork point");
        ctx.append_custom(compaction_summary("summary", second_ids[0].clone(), 100));
        ctx.append_transcript_records(kept_records);

        let transcript = ctx.transcript();
        assert_eq!(transcript.latest_compaction_summary(), Some("summary"));
        assert_eq!(transcript.last_turn_id(), TurnId(2));
        assert!(matches!(
            transcript.records()[0],
            TranscriptRecord::Custom(_)
        ));
        assert!(matches!(
            transcript.records()[1],
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) }
        ));
        assert_eq!(transcript.records().len(), 5);
        assert!(ctx.is_turn_boundary());
    }

    #[test]
    fn fork_at_custom_tail_is_a_valid_turn_boundary() {
        let mut ctx = Context::new();
        ctx.append_transcript_records(turn(1, "hi", "done"));
        let custom_id = ctx.append_custom(branch_summary("note", None));

        assert!(ctx.is_turn_boundary());
        let forked = ctx
            .create_branched_context_at_turn_boundary(Some(&custom_id))
            .expect("Custom tail should be a valid fork boundary");
        assert_eq!(forked.leaf_id(), Some(custom_id.as_str()));
    }
}
