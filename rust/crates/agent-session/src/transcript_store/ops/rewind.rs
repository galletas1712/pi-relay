use crate::transcript_store::edit::{HistoryEdit, HistoryEditError, HistoryEditKind};
use crate::transcript_store::TranscriptStore;

/// A rewind operation.
///
/// `leaf_id = Some(id)` moves the context leaf to `id`, which must point at a
/// `TurnFinished` entry (directly or transparently through injected entries).
/// `leaf_id = None` resets the leaf to the empty-log sentinel.
pub struct Rewind {
    pub leaf_id: Option<String>,
}

impl HistoryEdit for Rewind {
    type Output = ();
    const KIND: HistoryEditKind = HistoryEditKind::Rewind;

    fn apply(self, ctx: &mut TranscriptStore) -> Result<(), HistoryEditError> {
        match self.leaf_id.as_deref() {
            Some(leaf_id) => ctx
                .branch_at_turn_boundary(leaf_id)
                .map_err(HistoryEditError::Store)?,
            None => ctx.reset_leaf(),
        }
        Ok(())
    }
}
