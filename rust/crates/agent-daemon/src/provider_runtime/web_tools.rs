use agent_provider::{
    ModelRequest, ModelResponse, ModelTranscriptEntry, PromptSections, ProviderToolProfile,
};
use agent_store::SessionConfig;
use agent_tools::{
    limit_tool_output_with_max_tokens, nonempty_domains, ProviderTool, ToolContext, ToolExecution,
    WebFetchArgs, WebSearchArgs,
};
use agent_vocab::{
    AssistantItem, ProviderKind, ProviderReplayItem, ToolCall, ToolResultMessage, TranscriptItem,
    UserMessage,
};
use serde_json::{json, Value};

use crate::state::AppState;

use super::{run_model_sidecar, sidecar_session_id, ModelSidecarRequest};

fn web_sidecar_session_id(session_id: &str, call_id: &str) -> String {
    sidecar_session_id("web", session_id, &[call_id])
}

pub(crate) fn is_web_tool_name(name: &str) -> bool {
    canonical_web_tool_name(name).is_some()
}

pub(crate) async fn run_web_tool(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    call: &ToolCall,
    ctx: &ToolContext,
) -> ToolResultMessage {
    match canonical_web_tool_name(&call.tool_name) {
        Some("WebSearch") => run_web_search(state, config, session_id, call).await,
        Some("WebFetch") => run_web_fetch(state, config, session_id, call, ctx).await,
        _ => ToolResultMessage::error(
            call.id.clone(),
            &call.tool_name,
            "unknown web tool".to_string(),
        ),
    }
}

fn canonical_web_tool_name(name: &str) -> Option<&'static str> {
    match name {
        "WebSearch" | "web_search" => Some("WebSearch"),
        "WebFetch" | "web_fetch" => Some("WebFetch"),
        _ => None,
    }
}

async fn run_web_search(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    call: &ToolCall,
) -> ToolResultMessage {
    let args: WebSearchArgs = match serde_json::from_str(&call.args_json) {
        Ok(args) => args,
        Err(error) => {
            return ToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                format!("web_search arguments were invalid JSON: {error}"),
            )
        }
    };
    if args.query.trim().is_empty() {
        return ToolResultMessage::error(
            call.id.clone(),
            &call.tool_name,
            "web_search query cannot be empty".to_string(),
        );
    }

    let tool = match config.provider.kind {
        ProviderKind::Claude => anthropic_web_search_tool(&args),
        ProviderKind::OpenAi => openai_web_search_tool(),
    };
    let prompt = web_search_sidecar_prompt(&args);
    run_provider_web_sidecar(
        state,
        config,
        session_id,
        call,
        tool,
        prompt,
        args.max_output_tokens,
    )
    .await
}

async fn run_web_fetch(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    call: &ToolCall,
    ctx: &ToolContext,
) -> ToolResultMessage {
    let args: WebFetchArgs = match serde_json::from_str(&call.args_json) {
        Ok(args) => args,
        Err(error) => {
            return ToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                format!("web_fetch arguments were invalid JSON: {error}"),
            )
        }
    };
    if args.url.trim().is_empty() {
        return ToolResultMessage::error(
            call.id.clone(),
            &call.tool_name,
            "web_fetch url cannot be empty".to_string(),
        );
    }

    match config.provider.kind {
        ProviderKind::Claude => {
            let tool = anthropic_web_fetch_tool();
            let prompt = web_fetch_sidecar_prompt(&args);
            match run_provider_web_sidecar(
                state,
                config,
                session_id,
                call,
                tool,
                prompt,
                args.max_output_tokens,
            )
            .await
            {
                result if matches!(result.status, agent_vocab::ToolResultStatus::Success) => result,
                provider_error => {
                    // If Claude's server-side fetch cannot run, fall back to
                    // the provider-neutral HTTP fetch implementation so the
                    // local web tool still has a best-effort execution path.
                    let fallback = state
                        .tools
                        .execute(config.provider.kind, call, ctx)
                        .await
                        .unwrap_or_else(|_| {
                            ToolResultMessage::crashed(call.id.clone(), call.tool_name.clone())
                        });
                    if matches!(fallback.status, agent_vocab::ToolResultStatus::Success) {
                        fallback
                    } else {
                        provider_error
                    }
                }
            }
        }
        ProviderKind::OpenAi => state
            .tools
            .execute(config.provider.kind, call, ctx)
            .await
            .unwrap_or_else(|_| {
                ToolResultMessage::crashed(call.id.clone(), call.tool_name.clone())
            }),
    }
}

