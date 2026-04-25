use std::collections::{BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_core::{InjectedMessage, TranscriptRecord};
use uuid::Uuid;

use crate::transcript::Transcript;

pub mod edit;
pub(crate) mod ops;
pub(crate) mod span;
pub(crate) mod tokens;

pub use self::edit::{ContextEdit, HistoryEditError, PendingWork};
pub use self::ops::compaction::{
    compaction_summary, Compact, CompactionPlan, CompactionSettings, KIND_COMPACTION_SUMMARY,
};
pub use self::ops::replace::ReplaceTranscript;
pub use self::ops::rewind::Rewind;
pub use self::span::{SummarizeSpan, SummarySpanPlan};

/// Durable transcript entry holding one model-visible context item.
///
/// Entries form a forest: each entry has at most one parent, while a parent may
/// have many children. A session points at one leaf and materializes model
/// context by walking parents from that leaf back to a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp_ms: u128,
    pub record: TranscriptRecord,
}

/// Back-compat name for `TranscriptEntry`.
pub type SessionEntry = TranscriptEntry;

/// Append-only transcript forest plus one active session leaf.
///
/// Each `TranscriptEntry` holds a single `TranscriptRecord` plus a parent
/// pointer. The store keeps direct indexes by entry id, parent id, and current
/// leaves so future registry/storage layers can discover sibling paths and
/// common ancestors quickly. The active leaf is the one path this session is
/// currently using.
///
/// `Context` remains as a compatibility alias for this type while callers move
/// toward the clearer transcript-store/model-context vocabulary.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TranscriptStore {
    entries_by_id: HashMap<String, TranscriptEntry>,
    parent_by_id: HashMap<String, Option<String>>,
    children_by_parent: HashMap<Option<String>, Vec<String>>,
    leaf_ids: BTreeSet<String>,
    insertion_order: Vec<String>,
    active_leaf_id: Option<String>,
}

/// Back-compat name for `TranscriptStore`.
pub type Context = TranscriptStore;

