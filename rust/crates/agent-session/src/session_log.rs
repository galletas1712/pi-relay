use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_core::{AssistantItem, TranscriptRecord};

use crate::transcript::Transcript;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp_ms: u128,
    pub kind: SessionEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEntryKind {
    Transcript(TranscriptRecord),
    Injected(InjectedMessage),
}

/// A summary-style message appended to the session log at a boundary and
/// surfaced to the model on the next turn. Carries an `InjectedKind` tag that
/// identifies the boundary it was appended at (e.g. compaction, branch merge)
/// alongside any kind-specific metadata needed to anchor the injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectedMessage {
    pub kind: InjectedKind,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectedKind {
    CompactionSummary {
        first_kept_entry_id: String,
        tokens_before: usize,
    },
    BranchSummary {
        from_id: Option<String>,
    },
}

/// Materialized view of a session log path: the transcript the model sees plus
/// any injected messages (compaction summaries, branch summaries) that sit on
/// the active branch from the latest compaction forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionContext {
    pub transcript: Transcript,
    pub injections: Vec<InjectedMessage>,
}

impl SessionContext {
    /// Latest compaction summary on the active branch, if any. Used by callers
    /// that need to anchor the "everything before this" boundary.
    pub fn latest_compaction(&self) -> Option<&InjectedMessage> {
        self.injections
            .iter()
            .rev()
            .find(|msg| matches!(msg.kind, InjectedKind::CompactionSummary { .. }))
    }

