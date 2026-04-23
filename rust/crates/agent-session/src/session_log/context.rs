use crate::transcript::Transcript;

use super::entry::{is_compaction_summary, SessionEntry};

/// Materialize the model-visible transcript for a session-log path.
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
