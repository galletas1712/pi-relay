use crate::context::edit::{ContextEdit, HistoryEditError};
use crate::context::Context;

/// A rewind operation.
///
/// `leaf_id = Some(id)` moves the context leaf to `id`, which must point at a
/// `TurnFinished` entry (directly or transparently through injected entries).
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
