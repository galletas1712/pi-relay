use agent_provider::anthropic::AnthropicProvider;
use agent_provider::openai::OpenAiProvider;
use agent_provider::ModelProvider;
use agent_store::SessionConfig;
use agent_vocab::ProviderKind;
use anyhow::{anyhow, Result};

use crate::auth::Credentials;

pub(super) struct ProviderHandle {
    pub(super) provider: Box<dyn ModelProvider>,
    pub(super) uses_codex_auth: bool,
}

pub(super) fn provider_for_config(
    config: &SessionConfig,
    credentials: &Credentials,
) -> Result<ProviderHandle> {
    let handle = match config.provider.kind {
        ProviderKind::OpenAi => ProviderHandle {
            provider: Box::new(OpenAiProvider::codex(
                credentials.codex_access_token.clone().ok_or_else(|| {
                    anyhow!("~/.codex ChatGPT token not found for OpenAI subscription transport")
                })?,
                credentials.codex_account_id.clone(),
                credentials.codex_installation_id.clone(),
            )),
            uses_codex_auth: true,
        },
        ProviderKind::Claude => ProviderHandle {
            provider: Box::new(AnthropicProvider::new(
                credentials.anthropic_api_key.clone().ok_or_else(|| {
                    anyhow!("ANTHROPIC_API_KEY not found in env or Claude Code config")
                })?,
            )),
            uses_codex_auth: false,
        },
    };
    Ok(handle)
}
