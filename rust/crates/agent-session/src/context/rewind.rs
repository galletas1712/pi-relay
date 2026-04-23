use agent_core::CustomMessage;

use crate::context::edit::{ContextEdit, HistoryEditError};
use crate::context::Context;

/// Well-known `CustomMessage::kind` for branch summaries.
pub const KIND_BRANCH_SUMMARY: &str = "branch_summary";

/// Build a `CustomMessage` tagged as a branch summary with optional `from_id`
/// anchor metadata.
pub fn branch_summary(content: impl Into<String>, from_id: Option<String>) -> CustomMessage {
    let mut msg = CustomMessage::new(KIND_BRANCH_SUMMARY, content);
    if let Some(from) = from_id {
        msg = msg.with_metadata("from_id", from);
    }
    msg
}

/// A rewind operation.
///
/// `leaf_id = Some(id)` moves the context leaf to `id`, which must point at a
/// `TurnFinished` entry (directly or transparently through `Custom` entries).
/// `leaf_id = None` resets the leaf to the empty-log sentinel.
pub struct Rewind {
    pub leaf_id: Option<String>,
}

impl ContextEdit for Rewind {
    type Output = ();

    fn apply(self, ctx: &mut Context) -> Result<(), HistoryEditError> {
        match self.leaf_id.as_deref() {
            Some(leaf_id) => ctx
                .branch_at_turn_boundary(leaf_id)
                .map_err(HistoryEditError::Context)?,
            None => ctx.reset_leaf(),
        }
        Ok(())
    }
}
