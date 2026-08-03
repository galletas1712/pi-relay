use agent_vocab::{inline_content_display_text, InlineContentBlock};
use rmcp::model::{CallToolResult, Content, RawContent, ResourceContents};
use serde_json::json;

use super::*;

#[test]
fn preserves_all_blocks_for_authoritative_ingestion() {
    let mut result = CallToolResult::default();
    result.content = vec![
        Content::new(RawContent::text("first"), None),
        Content::new(
            RawContent::resource(ResourceContents::text("contents", "file:///a")),
            None,
        ),
        Content::new(RawContent::image("not-valid-base64!!!", "image/png"), None),
    ];
    result.structured_content = Some(json!({"z": 1, "a": 2}));
    result.is_error = Some(true);

    let (content, is_error) = normalize_call_result(result);
    assert!(is_error);
    assert_eq!(content[0], InlineContentBlock::text("first"));
    assert!(matches!(&content[1], InlineContentBlock::Text { text } if text.contains("[resource")));
    assert!(matches!(&content[2], InlineContentBlock::Text { text } if text.contains("contents")));
    assert!(
        matches!(&content[3], InlineContentBlock::Image { data, .. } if data == "not-valid-base64!!!")
    );
    assert!(inline_content_display_text(&content).contains("[structured content]"));
}

#[test]
fn normalization_does_not_semantically_truncate_text() {
    let mut result = CallToolResult::default();
    let expected = format!("{}é", "a".repeat(2 * 1024 * 1024));
    result.content = vec![Content::new(RawContent::text(expected.clone()), None)];

    let (content, is_error) = normalize_call_result(result);
    assert!(!is_error);
    assert_eq!(content, vec![InlineContentBlock::text(expected)]);
}
