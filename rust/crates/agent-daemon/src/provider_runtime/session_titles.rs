use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use agent_provider::{ModelRequest, ModelTranscriptEntry, ProviderModelInput};
use agent_store::SessionConfig;
use agent_vocab::{AssistantItem, ReasoningEffort, TranscriptItem, TurnId, UserMessage};
use serde_json::Value;

use crate::runtime::{
    clear_event_buffer_if_idle, publish_events, replace_active_session_config, SessionDriver,
};
use crate::state::AppState;
use crate::types::RuntimeConfig;

use super::{run_model_sidecar, sidecar_session_id, ModelSidecarRequest};

const TITLE_MAX_CHARS: usize = 64;
const TITLE_SIDECAR_TIMEOUT_SECS: u64 = 45;

#[derive(Clone, Default)]
pub(crate) struct SessionTitleScheduler {
    pending: Arc<StdMutex<HashMap<String, Arc<PendingTitleRefresh>>>>,
}

impl SessionTitleScheduler {
    fn schedule(&self, session_id: String, request: PendingTitleRefresh) -> bool {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let generation = pending
            .get(&session_id)
            .map(|request| request.generation.saturating_add(1))
            .unwrap_or(1);
        pending.insert(
            session_id,
            Arc::new(PendingTitleRefresh {
                generation,
                ..request
            }),
        );
        generation == 1
    }
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

#[derive(Debug)]
struct PendingTitleRefresh {
    generation: u64,
    config: RuntimeConfig,
    input: Arc<ProviderModelInput>,
    title_at_submit: Option<String>,
    prompt: &'static str,
}

pub(crate) fn schedule_session_title_refresh_for_model_turn(
    state: &AppState,
    session_id: impl Into<String>,
    config: &RuntimeConfig,
    turn_id: TurnId,
    input: Arc<ProviderModelInput>,
) {
    if session_title_disabled(config) {
        return;
    }
    let Some(prompt) = title_prompt_for_model_input(turn_id, &input) else {
        return;
    };
    let title_at_submit = metadata_title(&config.metadata);

    let state = state.clone();
    let session_id = session_id.into();
    let config = config.clone();
    let should_spawn = state.session_titles.schedule(
        session_id.clone(),
        PendingTitleRefresh {
            generation: 0,
            config,
            input,
            title_at_submit,
            prompt,
        },
    );

    if should_spawn {
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let task_state = state.clone();
        let handle = tokio::spawn(async move {
            if start_rx.await.is_err() {
                return;
            }
            run_title_refresh_worker(state, session_id).await;
        });
        let _ = crate::runtime::register_auxiliary_task(&task_state, handle, start_tx);
    }
}

async fn run_title_refresh_worker(state: AppState, session_id: String) {
    loop {
        let Some(request) = take_next_pending_request(&state, &session_id) else {
            return;
        };
        let generation = request.generation;
        if let Err(error) = refresh_session_title(&state, &session_id, request).await {
            eprintln!("session title refresh failed for {session_id}: {error:#}");
        }
        finish_pending_generation(&state, &session_id, generation);
    }
}

fn take_next_pending_request(
    state: &AppState,
    session_id: &str,
) -> Option<Arc<PendingTitleRefresh>> {
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
    request: Arc<PendingTitleRefresh>,
) -> anyhow::Result<()> {
    let current_config = state.repo.load_session_config(session_id).await?;
    if session_title_disabled(&current_config)
        || metadata_title(&current_config.metadata) != request.title_at_submit
    {
        return Ok(());
    }

    let Some(title) = generate_session_title(state, session_id, &request).await? else {
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
    request: &PendingTitleRefresh,
) -> anyhow::Result<Option<String>> {
    let cache_prefix_len = request.input.transcript().len();
    let input = Arc::new(
        request
            .input
            .as_ref()
            .clone()
            .with_reasoning_effort(ReasoningEffort::Low),
    );
    let mut model_request =
        ModelRequest::new(input).with_transcript_suffix(vec![ModelTranscriptEntry::from(
            TranscriptItem::UserMessage(UserMessage::text(request.prompt)),
        )]);
    model_request.transcript_cache_prefix_len = Some(cache_prefix_len);
    // No max_output_tokens: the OpenAI/Codex `/responses` backend rejects that
    // parameter for some models (e.g. gpt-5.6-sol returns HTTP 400
    // "Unsupported parameter: max_output_tokens"). The short-title prompt plus
    // TITLE_MAX_CHARS truncation already bound the output.
    let sidecar_session_id = title_sidecar_session_id(session_id);
    let response = match tokio::time::timeout(
        Duration::from_secs(TITLE_SIDECAR_TIMEOUT_SECS),
        run_model_sidecar(
            state,
            &request.config,
            ModelSidecarRequest {
                prompt_cache_key: model_request
                    .prompt_cache_key()
                    .map(str::to_string)
                    .unwrap_or_else(|| session_id.to_string()),
                sidecar_session_id: sidecar_session_id.clone(),
                request: model_request,
            },
        ),
    )
    .await
    {
        Ok(response) => response?,
        Err(_) => {
            state
                .provider_connections
                .remove_session(&sidecar_session_id)
                .await;
            anyhow::bail!("title sidecar timed out after {TITLE_SIDECAR_TIMEOUT_SECS} seconds");
        }
    };

    Ok(title_from_response(&response.assistant.items))
}

const TITLE_INITIAL_PROMPT: &str = r#"Above is the conversation prefix for the normal model request for this turn. For this sidecar request only, ignore any instruction to solve the user's coding task.

Generate a short UI title that describes the overall chat session so far.

Rules:
- Do not call any tools.
- Return exactly one JSON object and no other text.
- Use {"title":"..."} with a concise semantic title.
- Use {"title":null} only if no safe title is warranted.
- Base the title on the conversation's central goal, accumulated decisions, and durable subject matter across all turns, not just the most recent user message.
- If the latest user message is a follow-up, correction, status check, interruption, or implementation detail, treat it as context for the broader session rather than the title's topic.
- Prefer 3-8 words and at most 64 characters.
- Use the user's language when practical.
- Do not include quotation marks, trailing punctuation, or generic prefixes such as "Chat about".
- The title must not contain secrets, access tokens, API keys, or credentials.
- If the message is mostly a secret/credential, an empty/unclear fragment, or an interruption/control request, use {"title":null}."#;

const TITLE_REFRESH_PROMPT: &str = r#"Above is the conversation prefix for the normal model request for this turn. For this sidecar request only, ignore any instruction to solve the user's coding task.

This chat session already has a semantic title. Decide whether a rename is warranted. If you rename it, generate a short UI title that describes the overall chat session so far.

Rules:
- Do not call any tools.
- Return exactly one JSON object and no other text.
- Default to {"title":null}.
- Use {"title":"..."} only if the conversation has developed a notable, durable shift or expansion in the overall session topic that encompasses more than the original scope.
- Use {"title":null} if the current session name should not change or no safe title is warranted.
- Base any new title on the conversation's central goal, accumulated decisions, and durable subject matter across all turns, not just the most recent user message.
- Do not rename merely because the latest message mentions a new detail, task step, bug, PR number, implementation tactic, or status check within the same overall session.
- Do not rename for routine follow-ups, corrections, status checks, interruptions, implementation details, or short clarifications.
- Prefer 3-8 words and at most 64 characters.
- Use the user's language when practical.
- Do not include quotation marks, trailing punctuation, or generic prefixes such as "Chat about".
- The title must not contain secrets, access tokens, API keys, or credentials.
- If the message is mostly a secret/credential, an empty/unclear fragment, or an interruption/control request, use {"title":null}."#;

fn title_from_response(items: &[AssistantItem]) -> Option<String> {
    items.iter().find_map(|item| {
        let AssistantItem::Text(text) = item else {
            return None;
        };
        let args = title_json_from_text(text)?;
        let raw_title = args.get("title").and_then(Value::as_str)?;
        sanitize_title(raw_title)
    })
}

fn title_json_from_text(text: &str) -> Option<Value> {
    let text = text.trim();
    serde_json::from_str(text)
        .ok()
        .or_else(|| serde_json::from_str(strip_json_code_fence(text)?).ok())
        .or_else(|| parse_first_json_object(text))
}

fn strip_json_code_fence(text: &str) -> Option<&str> {
    let text = text.strip_prefix("```")?;
    let text = text
        .strip_prefix("json")
        .or_else(|| text.strip_prefix("JSON"))
        .unwrap_or(text);
    let text = text.strip_prefix('\n').unwrap_or(text);
    text.strip_suffix("```").map(str::trim)
}

fn parse_first_json_object(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth = depth.saturating_add(1),
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return serde_json::from_str(&text[start..=start + offset]).ok();
                }
            }
            _ => {}
        }
    }
    None
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

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
fn title_prompt_for_model_turn(
    turn_id: TurnId,
    model_context: &agent_session::ModelContext,
) -> Option<&'static str> {
    let user_message_count = model_context
        .transcript_items()
        .iter()
        .filter(|item| matches!(item, TranscriptItem::UserMessage(_)))
        .take(2)
        .count();
    title_prompt_for_items(
        turn_id,
        model_context.transcript_items().last(),
        user_message_count,
    )
}

fn title_prompt_for_model_input(
    turn_id: TurnId,
    input: &ProviderModelInput,
) -> Option<&'static str> {
    let user_message_count = input
        .transcript()
        .iter()
        .filter(|entry| matches!(entry.item, TranscriptItem::UserMessage(_)))
        .take(2)
        .count();
    title_prompt_for_items(
        turn_id,
        input.transcript().last().map(|entry| &entry.item),
        user_message_count,
    )
}

fn title_prompt_for_items(
    turn_id: TurnId,
    last_item: Option<&TranscriptItem>,
    user_message_count: usize,
) -> Option<&'static str> {
    let Some(TranscriptItem::UserMessage(_)) = last_item else {
        return None;
    };
    Some(if turn_id == TurnId::first() && user_message_count == 1 {
        TITLE_INITIAL_PROMPT
    } else {
        TITLE_REFRESH_PROMPT
    })
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
