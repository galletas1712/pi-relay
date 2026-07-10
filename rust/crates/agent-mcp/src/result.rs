use rmcp::model::{CallToolResult, RawContent, ResourceContents};

const MAX_NORMALIZED_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const TRUNCATION_MARKER: &str = "\n[output truncated by MCP client]";

pub(crate) fn normalize_call_result(result: CallToolResult) -> (String, bool) {
    let mut output = String::new();
    let mut truncated = false;
    for content in result.content {
        if truncated {
            break;
        }
        match content.raw {
            RawContent::Text(text) => append_part(&mut output, &text.text, &mut truncated),
            RawContent::Resource(resource) => match resource.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    append_part(&mut output, &format!("[resource {uri}]"), &mut truncated);
                    if !truncated {
                        append_part(&mut output, &text, &mut truncated);
                    }
                }
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => append_part(
                    &mut output,
                    &binary_placeholder("resource", mime_type.as_deref(), Some(&uri), blob.len()),
                    &mut truncated,
                ),
            },
            RawContent::Image(image) => append_part(
                &mut output,
                &binary_placeholder("image", Some(&image.mime_type), None, image.data.len()),
                &mut truncated,
            ),
            RawContent::Audio(audio) => append_part(
                &mut output,
                &binary_placeholder("audio", Some(&audio.mime_type), None, audio.data.len()),
                &mut truncated,
            ),
            RawContent::ResourceLink(resource) => append_part(
                &mut output,
                &format!(
                    "[resource link uri={} name={} mime_type={} size={}]",
                    resource.uri,
                    resource.name,
                    resource.mime_type.as_deref().unwrap_or("unknown"),
                    resource
                        .size
                        .map(|size| size.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                ),
                &mut truncated,
            ),
        }
    }
    if !truncated {
        if let Some(structured) = result.structured_content {
            let structured = crate::canonical_json(&structured);
            let json = serde_json::to_string(&structured).unwrap_or_else(|_| "null".to_string());
            append_part(
                &mut output,
                &format!("[structured content]\n{json}"),
                &mut truncated,
            );
        }
    }
    if truncated {
        output.push_str(TRUNCATION_MARKER);
    }
    (output, result.is_error.unwrap_or(false))
}

fn append_part(output: &mut String, part: &str, truncated: &mut bool) {
    let separator_bytes = usize::from(!output.is_empty());
    let remaining = MAX_NORMALIZED_OUTPUT_BYTES
        .saturating_sub(TRUNCATION_MARKER.len())
        .saturating_sub(output.len())
        .saturating_sub(separator_bytes);
    if !output.is_empty() && remaining > 0 {
        output.push('\n');
    }
    if part.len() <= remaining {
        output.push_str(part);
        return;
    }
    let mut end = remaining.min(part.len());
    while !part.is_char_boundary(end) {
        end -= 1;
    }
    output.push_str(&part[..end]);
    *truncated = true;
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