async fn run_provider_web_sidecar(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    call: &ToolCall,
    tool: ProviderTool,
    user_prompt: String,
    max_output_tokens: Option<usize>,
) -> ToolResultMessage {
    let sidecar_session_id = web_sidecar_session_id(session_id, call.id.as_str());
    let request = ModelSidecarRequest {
        prompt_cache_key: sidecar_session_id.clone(),
        sidecar_session_id,
        request: ModelRequest {
            model: config.provider.model.clone(),
            transcript_cache_prefix_len: None,
            prompt: PromptSections::stable(
                "You are a web-tool executor for pi-relay. Use the provided web tool to satisfy the requested tool call. Return a concise tool result for the caller and include source URLs whenever available. Do not ask follow-up questions.",
            ),
            transcript: vec![ModelTranscriptEntry::from(TranscriptItem::UserMessage(
                UserMessage::text(user_prompt),
            ))],
            tool_profile: ProviderToolProfile::CustomDefinitions,
            tools: vec![tool],
            max_tokens: Some(config.provider.max_tokens.unwrap_or(8_192).min(8_192)),
            reasoning_effort: crate::model_metadata::normalize_reasoning_effort(
                config.provider.kind,
                &config.provider.model,
                config.provider.reasoning_effort,
            ),
            prompt_cache_key: None,
            session_id: None,
            turn_id: None,
        },
    };

    match run_model_sidecar(state, config, request).await {
        Ok(response) => sidecar_response_to_tool_result(call, response, max_output_tokens),
        Err(error) => ToolResultMessage::error(
            call.id.clone(),
            &call.tool_name,
            format!("web tool provider backend failed: {error}"),
        ),
    }
}

fn sidecar_response_to_tool_result(
    call: &ToolCall,
    response: ModelResponse,
    max_output_tokens: Option<usize>,
) -> ToolResultMessage {
    let mut output = response.assistant.text().trim().to_string();
    if response.assistant.tool_calls().next().is_some() {
        output = run_embedded_web_tool_calls(&response, &output);
    }
    if output.is_empty() {
        output = summarize_web_replay(&response.provider_replay);
    }
    if output.is_empty() {
        output = "web tool backend returned no text output".to_string();
    }
    ToolResultMessage::success(
        call.id.clone(),
        &call.tool_name,
        limit_tool_output_with_max_tokens(output, max_output_tokens),
    )
}

fn run_embedded_web_tool_calls(response: &ModelResponse, initial_text: &str) -> String {
    let mut transcript = Vec::new();
    if !initial_text.is_empty() {
        transcript.push(initial_text.to_string());
    }
    for item in &response.assistant.items {
        match item {
            AssistantItem::Text(_) => {}
            AssistantItem::ToolCall(tool_call) => {
                if let Some(line) = embedded_tool_call_to_summary(tool_call) {
                    transcript.push(line);
                }
            }
        }
    }
    let replay = summarize_web_replay(&response.provider_replay);
    if !replay.is_empty() {
        transcript.push(replay);
    }
    transcript.join("\n")
}

fn embedded_tool_call_to_summary(call: &ToolCall) -> Option<String> {
    let input = call.args_value().unwrap_or_else(|_| json!({}));
    match call.tool_name.as_str() {
        "WebSearch" | "web_search" => {
            let query = input.get("query").and_then(Value::as_str)?;
            Some(format!("Search query: {query}"))
        }
        "WebFetch" | "web_fetch" => {
            let url = input.get("url").and_then(Value::as_str)?;
            Some(format!("Fetch URL: {url}"))
        }
        _ => None,
    }
}

fn anthropic_web_search_tool(args: &WebSearchArgs) -> ProviderTool {
    let mut declaration = json!({
        "type": "web_search_20250305",
        "name": "web_search",
        "max_uses": 8,
    });
    if let Some(domains) = nonempty_domains(args.allowed_domains.as_deref()) {
        declaration["allowed_domains"] = json!(domains);
    }
    if let Some(domains) = nonempty_domains(args.blocked_domains.as_deref()) {
        declaration["blocked_domains"] = json!(domains);
    }
    ProviderTool::new(
        "web_search",
        "Anthropic web search sidecar tool.",
        json!({ "type": "object" }),
        declaration,
        // This sidecar declaration is provider-native, but it is not part of
        // the extension API's execution model. The main-loop web_search tool
        // remains a local JSON wrapper that pi-relay executes.
        ToolExecution::LocalJson,
    )
}

