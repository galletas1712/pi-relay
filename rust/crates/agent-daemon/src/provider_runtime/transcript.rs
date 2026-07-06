use agent_provider::{normalize_transcript_for_provider, ModelTranscriptEntry};
use agent_session::ModelContext;

pub(super) fn provider_transcript(model_context: &ModelContext) -> Vec<ModelTranscriptEntry> {
    let transcript = model_context
        .entries()
        .map(|(item, provider_replay)| ModelTranscriptEntry {
            item: item.clone(),
            provider_replay: provider_replay.to_vec(),
        })
        .collect();
    normalize_transcript_for_provider(transcript)
}

pub(super) fn provider_transcript_owned(model_context: ModelContext) -> Vec<ModelTranscriptEntry> {
    let transcript = model_context
        .into_entries()
        .into_iter()
        .map(|entry| ModelTranscriptEntry {
            item: entry.item,
            provider_replay: entry.provider_replay,
        })
        .collect();
    normalize_transcript_for_provider(transcript)
}
