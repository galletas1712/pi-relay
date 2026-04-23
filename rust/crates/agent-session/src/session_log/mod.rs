use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_core::{CustomMessage, TranscriptRecord};

use crate::transcript::Transcript;

mod compaction;
mod context;
mod entry;

pub use self::compaction::{CompactionPlan, CompactionSettings};
pub use self::entry::{
    branch_summary, compaction_summary, SessionEntry, KIND_BRANCH_SUMMARY, KIND_COMPACTION_SUMMARY,
};

use self::compaction::{
    boundary_start_index, estimate_records_tokens, find_boundary_cut_index, transcript_records_in,
};
use self::context::materialize_context;
use self::entry::is_compaction_summary;

/// Append-only branching log of session records.
///
/// Each `SessionEntry` holds a single `TranscriptRecord` plus DAG pointers
/// (`parent_id`). The log tracks a current leaf; appends attach new entries
/// as children of that leaf, branching operations reparent the leaf onto an
/// existing entry, and `context()` walks the active branch and materializes
/// a `Transcript` for the model.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionLog {
    entries: Vec<SessionEntry>,
    leaf_id: Option<String>,
    next_id: u64,
}

impl SessionLog {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            ..Self::default()
        }
    }

    pub fn from_transcript(transcript: &Transcript) -> Self {
        let mut log = Self::new();
        log.append_transcript_records(transcript.records().iter().cloned());
        log
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    pub fn contains_entry(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.id == id)
    }

    pub fn is_turn_boundary(&self) -> bool {
        self.is_turn_boundary_leaf(self.leaf_id())
    }

    /// True when `leaf_id` points at a turn boundary (either a
    /// `TurnFinished` entry directly, or the empty-log sentinel). `Custom`
    /// entries are transparent — they live between turns, so the check walks
    /// past them to find the underlying `TurnFinished`.
    pub fn is_turn_boundary_leaf(&self, leaf_id: Option<&str>) -> bool {
        let Some(leaf_id) = leaf_id else {
            return true;
        };
        let Some(entry) = self.get_entry(leaf_id) else {
            return false;
        };
        match &entry.record {
            TranscriptRecord::TurnFinished { .. } => true,
            TranscriptRecord::Custom(_) => self.is_turn_boundary_leaf(entry.parent_id.as_deref()),
            _ => false,
        }
    }

    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        self.entries.iter().find(|entry| entry.id == id)
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

    /// Convenience wrapper that appends a compaction-summary Custom entry.
    pub fn append_compaction_summary(
        &mut self,
        content: impl Into<String>,
        first_kept_entry_id: impl Into<String>,
        tokens_before: usize,
    ) -> String {
        self.append_custom(compaction_summary(
            content,
            first_kept_entry_id,
            tokens_before,
        ))
    }

    /// Convenience wrapper that appends a branch-summary Custom entry.
    pub fn append_branch_summary(
        &mut self,
        content: impl Into<String>,
        from_id: Option<String>,
    ) -> String {
        self.append_custom(branch_summary(content, from_id))
    }

    pub fn branch(&mut self, entry_id: &str) -> Result<(), SessionLogError> {
        if !self.contains_entry(entry_id) {
            return Err(SessionLogError::EntryNotFound);
        }
        self.leaf_id = Some(entry_id.to_string());
        Ok(())
    }

    pub fn branch_at_turn_boundary(&mut self, entry_id: &str) -> Result<(), SessionLogError> {
        if !self.contains_entry(entry_id) {
            return Err(SessionLogError::EntryNotFound);
        }
        if !self.is_turn_boundary_leaf(Some(entry_id)) {
            return Err(SessionLogError::NotTurnBoundary);
        }
        self.leaf_id = Some(entry_id.to_string());
        Ok(())
    }

    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    pub fn branch_entries(&self, leaf_id: Option<&str>) -> Vec<SessionEntry> {
        let by_id: HashMap<&str, &SessionEntry> = self
            .entries
            .iter()
            .map(|entry| (entry.id.as_str(), entry))
            .collect();
        let mut path = Vec::new();
        let mut current = leaf_id
            .or(self.leaf_id.as_deref())
            .and_then(|id| by_id.get(id).copied());

        while let Some(entry) = current {
            path.push(entry.clone());
            current = entry
                .parent_id
                .as_deref()
                .and_then(|parent_id| by_id.get(parent_id).copied());
        }

        path.reverse();
        path
    }

    pub fn create_branched_log(&self, leaf_id: &str) -> Result<Self, SessionLogError> {
        if !self.contains_entry(leaf_id) {
            return Err(SessionLogError::EntryNotFound);
        }

        let entries = self.branch_entries(Some(leaf_id));
        Ok(Self {
            leaf_id: Some(leaf_id.to_string()),
            next_id: next_id_after(&entries),
            entries,
        })
    }

    pub fn create_branched_log_at_turn_boundary(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Self, SessionLogError> {
        if !self.is_turn_boundary_leaf(leaf_id) {
            return Err(SessionLogError::NotTurnBoundary);
        }

        match leaf_id {
            Some(leaf_id) => self.create_branched_log(leaf_id),
            None => Ok(Self::new()),
        }
    }

    /// Materialize the active branch into a `Transcript`. The latest
    /// compaction summary (if any) is inlined ahead of the kept suffix; see
    /// `context::materialize_context` for the ordering rationale.
    pub fn context(&self) -> Transcript {
        let path = self.branch_entries(None);
        materialize_context(&path)
    }

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

        let tokens_before = estimate_records_tokens(self.context().records());
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
            leaf_id: self.leaf_id.clone(),
            entry_count: self.entries.len(),
        })
    }

    fn append_record(&mut self, record: TranscriptRecord) -> String {
        let id = self.allocate_id();
        let entry = SessionEntry {
            id: id.clone(),
            parent_id: self.leaf_id.clone(),
            timestamp_ms: now_ms(),
            record,
        };
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        id
    }

    fn allocate_id(&mut self) -> String {
        let id = format!("{:016x}", self.next_id);
        self.next_id += 1;
        id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLogError {
    EntryNotFound,
    NotTurnBoundary,
    StalePlan,
}

fn next_id_after(entries: &[SessionEntry]) -> u64 {
    entries
        .iter()
        .filter_map(|entry| u64::from_str_radix(&entry.id, 16).ok())
        .max()
        .unwrap_or(0)
        + 1
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
    fn log_tracks_a_branch_path_from_the_active_leaf() {
        let mut log = SessionLog::new();
        let first_ids = log.append_transcript_records(turn(1, "first", "done"));
        log.append_transcript_records(turn(2, "second", "done"));

        log.branch(&first_ids[3]).expect("turn one should exist");
        log.append_transcript_records(turn(3, "alternate", "done"));

        let transcript = log.context();
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
    fn compaction_plan_cuts_only_at_turn_boundaries() {
        let mut log = SessionLog::new();
        log.append_transcript_records(turn(1, "first user message", "first assistant message"));
        log.append_transcript_records(turn(2, "second user message", "second assistant message"));
        log.append_transcript_records(turn(3, "third user message", "third assistant message"));

        let plan = log
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
    fn context_applies_latest_compaction_summary_and_kept_suffix() {
        let mut log = SessionLog::new();
        log.append_transcript_records(turn(1, "first", "done"));
        let kept_ids = log.append_transcript_records(turn(2, "second", "done"));

        log.append_compaction_summary("summary", kept_ids[0].clone(), 100);

        let transcript = log.context();
        assert_eq!(transcript.latest_compaction_summary(), Some("summary"));
        assert_eq!(transcript.last_turn_id(), TurnId(2));
        // After reordering the summary precedes the kept suffix: record[0]
        // is the Custom summary, record[1] is the first kept record
        // (TurnStarted for turn 2).
        assert!(matches!(
            transcript.records()[0],
            TranscriptRecord::Custom(_)
        ));
        assert!(matches!(
            transcript.records()[1],
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) }
        ));
        assert!(log.is_turn_boundary());
    }
}
