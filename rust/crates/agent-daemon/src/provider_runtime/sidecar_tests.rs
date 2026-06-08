use super::*;
use agent_provider::{PromptSections, ProviderToolProfile};
use agent_vocab::ReasoningEffort;

#[test]
fn sidecar_session_id_is_stable_short_and_descriptive() {
    let id = sidecar_session_id(
        "title",
        "session_00000000-0000-0000-0000-000000000000",
        &["turn_1"],
    );

    assert_eq!(
        id,
        sidecar_session_id(
            "title",
            "session_00000000-0000-0000-0000-000000000000",
            &["turn_1"],
        )
    );
    assert!(id.len() <= 64);
    assert!(id.starts_with("title-session_"));
}

#[test]
fn sidecar_session_id_varies_by_part() {
    let first = sidecar_session_id("web", "session", &["call_a"]);
    let second = sidecar_session_id("web", "session", &["call_b"]);

    assert_ne!(first, second);
}

#[test]
fn sidecar_session_id_falls_back_to_generic_prefix() {
    let id = sidecar_session_id("!!!", "###", &["part"]);

    assert!(id.starts_with("sidecar-"));
    assert!(id.len() <= 64);
}

#[test]
fn sidecar_request_preserves_owner_session_id_for_cache_cohort() {
    let request = test_model_request(Some("session-owner"));

    let prepared = prepare_sidecar_model_request(
        request,
        "title-session-owner-sidecar",
        "session-owner".to_string(),
    );

    assert_eq!(prepared.session_id.as_deref(), Some("session-owner"));
    assert_eq!(prepared.prompt_cache_key.as_deref(), Some("session-owner"));
    assert_eq!(prepared.turn_id, None);
}

#[test]
fn sidecar_request_uses_sidecar_session_id_when_no_owner_is_present() {
    let request = test_model_request(None);

    let prepared = prepare_sidecar_model_request(
        request,
        "web-session-call-sidecar",
        "web-cache-key".to_string(),
    );

    assert_eq!(
        prepared.session_id.as_deref(),
        Some("web-session-call-sidecar")
    );
    assert_eq!(prepared.prompt_cache_key.as_deref(), Some("web-cache-key"));
}

fn test_model_request(session_id: Option<&str>) -> ModelRequest {
    ModelRequest {
        model: "gpt-5.5".to_string(),
        prompt: PromptSections::stable("stable"),
        transcript_cache_prefix_len: None,
        transcript: Vec::new(),
        tool_profile: ProviderToolProfile::None,
        tools: Vec::new(),
        max_tokens: None,
        reasoning_effort: ReasoningEffort::Low,
        prompt_cache_key: None,
        session_id: session_id.map(str::to_string),
        turn_id: Some(agent_vocab::TurnId(1)),
    }
}
