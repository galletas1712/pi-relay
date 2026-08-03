use agent_vocab::InlineContentBlock;
use rmcp::model::{CallToolResult, RawContent, ResourceContents};

pub(crate) fn normalize_call_result(result: CallToolResult) -> (Vec<InlineContentBlock>, bool) {
    let mut blocks = Vec::new();

    for content in result.content {
        match content.raw {
            RawContent::Text(text) => {
                blocks.push(InlineContentBlock::text(text.text));
            }
            RawContent::Resource(resource) => match resource.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    blocks.push(InlineContentBlock::text(format!("[resource {uri}]")));
                    blocks.push(InlineContentBlock::text(text));
                }
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => blocks.push(InlineContentBlock::text(binary_placeholder(
                    "resource",
                    mime_type.as_deref(),
                    Some(&uri),
                    blob.len(),
                ))),
            },
            RawContent::Image(image) => {
                blocks.push(InlineContentBlock::image(image.mime_type, image.data));
            }
            RawContent::Audio(audio) => blocks.push(InlineContentBlock::text(binary_placeholder(
                "audio",
                Some(&audio.mime_type),
                None,
                audio.data.len(),
            ))),
            RawContent::ResourceLink(resource) => blocks.push(InlineContentBlock::text(format!(
                "[resource link uri={} name={} mime_type={} size={}]",
                resource.uri,
                resource.name,
                resource.mime_type.as_deref().unwrap_or("unknown"),
                resource
                    .size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ))),
        }
    }

    if let Some(structured) = result.structured_content {
        let structured = crate::canonical_json(&structured);
        let json = serde_json::to_string(&structured).unwrap_or_else(|_| "null".to_string());
        blocks.push(InlineContentBlock::text(format!(
            "[structured content]\n{json}"
        )));
    }
    (blocks, result.is_error.unwrap_or(false))
}

fn binary_placeholder(
    kind: &str,
    mime_type: Option<&str>,
    uri: Option<&str>,
    encoded_bytes: usize,
) -> String {
    format!(
        "[{kind} omitted mime_type={}{} encoded_bytes={encoded_bytes}]",
        mime_type.unwrap_or("unknown"),
        uri.map(|uri| format!(" uri={uri}")).unwrap_or_default()
    )
}

#[cfg(test)]
#[path = "result_tests.rs"]
mod tests;
