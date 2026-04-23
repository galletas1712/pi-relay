use agent_core::TranscriptRecord;

use super::entry::{compaction_first_kept_entry_id, is_compaction_summary, SessionEntry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub keep_recent_tokens: usize,
}

/// Describes a compaction the caller may apply to a session log.
///
/// A plan captures everything needed to turn a summary string back into a
/// durable compaction entry: the anchor (`first_kept_entry_id`), the records
/// the summary replaces (`records_to_summarize`), the surviving suffix
/// (`records_to_keep`), the pre-compaction token estimate (`tokens_before`),
/// and the immediate previous summary to thread through when summarizing.
/// `leaf_id` + `entry_count` are staleness-check hooks: the boundary
/// operation refuses to apply a plan if the log's shape has changed since the
/// plan was prepared.
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

/// Compute the starting index for the boundary-cut search.
///
/// If a previous compaction exists on the active branch, we skip everything up
/// to and including its `first_kept_entry_id` — records before that were
/// already evicted under the earlier summary.
pub(super) fn boundary_start_index(path: &[SessionEntry]) -> (usize, Option<&SessionEntry>) {
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

pub(super) fn find_boundary_cut_index(
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

pub(super) fn turn_start_at_or_before(
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

pub(super) fn transcript_records_in(entries: &[SessionEntry]) -> Vec<TranscriptRecord> {
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

pub(super) fn estimate_records_tokens(records: &[TranscriptRecord]) -> usize {
    records.iter().map(estimate_record_tokens).sum()
}

pub(super) fn estimate_record_tokens(record: &TranscriptRecord) -> usize {
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
