use std::sync::Arc;

use agent_provider::{
    openai::OpenAiProvider, ModelRequest, ModelTranscriptEntry, PromptSections, ProviderModelInput,
    ProviderToolProfile,
};
use agent_vocab::{ReasoningEffort, TranscriptItem, TurnId, UserMessage};

use super::{
    auth_attempt_requests, ensure_compatible_prepared_request, install_refreshed_provider,
    PreparedModelRequestState,
};
use crate::provider_runtime::provider::ProviderHandle;

#[test]
fn auth_retry_request_clone_reuses_logical_input_allocation() {
    let input = Arc::new(ProviderModelInput::new(
        "test-model",
        PromptSections::stable("stable prompt"),
        vec![ModelTranscriptEntry::from(TranscriptItem::UserMessage(
            UserMessage::text("large transcript"),
        ))],
        ProviderToolProfile::None,
        Vec::new(),
        ReasoningEffort::Medium,
    ));
    let (first_attempt, auth_retry) = auth_attempt_requests(ModelRequest::new(input));

    assert!(std::ptr::eq::<ProviderModelInput>(
        &*first_attempt,
        &*auth_retry
    ));
    assert!(std::ptr::eq(
        first_attempt.transcript().as_ptr(),
        auth_retry.transcript().as_ptr()
    ));
    assert!(std::ptr::eq(
        first_attempt.tools().as_ptr(),
        auth_retry.tools().as_ptr()
    ));
    assert!(std::ptr::eq(first_attempt.prompt(), auth_retry.prompt()));
}

#[tokio::test]
async fn missing_or_changed_account_replacement_reprepares_bytes() {
    for (previous_account, replacement_account) in
        [(None, None), (Some("account-a"), Some("account-b"))]
    {
        let request = ModelRequest::new(Arc::new(
            ProviderModelInput::new(
                "test-model",
                PromptSections::stable("stable prompt"),
                vec![ModelTranscriptEntry::from(TranscriptItem::UserMessage(
                    UserMessage::text("large transcript"),
                ))],
                ProviderToolProfile::None,
                Vec::new(),
                ReasoningEffort::Medium,
            )
            .with_session_id("session-1"),
        ))
        .with_turn_id(TurnId(1));
        let original =
            OpenAiProvider::codex("original-token", previous_account.map(str::to_string), None);
        original.install_model_metadata_for_test("test-model").await;
        let mut provider = ProviderHandle {
            provider: Box::new(original),
            uses_codex_auth: true,
            codex_account_id: previous_account.map(str::to_string),
        };
        let mut prepared = PreparedModelRequestState::default();
        ensure_compatible_prepared_request(&provider, &request, &mut prepared)
            .await
            .expect("original provider prepares");
        let original_prepared = prepared
            .request
            .as_ref()
            .expect("OpenAI prepares bytes")
            .clone();

        let replacement = OpenAiProvider::codex(
            "replacement-token",
            replacement_account.map(str::to_string),
            None,
        );
        replacement
            .install_model_metadata_for_test("test-model")
            .await;
        install_refreshed_provider(
            &mut provider,
            ProviderHandle {
                provider: Box::new(replacement),
                uses_codex_auth: true,
                codex_account_id: replacement_account.map(str::to_string),
            },
            previous_account,
            &request,
            &mut prepared,
        )
        .await
        .expect("replacement provider prepares");

        assert_ne!(
            prepared
                .request
                .as_ref()
                .expect("replacement bytes exist")
                .body_allocation()
                .expect("replacement allocation"),
            original_prepared
                .body_allocation()
                .expect("original allocation")
        );
    }
}
