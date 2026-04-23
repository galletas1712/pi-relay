use agent_core::{CustomMessage, TranscriptRecord};

/// DAG entry holding a single `TranscriptRecord`. The DAG is append-only; new
/// entries attach as children of `parent_id`. The session log tracks the
/// currently-active leaf, and path-walking materializes a linear transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp_ms: u128,
    pub record: TranscriptRecord,
}

/// Well-known `CustomMessage::kind` for compaction summaries.
pub const KIND_COMPACTION_SUMMARY: &str = "compaction_summary";

/// Well-known `CustomMessage::kind` for branch summaries.
pub const KIND_BRANCH_SUMMARY: &str = "branch_summary";

/// Build a `CustomMessage` tagged as a compaction summary with the standard
/// `first_kept_entry_id` + `tokens_before` metadata.
pub fn compaction_summary(
    content: impl Into<String>,
    first_kept_entry_id: impl Into<String>,
    tokens_before: usize,
) -> CustomMessage {
    CustomMessage::new(KIND_COMPACTION_SUMMARY, content)
        .with_metadata("first_kept_entry_id", first_kept_entry_id)
        .with_metadata("tokens_before", tokens_before.to_string())
}

/// Build a `CustomMessage` tagged as a branch summary with optional `from_id`
/// anchor metadata.
pub fn branch_summary(content: impl Into<String>, from_id: Option<String>) -> CustomMessage {
    let mut msg = CustomMessage::new(KIND_BRANCH_SUMMARY, content);
    if let Some(from) = from_id {
        msg = msg.with_metadata("from_id", from);
    }
    msg
}

/// True if the record is a `Custom` with kind = `compaction_summary`.
pub fn is_compaction_summary(record: &TranscriptRecord) -> bool {
    matches!(record, TranscriptRecord::Custom(cm) if cm.kind == KIND_COMPACTION_SUMMARY)
}

/// Pull the `first_kept_entry_id` metadata off a compaction summary record.
pub fn compaction_first_kept_entry_id(record: &TranscriptRecord) -> Option<&str> {
    match record {
        TranscriptRecord::Custom(cm) if cm.kind == KIND_COMPACTION_SUMMARY => {
            cm.metadata.get("first_kept_entry_id").map(|s| s.as_str())
        }
        _ => None,
    }
}
