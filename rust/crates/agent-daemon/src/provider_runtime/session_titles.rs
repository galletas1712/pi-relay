use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use agent_provider::{ModelTranscriptEntry, PromptSections, ProviderToolProfile};
use agent_store::SessionConfig;
use agent_vocab::{
    AssistantItem, ContentBlock, ProviderKind, ReasoningEffort, TranscriptItem, UserMessage,
};
use serde::Serialize;
use serde_json::Value;

use crate::runtime::{
    clear_event_buffer_if_idle, publish_events, replace_active_session_config, SessionDriver,
};
use crate::state::AppState;

use super::{model_prompt_cache_key, run_model_sidecar, sidecar_session_id, ModelSidecarRequest};

const TITLE_GENERATION_MAX_OUTPUT_TOKENS: u32 = 160;
const TITLE_INPUT_CHAR_LIMIT: usize = 8_000;
const TITLE_MAX_CHARS: usize = 64;

#[derive(Clone, Default)]
pub(crate) struct SessionTitleScheduler {
    pending: Arc<StdMutex<HashMap<String, PendingTitleRefresh>>>,
}

fn pending_generation_matches(state: &AppState, session_id: &str, generation: u64) -> bool {
    state
        .session_titles
        .pending
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(session_id)
        .is_some_and(|request| request.generation == generation)
}

#[derive(Debug, Clone)]
struct PendingTitleRefresh {
    generation: u64,
    message: UserMessage,
    title_at_submit: Option<String>,
}

pub(crate) fn schedule_session_title_refresh(
    state: &AppState,
    session_id: impl Into<String>,
    config: &SessionConfig,
    message: &UserMessage,
) {
    if session_title_disabled(config) {
        return;
    }
    let title_at_submit = metadata_title(&config.metadata);

    let state = state.clone();
    let session_id = session_id.into();
    let message = message.clone();
    let should_spawn = {
        let mut pending = state
            .session_titles
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let generation = pending
            .get(&session_id)
            .map(|request| request.generation.saturating_add(1))
            .unwrap_or(1);
        pending.insert(
            session_id.clone(),
            PendingTitleRefresh {
                generation,
                message,
                title_at_submit,
            },
        );
        generation == 1
    };

    if should_spawn {
        tokio::spawn(async move {
            run_title_refresh_worker(state, session_id).await;
        });
    }
}

async fn run_title_refresh_worker(state: AppState, session_id: String) {
    loop {
        let Some(request) = take_next_pending_request(&state, &session_id) else {
            return;
        };
        if let Err(error) = refresh_session_title(&state, &session_id, request.clone()).await {
            eprintln!("session title refresh failed for {session_id}: {error:#}");
        }
        finish_pending_generation(&state, &session_id, request.generation);
    }
}

fn take_next_pending_request(state: &AppState, session_id: &str) -> Option<PendingTitleRefresh> {
    state
        .session_titles
        .pending
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(session_id)
        .cloned()
}

fn finish_pending_generation(state: &AppState, session_id: &str, generation: u64) {
    let mut pending = state
        .session_titles
        .pending
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if pending
        .get(session_id)
        .is_some_and(|request| request.generation == generation)
    {
        pending.remove(session_id);
    }
}

async fn refresh_session_title(
    state: &AppState,
    session_id: &str,
    request: PendingTitleRefresh,
) -> anyhow::Result<()> {
    let current_config = state.repo.load_session_config(session_id).await?;
    if session_title_disabled(&current_config)
        || metadata_title(&current_config.metadata) != request.title_at_submit
    {
        return Ok(());
    }

    let Some(title) = generate_session_title(state, session_id, &current_config, &request).await?
    else {
        return Ok(());
    };
    if Some(title.as_str()) == request.title_at_submit.as_deref() {
        return Ok(());
    }
    if !pending_generation_matches(state, session_id, request.generation) {
        return Ok(());
    }

    let _driver = SessionDriver::acquire(state, session_id).await;
    let latest_config = state.repo.load_session_config(session_id).await?;
    if session_title_disabled(&latest_config)
        || metadata_title(&latest_config.metadata) != request.title_at_submit
        || !pending_generation_matches(state, session_id, request.generation)
    {
        return Ok(());
    }

    let events = state.repo.rename_session(session_id, &title).await?;
    let config = state.repo.load_session_config(session_id).await?;
    replace_active_session_config(state, session_id, config).await;
    publish_events(state, events);
    clear_event_buffer_if_idle(state, session_id)
        .await
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
    Ok(())
}

