use std::collections::BTreeSet;

use agent_provider::{
    normalize_transcript_for_provider, ModelTranscriptEntry, ResolvedImage, ResolvedImageMap,
};
use agent_session::ModelContext;
use agent_store::PostgresAgentStore;
use agent_vocab::{ContentBlock, ImageArtifactId, TranscriptItem};
use anyhow::Result;

pub(super) fn provider_transcript(model_context: ModelContext) -> Vec<ModelTranscriptEntry> {
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

pub(super) async fn resolve_transcript_images(
    repo: &PostgresAgentStore,
    transcript: &[ModelTranscriptEntry],
) -> Result<ResolvedImageMap> {
    let mut artifact_ids = BTreeSet::<ImageArtifactId>::new();
    for entry in transcript {
        let content = match entry.item() {
            TranscriptItem::UserMessage(message) => Some(message.content.as_slice()),
            TranscriptItem::ToolResult(result) => Some(result.content.as_slice()),
            _ => None,
        };
        if let Some(content) = content {
            artifact_ids.extend(content.iter().filter_map(|block| match block {
                ContentBlock::Text { .. } => None,
                ContentBlock::Image { artifact_id } => Some(artifact_id.clone()),
            }));
        }
    }
    Ok(repo
        .load_image_artifacts(artifact_ids)
        .await?
        .into_iter()
        .map(|(artifact_id, artifact)| {
            let data = artifact.base64();
            (
                artifact_id,
                ResolvedImage {
                    mime_type: artifact.metadata.mime_type,
                    data,
                },
            )
        })
        .collect())
}
