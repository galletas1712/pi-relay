use crate::context::edit::{ContextEdit, HistoryEditError};
use crate::context::Context;
use crate::transcript::Transcript;

/// Replace the durable context with a new transcript.
///
/// `replacement` must itself end at a turn boundary. The op's `Output` is the
/// previous transcript so callers can persist it out-of-band if needed.
pub struct ReplaceTranscript {
    pub replacement: Transcript,
}

impl ContextEdit for ReplaceTranscript {
    type Output = Transcript;

    fn apply(self, ctx: &mut Context) -> Result<Transcript, HistoryEditError> {
        if !self.replacement.is_turn_boundary() {
            return Err(HistoryEditError::ReplacementNotAtTurnBoundary);
        }

        let previous = ctx.transcript();
        *ctx = Context::from_transcript(&self.replacement);
        Ok(previous)
    }
}
