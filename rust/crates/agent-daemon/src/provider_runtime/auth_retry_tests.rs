use std::sync::Arc;

use agent_provider::{
    ModelRequest, ModelTranscriptEntry, PromptSections, ProviderModelInput, ProviderToolProfile,
};
use agent_vocab::{ReasoningEffort, TranscriptItem, UserMessage};

use super::auth_attempt_requests;

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
