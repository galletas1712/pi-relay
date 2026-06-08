use super::*;
use agent_session::ModelContext;
use agent_vocab::{AssistantItem, TranscriptItem, UserMessage};
use serde_json::json;

#[test]
fn sanitize_title_trims_quotes_and_punctuation() {
    assert_eq!(
        sanitize_title("  \"Production deploy notes.\"  "),
        Some("Production deploy notes".to_string())
    );
}

#[test]
fn title_trigger_message_uses_only_model_context_ending_in_user_message() {
    let message = UserMessage::text("debug flaky tests");
    let context = ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted {
            turn_id: agent_vocab::TurnId(1),
        },
        TranscriptItem::UserMessage(message.clone()),
    ]);

    assert_eq!(title_trigger_message(&context), Some(&message));

    let context = ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted {
            turn_id: agent_vocab::TurnId(1),
        },
        TranscriptItem::UserMessage(message),
        TranscriptItem::AssistantMessage(agent_vocab::AssistantMessage {
            items: vec![AssistantItem::Text("done".to_string())],
        }),
    ]);

    assert_eq!(title_trigger_message(&context), None);
}

#[test]
fn title_from_response_ignores_null_title() {
    let items = vec![AssistantItem::Text("{\"title\":null}".to_string())];

    assert_eq!(title_from_response(&items), None);
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
fn truncated_web_titles_are_treated_as_placeholders() {
    assert!(current_title_is_truncated_placeholder(
        Some("right now session names in pi-relay are just a prefix of the ..."),
        &json!({ "created_by": "web" }),
    ));
}

#[test]
fn explicit_or_finished_titles_are_not_truncated_placeholders() {
    assert!(!current_title_is_truncated_placeholder(
        Some("Auto Session Titles"),
        &json!({ "created_by": "web" }),
    ));
    assert!(!current_title_is_truncated_placeholder(
        Some("Long title from another client ..."),
        &json!({ "created_by": "api" }),
    ));
    assert!(!current_title_is_truncated_placeholder(
        Some("Unknown client title ..."),
        &json!({}),
    ));
    assert!(!current_title_is_truncated_placeholder(
        Some("Manual looking title ..."),
        &json!({ "created_by": "web", "auto_title_disabled": true }),
    ));
}

#[test]
fn title_reasoning_effort_uses_provider_supported_low_effort() {
    assert_eq!(
        title_reasoning_effort(ProviderKind::OpenAi),
        ReasoningEffort::Low
    );
    assert_eq!(
        title_reasoning_effort(ProviderKind::Claude),
        ReasoningEffort::Low
    );
}

#[test]
fn title_from_response_uses_json_text() {
    let items = vec![AssistantItem::Text(
        "{\"title\":\"Debug flaky tests\"}".to_string(),
    )];

    assert_eq!(
        title_from_response(&items),
        Some("Debug flaky tests".to_string())
    );
}

#[test]
fn title_from_response_uses_fenced_json_text() {
    let items = vec![AssistantItem::Text(
        "```json\n{\"title\":\"Debug flaky tests\"}\n```".to_string(),
    )];

    assert_eq!(
        title_from_response(&items),
        Some("Debug flaky tests".to_string())
    );
}

#[test]
fn title_from_response_uses_json_object_embedded_in_text() {
    let items = vec![AssistantItem::Text(
        "Here is the title:\n{\"title\":\"Debug flaky tests\"}\nThanks".to_string(),
    )];

    assert_eq!(
        title_from_response(&items),
        Some("Debug flaky tests".to_string())
    );
}

#[test]
fn title_from_response_ignores_embedded_object_without_title() {
    let items = vec![AssistantItem::Text(
        "Here is the title:\n{\"other\":\"Debug flaky tests\"}\n{\"title\":\"Ignored\"}"
            .to_string(),
    )];

    assert_eq!(title_from_response(&items), None);
}

#[test]
fn title_sidecar_session_id_is_short_and_distinct_from_main_session() {
    let id = title_sidecar_session_id("session_00000000-0000-0000-0000-000000000000");

    assert!(id.len() <= 64);
    assert!(id.starts_with("title-session_"));
    assert_ne!(id, "session_00000000-0000-0000-0000-000000000000");
}