async fn generate_session_title(
    state: &AppState,
    session_id: &str,
    config: &SessionConfig,
    request: &PendingTitleRefresh,
) -> anyhow::Result<Option<String>> {
    let title_context = SessionTitlePromptContext {
        current_title: request.title_at_submit.as_deref().unwrap_or_default(),
        user_message: render_user_message_for_title(&request.message),
    };
    let response = run_model_sidecar(
        state,
        config,
        ModelSidecarRequest {
            prompt_cache_key: model_prompt_cache_key(config, session_id),
            sidecar_session_id: title_sidecar_session_id(session_id),
            prompt: PromptSections::new(
                Some(config.system_prompt.clone()),
                Some(TITLE_GENERATION_SYSTEM_PROMPT.to_string()),
            ),
            transcript: vec![ModelTranscriptEntry::from(TranscriptItem::UserMessage(
                UserMessage::text(serde_json::to_string(&title_context)?),
            ))],
            tool_profile: ProviderToolProfile::for_provider(config.provider.kind),
            tools: state
                .tools
                .provider_tools_for_provider(config.provider.kind),
            max_tokens: Some(TITLE_GENERATION_MAX_OUTPUT_TOKENS),
            reasoning_effort: title_reasoning_effort(config.provider.kind),
        },
    )
    .await?;

    Ok(title_from_response(&response.assistant.items))
}

#[derive(Serialize)]
struct SessionTitlePromptContext<'a> {
    current_title: &'a str,
    user_message: String,
}

const TITLE_GENERATION_SYSTEM_PROMPT: &str = r#"Above is the normal pi-relay system prompt for this session. For this sidecar request only, ignore any instruction to solve the user's coding task.

You generate short UI titles for pi-relay chat sessions. You are given JSON containing the current session title and the user message for this turn. Decide whether to rename the session that encapsulates the conversation so far.

Rules:
- Do not call any tools.
- Return exactly one JSON object and no other text.
- Use {"title":"..."} only when the new title is clearly better than the current title.
- Use {"title":null} if the session name is already appropriate or no safe title is warranted.
- Prefer 3-8 words and at most 64 characters.
- Use the user's language when practical.
- Do not include quotation marks, trailing punctuation, or generic prefixes such as "Chat about".
- The title must not contain secrets, access tokens, API keys, or credentials.
- If the message is mostly a secret/credential, an empty/unclear fragment, or an interruption/control request, use {"title":null}."#;

fn title_reasoning_effort(provider: ProviderKind) -> ReasoningEffort {
    match provider {
        ProviderKind::OpenAi => ReasoningEffort::Minimal,
        ProviderKind::Claude => ReasoningEffort::Low,
    }
}

fn title_from_response(items: &[AssistantItem]) -> Option<String> {
    items.iter().find_map(|item| {
        let AssistantItem::Text(text) = item else {
            return None;
        };
        let args: Value = serde_json::from_str(text.trim()).ok()?;
        let raw_title = args.get("title").and_then(Value::as_str)?;
        sanitize_title(raw_title)
    })
}

fn sanitize_title(title: &str) -> Option<String> {
    let title = title
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim_end_matches(['.', '!', '?', ':', ';'])
        .trim();
    if title.is_empty() {
        return None;
    }
    let mut title = truncate_chars(title, TITLE_MAX_CHARS).trim().to_string();
    if title.is_empty() || looks_like_secret(&title) {
        return None;
    }
    while title.ends_with(['.', '!', '?', ':', ';']) {
        title.pop();
        title = title.trim_end().to_string();
    }
    (!title.is_empty()).then_some(title)
}

fn render_user_message_for_title(message: &UserMessage) -> String {
    let mut rendered = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => rendered.push(text.clone()),
            ContentBlock::Image { .. } => rendered.push("[image]".to_string()),
        }
    }
    truncate_chars(&rendered.join("\n"), TITLE_INPUT_CHAR_LIMIT)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn metadata_title(metadata: &Value) -> Option<String> {
    metadata
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn session_title_disabled(config: &SessionConfig) -> bool {
    config
        .metadata
        .get("harness")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || config
            .metadata
            .get("auto_title_disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn title_sidecar_session_id(session_id: &str) -> String {
    sidecar_session_id("title", session_id, &[])
}

fn looks_like_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("sk-") || lower.contains("bearer ") {
        return true;
    }
    let credential_words = [
        "api_key",
        "apikey",
        "access_token",
        "secret",
        "token",
        "password",
        "passwd",
    ];
    credential_words
        .iter()
        .any(|word| lower.contains(word) && lower.contains(['=', ':']))
}

#[cfg(test)]
#[path = "session_titles_tests.rs"]
mod tests;