impl TranscriptStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_transcript(transcript: &Transcript) -> Self {
        let mut ctx = Self::new();
        ctx.append_transcript_records(transcript.records().iter().cloned());
        ctx
    }

    /// Return all transcript entries in append order.
    ///
    /// This is an owned snapshot because the store indexes entries by id
    /// internally. Future persistence code can serialize this vector together
    /// with `leaf_id()`.
    pub fn entries(&self) -> Vec<TranscriptEntry> {
        self.insertion_order
            .iter()
            .filter_map(|id| self.entries_by_id.get(id).cloned())
            .collect()
    }

    pub fn entry_count(&self) -> usize {
        self.insertion_order.len()
    }

    pub fn leaf_ids(&self) -> impl Iterator<Item = &str> {
        self.leaf_ids.iter().map(String::as_str)
    }

    pub fn parent_id(&self, entry_id: &str) -> Option<Option<&str>> {
        self.parent_by_id
            .get(entry_id)
            .map(|parent_id| parent_id.as_deref())
    }

    pub fn child_ids(&self, parent_id: Option<&str>) -> Vec<&str> {
        let key = parent_id.map(str::to_string);
        self.children_by_parent
            .get(&key)
            .into_iter()
            .flat_map(|ids| ids.iter().map(String::as_str))
            .collect()
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.active_leaf_id.as_deref()
    }

    pub fn contains_entry(&self, id: &str) -> bool {
        self.entries_by_id.contains_key(id)
    }

    pub fn is_turn_boundary(&self) -> bool {
        self.is_turn_boundary_leaf(self.leaf_id())
    }

    /// True when `leaf_id` points at a turn boundary (either a
    /// `TurnFinished` entry directly, or the empty-log sentinel). Trailing
    /// injected entries are transparent: the check walks past them to find the
    /// underlying boundary. An injected turn opener still resolves to
    /// `TurnStarted`, so it is not a boundary.
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
                TranscriptRecord::Injected(_) => {
                    cursor = entry.parent_id.as_deref();
                }
                _ => return false,
            }
        }
    }

    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        self.entries_by_id.get(id)
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

    /// Append a `TranscriptRecord::Injected(injected)` entry and return its id.
    pub fn append_injected(&mut self, injected: InjectedMessage) -> String {
        self.append_record(TranscriptRecord::Injected(injected))
    }

    pub fn branch(&mut self, entry_id: &str) -> Result<(), ContextError> {
        if !self.contains_entry(entry_id) {
            return Err(ContextError::EntryNotFound);
        }
        self.active_leaf_id = Some(entry_id.to_string());
        Ok(())
    }

    pub fn branch_at_turn_boundary(&mut self, entry_id: &str) -> Result<(), ContextError> {
        if !self.contains_entry(entry_id) {
            return Err(ContextError::EntryNotFound);
        }
        if !self.is_turn_boundary_leaf(Some(entry_id)) {
            return Err(ContextError::NotTurnBoundary);
        }
        self.active_leaf_id = Some(entry_id.to_string());
        Ok(())
    }

    pub fn reset_leaf(&mut self) {
        self.active_leaf_id = None;
    }

    pub fn branch_entries(&self, leaf_id: Option<&str>) -> Vec<SessionEntry> {
        let mut path = Vec::new();
        let mut current = leaf_id
            .or(self.active_leaf_id.as_deref())
            .and_then(|id| self.entries_by_id.get(id));

        while let Some(entry) = current {
            path.push(entry.clone());
            current = entry
                .parent_id
                .as_deref()
                .and_then(|parent_id| self.entries_by_id.get(parent_id));
        }

        path.reverse();
        path
    }

    pub fn create_branched_context(&self, leaf_id: &str) -> Result<Self, ContextError> {
        if !self.contains_entry(leaf_id) {
            return Err(ContextError::EntryNotFound);
        }

        Ok(Self::from_entries(
            self.branch_entries(Some(leaf_id)),
            Some(leaf_id.to_string()),
        ))
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
    /// Summary-span edits rebuild the active branch in model-visible order:
    /// prefix before the summarized span, the summary record, then copies of
    /// the suffix after the summarized span. Materialization is therefore the
    /// full active path.
    pub fn transcript(&self) -> Transcript {
        let path = self.branch_entries(None);
        let records = path.into_iter().map(|e| e.record).collect();
        Transcript::from_records(records)
    }

    pub(crate) fn append_record(&mut self, record: TranscriptRecord) -> String {
        let entry = TranscriptEntry {
            id: Uuid::new_v4().to_string(),
            parent_id: self.active_leaf_id.clone(),
            timestamp_ms: now_ms(),
            record,
        };
        self.append_entry(entry)
    }

    fn append_entry(&mut self, entry: TranscriptEntry) -> String {
        let id = entry.id.clone();
        let parent_id = entry.parent_id.clone();
        self.parent_by_id.insert(id.clone(), parent_id.clone());
        self.children_by_parent
            .entry(parent_id.clone())
            .or_default()
            .push(id.clone());
        if let Some(parent_id) = parent_id {
            self.leaf_ids.remove(&parent_id);
        }
        self.leaf_ids.insert(id.clone());
        self.insertion_order.push(id.clone());
        self.active_leaf_id = Some(id.clone());
        self.entries_by_id.insert(id.clone(), entry);
        id
    }

    fn from_entries(entries: Vec<TranscriptEntry>, active_leaf_id: Option<String>) -> Self {
        let mut ctx = Self::new();
        for entry in entries {
            ctx.append_entry(entry);
        }
        ctx.active_leaf_id = active_leaf_id;
        ctx
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextError {
    EntryNotFound,
    InvalidSpan,
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
    use crate::context::compaction_summary;
    use agent_core::{AssistantItem, AssistantMessage, InjectedMessage, TurnId, TurnOutcome};

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
    fn context_indexes_children_and_leaves_for_alternate_paths() {
        let mut ctx = Context::new();
        let first_ids = ctx.append_transcript_records(turn(1, "first", "done"));
        let original_second_ids = ctx.append_transcript_records(turn(2, "second", "done"));

        ctx.branch_at_turn_boundary(&first_ids[3])
            .expect("T1 boundary is a valid fork point");
        let alternate_second_ids = ctx.append_transcript_records(turn(3, "alternate", "done"));

        let children = ctx.child_ids(Some(&first_ids[3]));
        assert!(children.contains(&original_second_ids[0].as_str()));
        assert!(children.contains(&alternate_second_ids[0].as_str()));

        let leaves = ctx.leaf_ids().collect::<Vec<_>>();
        assert!(leaves.contains(&original_second_ids[3].as_str()));
        assert!(leaves.contains(&alternate_second_ids[3].as_str()));
        assert_eq!(
            ctx.parent_id(&alternate_second_ids[0]),
            Some(Some(first_ids[3].as_str()))
        );
    }

    #[test]
    fn transcript_materializes_the_full_active_branch_after_a_summary() {
        // Simulate a summary-span edit manually at the context level: append
        // two turns, navigate back to the T1 boundary, append a summary there,
        // then re-append T2's records as descendants. The active branch is now
        // [T1 records..., summary, T2 records...], and the materialized view is
        // that full active path.
        let mut ctx = Context::new();
        let first_ids = ctx.append_transcript_records(turn(1, "first", "done"));
        let second_ids = ctx.append_transcript_records(turn(2, "second", "done"));
        let kept_records = second_ids
            .iter()
            .map(|id| ctx.get_entry(id).expect("kept id exists").record.clone())
            .collect::<Vec<_>>();

        ctx.branch_at_turn_boundary(&first_ids[3])
            .expect("T1 boundary is a valid fork point");
        ctx.append_injected(compaction_summary("summary", second_ids[0].clone(), 100));
        ctx.append_transcript_records(kept_records);

        let transcript = ctx.transcript();
        assert_eq!(transcript.latest_compaction_summary(), Some("summary"));
        assert_eq!(transcript.last_turn_id(), TurnId(2));
        assert!(matches!(
            transcript.records()[4],
            TranscriptRecord::Injected(_)
        ));
        assert!(matches!(
            transcript.records()[5],
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) }
        ));
        assert_eq!(transcript.records().len(), 9);
        assert!(ctx.is_turn_boundary());
    }

    #[test]
    fn fork_at_injected_tail_is_a_valid_turn_boundary() {
        let mut ctx = Context::new();
        ctx.append_transcript_records(turn(1, "hi", "done"));
        let injected_id = ctx.append_injected(InjectedMessage::new("note", "note"));

        assert!(ctx.is_turn_boundary());
        let forked = ctx
            .create_branched_context_at_turn_boundary(Some(&injected_id))
            .expect("injected tail should be a valid fork boundary");
        assert_eq!(forked.leaf_id(), Some(injected_id.as_str()));
    }
}