fn anthropic_web_fetch_tool() -> ProviderTool {
    ProviderTool::new(
        "web_fetch",
        "Anthropic web fetch sidecar tool.",
        json!({ "type": "object" }),
        json!({
            "type": "web_fetch_20250910",
            "name": "web_fetch",
            "citations": { "enabled": true },
            "max_content_tokens": 20_000,
        }),
        // Sidecar-only provider declaration; main-loop execution is still the
        // local JSON wrapper.
        ToolExecution::LocalJson,
    )
}

fn openai_web_search_tool() -> ProviderTool {
    ProviderTool::new(
        "web_search",
        "OpenAI web search sidecar tool.",
        json!({ "type": "object" }),
        json!({
            "type": "web_search",
            "search_context_size": "high",
        }),
        // Sidecar-only provider declaration; main-loop execution is still the
        // local JSON wrapper.
        ToolExecution::LocalJson,
    )
}

fn web_search_sidecar_prompt(args: &WebSearchArgs) -> String {
    let mut prompt = format!(
        "Perform a web search for this query and return concise, cited results:\n\n{}",
        args.query.trim()
    );
    if let Some(recency) = args
        .recency
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        prompt.push_str(&format!("\n\nRecency preference: {recency}"));
    }
    if let Some(domains) = nonempty_domains(args.allowed_domains.as_deref()) {
        prompt.push_str(&format!(
            "\n\nOnly include these domains: {}",
            domains.join(", ")
        ));
    }
    if let Some(domains) = nonempty_domains(args.blocked_domains.as_deref()) {
        prompt.push_str(&format!(
            "\n\nExclude these domains: {}",
            domains.join(", ")
        ));
    }
    prompt
}

fn web_fetch_sidecar_prompt(args: &WebFetchArgs) -> String {
    let mut prompt = format!(
        "Fetch this URL and return the requested information with source URL included:\n\n{}",
        args.url.trim()
    );
    if let Some(instruction) = args
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        prompt.push_str(&format!("\n\nInstruction: {instruction}"));
    }
    prompt
}

fn summarize_web_replay(replay: &[ProviderReplayItem]) -> String {
    let mut lines = Vec::new();
    for item in replay {
        let Ok(raw) = item.raw_value() else {
            continue;
        };
        match raw.get("type").and_then(Value::as_str) {
            Some("web_search_tool_result") => summarize_anthropic_search_result(&raw, &mut lines),
            Some("web_fetch_tool_result") => summarize_json_block("web_fetch", &raw, &mut lines),
            Some("web_search_call") => summarize_openai_search_call(&raw, &mut lines),
            _ => {}
        }
    }
    lines.join("\n")
}

fn summarize_anthropic_search_result(raw: &Value, lines: &mut Vec<String>) {
    let Some(content) = raw.get("content") else {
        return;
    };
    if let Some(results) = content.as_array() {
        for result in results {
            let title = result
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("result");
            let url = result.get("url").and_then(Value::as_str).unwrap_or("");
            if url.is_empty() {
                lines.push(format!("- {title}"));
            } else {
                lines.push(format!("- {title}: {url}"));
            }
        }
    } else {
        summarize_json_block("web_search", raw, lines);
    }
}

fn summarize_openai_search_call(raw: &Value, lines: &mut Vec<String>) {
    let Some(action) = raw.get("action") else {
        summarize_json_block("web_search", raw, lines);
        return;
    };
    match action.get("type").and_then(Value::as_str) {
        Some("search") => {
            if let Some(query) = action.get("query").and_then(Value::as_str) {
                lines.push(format!("Search query: {query}"));
            }
        }
        Some("open_page") => {
            if let Some(url) = action.get("url").and_then(Value::as_str) {
                lines.push(format!("Opened page: {url}"));
            }
        }
        _ => summarize_json_block("web_search", raw, lines),
    }
}

fn summarize_json_block(label: &str, raw: &Value, lines: &mut Vec<String>) {
    if let Ok(serialized) = serde_json::to_string(raw) {
        lines.push(format!("{label}: {serialized}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_session_id_is_short_enough_for_openai_prompt_cache_key() {
        let id = web_sidecar_session_id(
            "session_00000000-0000-0000-0000-000000000000",
            "call_0123456789abcdefghijklmnopqrstuvwxyz",
        );

        assert!(id.len() <= 64);
        assert!(id.starts_with("web-"));
    }

    #[test]
    fn sidecar_session_id_varies_by_tool_call() {
        let first = web_sidecar_session_id("session", "call_a");
        let second = web_sidecar_session_id("session", "call_b");

        assert_ne!(first, second);
    }
}
