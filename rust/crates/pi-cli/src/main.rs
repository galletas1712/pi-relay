use std::env;
use std::sync::Arc;

use agent_core::AgentInput;
use agent_provider::anthropic::AnthropicProvider;
use agent_provider::openai::OpenAiProvider;
use agent_provider::{ModelProvider, ModelRequest, PromptSections};
use agent_session::{AgentSession, SessionAction, SessionInput};
use agent_tools::{ToolContext, ToolRegistry};

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
        "openai" => "gpt-4.1".to_string(),
        _ => "claude-sonnet-4-5".to_string(),
    });
    let prompt = args.collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        return Err("usage: pi-rs [claude|openai] [model] <prompt>".into());
    }

    let provider: Arc<dyn ModelProvider> = match provider_name.as_str() {
        "openai" => Arc::new(OpenAiProvider::new(env::var("OPENAI_API_KEY")?)),
        "claude" | "anthropic" => Arc::new(AnthropicProvider::new(env::var("ANTHROPIC_API_KEY")?)),
        other => return Err(format!("unknown provider: {other}").into()),
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
                            tools: tools.definitions(),
                            max_tokens: None,
                            prompt_cache_key: None,
                        })
                        .await?;
                    let text = response.assistant.text();
                    if !text.trim().is_empty() {
                        println!("{text}");
                    }
                    session.enqueue_session_input(SessionInput::ModelCompleted {
                        action_id,
                        turn_id,
                        assistant: response.assistant,
                        context_tokens: None,
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
