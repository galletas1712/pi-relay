use std::env;
use std::sync::Arc;

use agent_core::AgentInput;
use agent_provider::anthropic::AnthropicProvider;
use agent_provider::openai::OpenAiProvider;
use agent_provider::{ModelProvider, ModelRequest, PromptSections, ProviderToolProfile};
use agent_session::{AgentSession, SessionAction, SessionInput};
use agent_tools::{ToolContext, ToolRegistry};
use agent_vocab::ProviderKind;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let provider_name = args.next().unwrap_or_else(|| "claude".to_string());
    let model = args.next().unwrap_or_else(|| match provider_name.as_str() {
        "openai" => "gpt-5.5".to_string(),
        _ => "claude-sonnet-4-5".to_string(),
    });
    let prompt = args.collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        return Err("usage: pi-rs [claude|openai] [model] <prompt>".into());
    }

    let provider_kind = match provider_name.as_str() {
        "openai" => ProviderKind::OpenAi,
        "claude" | "anthropic" => ProviderKind::Claude,
        other => return Err(format!("unknown provider: {other}").into()),
    };
    let provider: Arc<dyn ModelProvider> = match provider_kind {
        ProviderKind::OpenAi => {
            let (access_token, account_id) = read_codex_auth()?;
            Arc::new(OpenAiProvider::codex(access_token, account_id))
        }
        ProviderKind::Claude => Arc::new(AnthropicProvider::new(env::var("ANTHROPIC_API_KEY")?)),
    };

    let mut session = AgentSession::new();
    let tools = ToolRegistry::with_builtin_tools();
    let tool_ctx = ToolContext::new(env::current_dir()?);
    session.enqueue_input(AgentInput::follow_up(prompt))?;

    loop {
        session.drive();
        let actions = session.drain_actions();
        if actions.is_empty() {
            break;
        }
        for action in actions {
            match action {
                SessionAction::RequestModel {
                    action_id,
                    turn_id,
                    model_context,
                    ..
                } => {
                    let response = provider
                        .complete(ModelRequest {
                            model: model.clone(),
                            prompt: PromptSections::new(
                                Some("You are a concise coding agent.".to_string()),
                                Some(format!(
                                    "Current working directory: {}",
                                    tool_ctx.cwd.display()
                                )),
                            ),
                            transcript: model_context
                                .into_transcript_items()
                                .into_iter()
                                .map(Into::into)
                                .collect(),
                            tool_profile: ProviderToolProfile::for_provider(provider_kind),
                            tools: tools.definitions_for_provider(provider_kind),
                            max_tokens: None,
                            reasoning_effort: agent_vocab::ReasoningEffort::XHigh,
                            prompt_cache_key: None,
                        })
                        .await?;
                    let context_tokens =
                        response.usage.as_ref().and_then(|usage| usage.input_tokens);
                    let text = response.assistant.text();
                    if !text.trim().is_empty() {
                        println!("{text}");
                    }
                    session.enqueue_session_input(SessionInput::ModelCompleted {
                        action_id,
                        turn_id,
                        assistant: response.assistant,
                        context_tokens,
                    })?;
                }
                SessionAction::RequestTool {
                    action_id,
                    turn_id,
                    tool_call,
                } => {
                    let result = match tools.execute(&tool_call, &tool_ctx).await {
                        Ok(result) => result,
                        Err(error) => agent_vocab::ToolResultMessage::error(
                            tool_call.id,
                            tool_call.tool_name,
                            error.to_string(),
                        ),
                    };
                    session.enqueue_input(AgentInput::ToolCompleted {
                        action_id,
                        turn_id,
                        result,
                    })?;
                }
                SessionAction::CancelSessionWork { .. } => {}
            }
        }
    }

    Ok(())
}

fn read_codex_auth() -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    if let Ok(token) = env::var("CODEX_ACCESS_TOKEN") {
        return Ok((token, env::var("CODEX_ACCOUNT_ID").ok()));
    }

    let path = env::var("HOME")? + "/.codex/auth.json";
    let contents = std::fs::read_to_string(&path)?;
    let value: serde_json::Value = serde_json::from_str(&contents)?;
    let access_token = value
        .pointer("/tokens/access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .ok_or("~/.codex/auth.json does not contain tokens.access_token")?
        .to_string();
    let account_id = value
        .pointer("/tokens/account_id")
        .and_then(serde_json::Value::as_str)
        .filter(|account| !account.trim().is_empty())
        .map(ToOwned::to_owned);
    Ok((access_token, account_id))
}