    pub fn branch_summaries(&self) -> impl Iterator<Item = &InjectedMessage> {
        self.injections
            .iter()
            .filter(|msg| matches!(msg.kind, InjectedKind::BranchSummary { .. }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub keep_recent_tokens: usize,
}

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

    pub fn is_turn_boundary_leaf(&self, leaf_id: Option<&str>) -> bool {
        let Some(leaf_id) = leaf_id else {
            return true;
        };
        let Some(entry) = self.get_entry(leaf_id) else {
            return false;
        };

        match &entry.kind {
            SessionEntryKind::Transcript(TranscriptRecord::TurnFinished { .. }) => true,
            SessionEntryKind::Injected(_) => self.is_turn_boundary_leaf(entry.parent_id.as_deref()),
            SessionEntryKind::Transcript(
                TranscriptRecord::TurnStarted { .. }
                | TranscriptRecord::UserMessage(_)
                | TranscriptRecord::AssistantMessage(_)
                | TranscriptRecord::ToolCallStarted { .. }
                | TranscriptRecord::ToolResult(_),
            ) => false,
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
            .map(|record| self.append_kind(SessionEntryKind::Transcript(record)))
            .collect()
    }

    pub fn append_compaction(
        &mut self,
        summary: impl Into<String>,
        first_kept_entry_id: impl Into<String>,
        tokens_before: usize,
    ) -> String {
        self.append_kind(SessionEntryKind::Injected(InjectedMessage {
            kind: InjectedKind::CompactionSummary {
                first_kept_entry_id: first_kept_entry_id.into(),
                tokens_before,
            },
            content: summary.into(),
        }))
    }

    pub fn append_branch_summary(
        &mut self,
        from_id: Option<String>,
        summary: impl Into<String>,
    ) -> String {
        self.append_kind(SessionEntryKind::Injected(InjectedMessage {
            kind: InjectedKind::BranchSummary { from_id },
            content: summary.into(),
        }))
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

    pub fn context(&self) -> SessionContext {
        let path = self.branch_entries(None);
        materialize_context(&path)
    }

    pub fn prepare_compaction(&self, settings: CompactionSettings) -> Option<CompactionPlan> {
        let path = self.branch_entries(None);
        if matches!(
            path.last().map(|entry| &entry.kind),
            Some(SessionEntryKind::Injected(InjectedMessage {
                kind: InjectedKind::CompactionSummary { .. },
                ..
            }))
        ) {
            return None;
        }

        let previous_compaction_index = path.iter().rposition(|entry| {
            matches!(
                &entry.kind,
                SessionEntryKind::Injected(InjectedMessage {
                    kind: InjectedKind::CompactionSummary { .. },
                    ..
                })
            )
        });
        let previous_compaction =
            previous_compaction_index.and_then(|index| match &path[index].kind {
                SessionEntryKind::Injected(
                    msg @ InjectedMessage {
                        kind: InjectedKind::CompactionSummary { .. },
                        ..
                    },
                ) => Some(msg.clone()),
                _ => None,
            });
        let boundary_start = previous_compaction
            .as_ref()
            .and_then(|msg| match &msg.kind {
                InjectedKind::CompactionSummary {
                    first_kept_entry_id,
                    ..
                } => path
                    .iter()
                    .position(|entry| entry.id == *first_kept_entry_id),
                _ => None,
            })
            .or_else(|| previous_compaction_index.map(|index| index + 1))
            .unwrap_or(0);

        let tokens_before = estimate_records_tokens(self.context().transcript.records());
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
            previous_summary: previous_compaction.map(|msg| msg.content),
            leaf_id: self.leaf_id.clone(),
            entry_count: self.entries.len(),
        })
    }

    fn append_kind(&mut self, kind: SessionEntryKind) -> String {
        let id = self.allocate_id();
        let entry = SessionEntry {
            id: id.clone(),
            parent_id: self.leaf_id.clone(),
            timestamp_ms: now_ms(),
            kind,
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

fn materialize_context(path: &[SessionEntry]) -> SessionContext {
    let latest_compaction_index = path.iter().rposition(|entry| {
        matches!(
            &entry.kind,
            SessionEntryKind::Injected(InjectedMessage {
                kind: InjectedKind::CompactionSummary { .. },
                ..
            })
        )
    });

    let start = match latest_compaction_index {
        Some(index) => {
            // Everything strictly before the latest compaction is replaced by
            // its summary. Anchor the kept suffix at `first_kept_entry_id`; the
            // compaction entry itself falls into the kept range so the summary
            // is surfaced via `injections`.
            let SessionEntryKind::Injected(msg) = &path[index].kind else {
                unreachable!()
            };
            let InjectedKind::CompactionSummary {
                first_kept_entry_id,
                ..
            } = &msg.kind
            else {
                unreachable!()
            };
            path.iter()
                .position(|entry| &entry.id == first_kept_entry_id)
                .unwrap_or(index + 1)
        }
        None => 0,
    };

    let mut records = Vec::new();
    let mut injections = Vec::new();

    for entry in &path[start..] {
        match &entry.kind {
            SessionEntryKind::Transcript(record) => records.push(record.clone()),
            SessionEntryKind::Injected(msg) => injections.push(msg.clone()),
        }
    }

    SessionContext {
        transcript: Transcript::from_records(records),
        injections,
    }
}

fn transcript_records_in(entries: &[SessionEntry]) -> Vec<TranscriptRecord> {
    entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            SessionEntryKind::Transcript(record) => Some(record.clone()),
            SessionEntryKind::Injected(_) => None,
        })
        .collect()
}

fn find_boundary_cut_index(
    path: &[SessionEntry],
    boundary_start: usize,
    keep_recent_tokens: usize,
) -> Option<usize> {
    let mut accumulated_tokens = 0;

    for index in (boundary_start..path.len()).rev() {
        let SessionEntryKind::Transcript(record) = &path[index].kind else {
            continue;
        };
        accumulated_tokens += estimate_record_tokens(record);
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
        if matches!(
            path[candidate].kind,
            SessionEntryKind::Transcript(TranscriptRecord::TurnStarted { .. })
        ) {
            return Some(candidate);
        }
    }
    Some(boundary_start)
}

fn estimate_records_tokens(records: &[TranscriptRecord]) -> usize {
    records.iter().map(estimate_record_tokens).sum()
}

fn estimate_record_tokens(record: &TranscriptRecord) -> usize {
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
    };
    chars.div_ceil(4)
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
    use agent_core::{AssistantMessage, TurnId, TurnOutcome};

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

        let context = log.context();
        assert_eq!(context.transcript.last_turn_id(), TurnId(3));
        assert_eq!(
            context.transcript.records()[1],
            TranscriptRecord::UserMessage("first".to_string())
        );
        assert_eq!(
            context.transcript.records()[5],
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

        log.append_compaction("summary", kept_ids[0].clone(), 100);

        let context = log.context();
        assert_eq!(
            context.latest_compaction().map(|msg| msg.content.as_str()),
            Some("summary")
        );
        assert_eq!(context.transcript.last_turn_id(), TurnId(2));
        assert_eq!(
            context.transcript.records()[1],
            TranscriptRecord::UserMessage("second".to_string())
        );
        assert!(log.is_turn_boundary());
    }
}
