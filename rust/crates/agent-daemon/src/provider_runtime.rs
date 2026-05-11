use agent_provider::anthropic::AnthropicProvider;
use agent_provider::openai::OpenAiProvider;
use agent_provider::{ModelProvider, ModelRequest, PromptSections, ProviderError};
use agent_session::{AssistantMessage, ModelContext};
use agent_store::{ProviderKind, SessionConfig};
use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::auth::{refresh_codex_credentials, Credentials};
use crate::state::AppState;

pub(crate) async fn run_model(
    state: &AppState,
    config: &SessionConfig,
    model_context: ModelContext,
) -> Result<AssistantMessage> {
    let request = ModelRequest {
        model: config.provider.model.clone(),
        prompt: PromptSections::new(
            state.repo.global_system_prompt().await?,
            Some(dynamic_prompt_context(state)),
        ),
        transcript: model_context.into_transcript_items(),
        tools: state.tools.definitions(),
        max_tokens: config.provider.max_tokens,
        prompt_cache_key: config
            .provider
            .prompt_cache
            .as_ref()
            .and_then(|value| value.get("key"))
            .and_then(Value::as_str)
            .map(str::to_string),
    };

    let credentials = Credentials::load();
    let provider = provider_for_config(config, &credentials)?;
    match provider.complete(request.clone()).await {
        Ok(response) => Ok(response.assistant),
        Err(error)
            if config.provider.kind.is_codex() && provider_error_status(&error) == Some(401) =>
        {
            let credentials = refresh_codex_credentials().await?;
            let provider = provider_for_config(config, &credentials)?;
            Ok(provider.complete(request).await?.assistant)
        }
        Err(error) => Err(anyhow::Error::from(error)),
    }
}

fn dynamic_prompt_context(state: &AppState) -> String {
    format!(
        "Current working directory: {}",
        state.tool_context.cwd.display()
    )
}

fn provider_for_config(
    config: &SessionConfig,
    credentials: &Credentials,
) -> Result<Box<dyn ModelProvider>> {
    let provider: Box<dyn ModelProvider> =
        match config.provider.kind {
            ProviderKind::OpenAi => Box::new(OpenAiProvider::new(
                credentials.openai_api_key.clone().ok_or_else(|| {
                    anyhow!("OPENAI_API_KEY not found in env or ~/.codex/auth.json")
                })?,
            )),
            ProviderKind::Codex => Box::new(OpenAiProvider::codex(
                credentials.codex_access_token.clone().ok_or_else(|| {
                    anyhow!("CODEX_ACCESS_TOKEN or ~/.codex ChatGPT token not found")
                })?,
                credentials.codex_account_id.clone(),
            )),
            ProviderKind::Claude => Box::new(AnthropicProvider::new(
                credentials
                    .anthropic_api_key
                    .clone()
                    .ok_or_else(|| anyhow!("ANTHROPIC_API_KEY not found in env"))?,
            )),
        };
    Ok(provider)
}

fn provider_error_status(error: &ProviderError) -> Option<u16> {
    match error {
        ProviderError::Http(error) => error.status().map(|status| status.as_u16()),
        _ => None,
    }
}
