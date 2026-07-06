use super::*;
use agent_provider::ProviderToolProfile;
use agent_session::ModelContext;
use agent_store::SessionConfig;
use agent_vocab::{
    AssistantItem, ProviderConfig, ProviderKind, ReasoningEffort, TranscriptItem, TurnId,
    UserMessage,
};
use serde_json::json;

fn runtime_config(system_prompt: &str) -> RuntimeConfig {
    SessionConfig {
        project_id: None,
        outer_cwd: "/tmp".to_string(),
        workspaces: Vec::new(),
        system_prompt: system_prompt.to_string(),
        provider: ProviderConfig {
            kind: ProviderKind::Claude,
            model: "claude-opus-4-8".to_string(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::High,
            prompt_cache: None,
        },
        metadata: json!({}),
    }
    .into()
}

#[test]
fn pending_title_schedule_retains_runtime_config_and_prompt_allocations() {
    let scheduler = SessionTitleScheduler::default();
    let config = runtime_config(&"large stable prompt".repeat(1_000));
    let input = Arc::new(ProviderModelInput::from_shared(
        "claude-opus-4-8",
        Arc::clone(config.prompt()),
        vec![TranscriptItem::UserMessage(UserMessage::text("title this")).into()],
        ProviderToolProfile::AnthropicCoding,
        Arc::from([]),
        ReasoningEffort::High,
    ));
    let config_allocation = config.config_allocation();
    let prompt_allocation = Arc::as_ptr(config.prompt());

    assert!(scheduler.schedule(
        "session-1".to_string(),
        PendingTitleRefresh {
            generation: 0,
            config: config.clone(),
            input,
            title_at_submit: None,
            prompt: TITLE_INITIAL_PROMPT,
        },
    ));

    let pending = scheduler
        .pending
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let request = pending.get("session-1").expect("scheduled title refresh");
    assert_eq!(request.config.config_allocation(), config_allocation);
    assert_eq!(Arc::as_ptr(request.config.prompt()), prompt_allocation);
    assert_eq!(request.input.prompt_allocation(), prompt_allocation);
}

#[test]
fn sanitize_title_trims_quotes_and_punctuation() {
    assert_eq!(
        sanitize_title("  \"Production deploy notes.\"  "),
        Some("Production deploy notes".to_string())
    );
}

#[test]
fn title_prompt_requires_context_ending_in_user_message() {
    let context = ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted {
            turn_id: TurnId::first(),
        },
        TranscriptItem::UserMessage(UserMessage::text("debug flaky tests")),
    ]);

    assert_eq!(
        title_prompt_for_model_turn(TurnId::first(), &context),
        Some(TITLE_INITIAL_PROMPT)
    );

    let context = ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted {
            turn_id: TurnId::first(),
        },
        TranscriptItem::UserMessage(UserMessage::text("debug flaky tests")),
        TranscriptItem::AssistantMessage(agent_vocab::AssistantMessage {
            items: vec![AssistantItem::Text("done".to_string())],
        }),
    ]);

    assert_eq!(title_prompt_for_model_turn(TurnId::first(), &context), None);
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
fn first_user_message_turn_uses_replacement_prompt() {
    let context = ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted {
            turn_id: TurnId::first(),
        },
        TranscriptItem::UserMessage(UserMessage::text(
            "https://github.com/deepseek-ai/DeepEP\n\nDeepEPV2 uses NCCL GIN.",
        )),
    ]);

    assert_eq!(
        title_prompt_for_model_turn(TurnId::first(), &context),
        Some(TITLE_INITIAL_PROMPT)
    );
}

#[test]
fn later_user_message_turn_uses_refresh_prompt() {
    let context = ModelContext::from_transcript_items(vec![
        TranscriptItem::TurnStarted {
            turn_id: TurnId::first(),
        },
        TranscriptItem::UserMessage(UserMessage::text("https://github.com/deepseek-ai/DeepEP")),
        TranscriptItem::AssistantMessage(agent_vocab::AssistantMessage {
            items: vec![AssistantItem::Text("done".to_string())],
        }),
        TranscriptItem::TurnFinished {
            turn_id: TurnId::first(),
            outcome: agent_vocab::TurnOutcome::Graceful,
        },
        TranscriptItem::TurnStarted { turn_id: TurnId(2) },
        TranscriptItem::UserMessage(UserMessage::text("follow-up")),
    ]);

    assert_eq!(
        title_prompt_for_model_turn(TurnId(2), &context),
        Some(TITLE_REFRESH_PROMPT)
    );
}

#[test]
fn compacted_later_user_message_turn_uses_refresh_prompt() {
    let context = ModelContext::from_transcript_items(vec![
        TranscriptItem::CompactionSummary(agent_vocab::CompactionSummary::new(
            "session",
            "entry",
            "Earlier session summary",
            None,
            TurnId(8),
        )),
        TranscriptItem::TurnStarted { turn_id: TurnId(9) },
        TranscriptItem::UserMessage(UserMessage::text("follow-up after compaction")),
    ]);

    assert_eq!(
        title_prompt_for_model_turn(TurnId(9), &context),
        Some(TITLE_REFRESH_PROMPT)
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
