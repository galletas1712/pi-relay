use rmcp::model::{CallToolResult, Content, RawContent, ResourceContents};
use serde_json::json;

use super::*;

#[test]
fn normalizes_ordered_text_resources_and_structured_content_without_meta() {
    let mut result = CallToolResult::default();
    result.content = vec![
        Content::new(RawContent::text("first"), None),
        Content::new(
            RawContent::resource(ResourceContents::text("contents", "file:///a")),
            None,
        ),
        Content::new(RawContent::image("YWJj", "image/png"), None),
    ];
    result.structured_content = Some(json!({"z": 1, "a": 2}));
    result.is_error = Some(true);
    result.meta = Some(Default::default());

    assert_eq!(
        normalize_call_result(result),
        (
            "first\n[resource file:///a]\ncontents\n[image omitted mime_type=image/png encoded_bytes=4]\n[structured content]\n{\"a\":2,\"z\":1}".to_string(),
            true
        )
    );
}

#[test]
fn truncates_at_a_utf8_boundary_without_exceeding_the_cap() {
    let mut result = CallToolResult::default();
    result.content = vec![Content::new(
        RawContent::text(format!(
            "{}é",
            "a".repeat(MAX_NORMALIZED_OUTPUT_BYTES - TRUNCATION_MARKER.len() - 1)
        )),
        None,
    )];

    let (output, is_error) = normalize_call_result(result);

    assert!(!is_error);
    assert!(output.is_char_boundary(output.len()));
    assert!(output.ends_with(TRUNCATION_MARKER));
    assert!(output.len() <= MAX_NORMALIZED_OUTPUT_BYTES);
}
