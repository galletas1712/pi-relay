use super::*;
use agent_vocab::{AssistantItem, ToolCall, ToolCallId};
use serde_json::json;

#[test]
fn sanitize_title_trims_quotes_and_punctuation() {
    assert_eq!(
        sanitize_title("  \"Production deploy notes.\"  "),
        Some("Production deploy notes".to_string())
    );
}

#[test]
fn sanitize_title_rejects_secret_like_titles() {
    assert_eq!(sanitize_title("sk-1234567890abcdef"), None);
    assert_eq!(sanitize_title("api_key: abc123"), None);
    assert_eq!(
        sanitize_title("API key rotation"),
        Some("API key rotation".to_string())
    );
}

#[test]
fn title_from_response_uses_rename_tool_call() {
    let items = vec![
        AssistantItem::Text("ignored".to_string()),
        AssistantItem::ToolCall(ToolCall {
            id: ToolCallId::new("call_1"),
            tool_name: TITLE_TOOL_NAME.to_string(),
            args_json: json!({ "title": "Debug flaky tests" }).to_string(),
        }),
    ];

    assert_eq!(
        title_from_response(&items),
        Some("Debug flaky tests".to_string())
    );
}

#[test]
fn title_sidecar_session_id_is_short_and_distinct_from_main_session() {
    let id = title_sidecar_session_id("session_00000000-0000-0000-0000-000000000000");

    assert!(id.len() <= 64);
    assert!(id.starts_with("title-session_"));
    assert_ne!(id, "session_00000000-0000-0000-0000-000000000000");
}
