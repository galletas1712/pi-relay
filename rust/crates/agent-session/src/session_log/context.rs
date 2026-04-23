use crate::transcript::Transcript;

use super::entry::{compaction_first_kept_entry_id, is_compaction_summary, SessionEntry};

/// Materialize the model-visible transcript for a session-log path.
///
/// The log is append-only and chronological, but the materialized view
/// deliberately reorders so the model sees the compaction summary *before*
/// the turns it replaces. This mirrors the earlier `SessionContext` shape
/// where the compaction was surfaced as a preamble separate from the
/// transcript tail; inlining it at position 0 here keeps that preamble
/// semantics while collapsing the two fields into a single `Transcript`.
///
/// With a compaction present, the materialized order is:
/// 1. The compaction summary record.
/// 2. Records chronologically after `first_kept_entry_id` up to (but
///    excluding) the compaction entry.
/// 3. Records chronologically after the compaction entry.
pub(crate) fn materialize_context(path: &[SessionEntry]) -> Transcript {
    let latest_compaction_idx = path.iter().rposition(|e| is_compaction_summary(&e.record));

    if let Some(cidx) = latest_compaction_idx {
        let first_kept_idx = compaction_first_kept_entry_id(&path[cidx].record)
            .and_then(|fk| path.iter().position(|e| e.id == fk))
            .unwrap_or(cidx + 1);

        let mut records = Vec::with_capacity(path.len());
        records.push(path[cidx].record.clone());
        records.extend(path[first_kept_idx..cidx].iter().map(|e| e.record.clone()));
        records.extend(path[cidx + 1..].iter().map(|e| e.record.clone()));
        Transcript::from_records(records)
    } else {
        let records = path.iter().map(|e| e.record.clone()).collect();
        Transcript::from_records(records)
    }
}
