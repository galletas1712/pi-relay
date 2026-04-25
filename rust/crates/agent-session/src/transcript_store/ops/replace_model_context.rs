use crate::model_context::ModelContext;
use crate::transcript_store::edit::{HistoryEdit, HistoryEditError, HistoryEditKind};
use crate::transcript_store::TranscriptStore;

/// Replace the durable transcript store with entries from a new model context.
///
/// `replacement` must itself end at a turn boundary. The op's `Output` is the
/// previous model context so callers can persist it out-of-band if needed.
pub struct ReplaceModelContext {
    pub replacement: ModelContext,
}

impl HistoryEdit for ReplaceModelContext {
    type Output = ModelContext;
    const KIND: HistoryEditKind = HistoryEditKind::ReplaceModelContext;

    fn apply(self, ctx: &mut TranscriptStore) -> Result<ModelContext, HistoryEditError> {
        if !self.replacement.is_turn_boundary() {
            return Err(HistoryEditError::ReplacementNotAtTurnBoundary);
        }

        let previous = ctx.model_context();
        *ctx = TranscriptStore::from_model_context(&self.replacement);
        Ok(previous)
    }
}
