use agent_core::AgentInput;
use agent_session::{ModelContext, SessionAction, SessionInput};
use agent_store::{ActionStatus, ActionUpdate};
use agent_vocab::TurnId;
use serde_json::{json, Value};

use crate::provider_runtime::{run_model, schedule_session_title_refresh_after_model};
use crate::state::AppState;
use crate::types::{DispatchAction, RpcError};

use super::SessionDriver;

const MODEL_PROVIDER_MAX_ATTEMPTS: usize = 3;

pub(super) async fn run_model_turn(
    state: AppState,
    session_id: String,
    dispatch: DispatchAction,
) -> std::result::Result<(), RpcError> {
    let original_action = dispatch.action.clone();
    let SessionAction::RequestModel {
        action_id,
        turn_id,
        model_context,
        context_leaf_id,
    } = original_action
    else {
        return Ok(());
    };
    let dispatch = DispatchAction {
        action: SessionAction::RequestModel {
            action_id,
            turn_id,
            model_context: model_context.clone(),
            context_leaf_id,
        },
        ..dispatch
    };
    let title_model_context = model_context.clone();

    let result =
        run_model_for_action_with_retries(&state, &session_id, &dispatch, turn_id, model_context)
            .await?;
    let driver = SessionDriver::acquire(&state, &session_id).await;
    if !state
        .repo
        .action_can_complete(&session_id, &dispatch.row_id, &dispatch.attempt_id)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Ok(());
    }
    let active = driver
        .active_session()
        .await
        .ok_or_else(|| RpcError::new("stale_action", "session is not active"))?;
    let dispatches = match result {
        Ok(response) => {
            if response.usage.is_some() {
                state
                    .repo
                    .reset_auto_compaction_failures(&session_id)
                    .await
                    .map_err(anyhow::Error::from)?;
            }
            let stop_reason = response.stop_reason;
            let max_output_tokens = matches!(
                stop_reason,
                agent_provider::ModelStopReason::MaxOutputTokens
            );
            let error = max_output_tokens.then_some("provider response hit max_output_tokens");
            let action_update = Some(ActionUpdate {
                row_id: dispatch.row_id,
                attempt_id: dispatch.attempt_id,
                status: if max_output_tokens {
                    ActionStatus::Error
                } else {
                    ActionStatus::Completed
                },
                result: json!({
                    "source": "provider",
                    "usage": response.usage,
                    "stop_reason": stop_reason,
                    "error": error,
                }),
            });
            let provider_replay = response.provider_replay;
            match stop_reason {
                agent_provider::ModelStopReason::Complete => {
                    let dispatches = driver
                        .apply_session_input(
                            active,
                            SessionInput::ModelCompleted {
                                action_id,
                                turn_id,
                                assistant: response.assistant,
                            },
                            action_update,
                            provider_replay,
                        )
                        .await?;
                    schedule_session_title_refresh_after_model(
                        &state,
                        session_id.clone(),
                        &dispatch.config,
                        &title_model_context,
                    );
                    dispatches
                }
                agent_provider::ModelStopReason::MaxOutputTokens => {
                    driver
                        .apply_session_input(
                            active,
                            SessionInput::ModelMaxOutputTokens {
                                action_id,
                                turn_id,
                                assistant: response.assistant,
                                provider_replay,
                                error: error
                                    .unwrap_or("provider response hit max_output_tokens")
                                    .to_string(),
                            },
                            action_update,
                            Vec::new(),
                        )
                        .await?
                }
            }
        }
        Err(error) => {
            if super::compaction::recover_model_context_overflow_with_compaction(
                &state,
                &session_id,
                &dispatch,
                &error.error,
            )
            .await?
            {
                return Ok(());
            }
            let message = error.error.to_string();
            let update_result = model_failure_update_result(&error);
            driver
                .apply_agent_input(
                    active,
                    AgentInput::ModelFailed {
                        action_id,
                        turn_id,
                        error: message.clone(),
                    },
                    Some(ActionUpdate {
                        row_id: dispatch.row_id,
                        attempt_id: dispatch.attempt_id,
                        status: ActionStatus::Error,
                        result: update_result,
                    }),
                )
                .await?
        }
    };
    driver.dispatch(dispatches).await?;
    driver.drive_until_blocked().await?;
    Ok(())
}

async fn run_model_for_action_with_retries(
    state: &AppState,
    session_id: &str,
    dispatch: &DispatchAction,
    turn_id: TurnId,
    model_context: ModelContext,
) -> std::result::Result<
    std::result::Result<agent_provider::ModelResponse, ModelProviderFailure>,
    RpcError,
> {
    for attempt in 1..=MODEL_PROVIDER_MAX_ATTEMPTS {
        if attempt > 1
            && !state
                .repo
                .action_can_complete(session_id, &dispatch.row_id, &dispatch.attempt_id)
                .await
                .map_err(anyhow::Error::from)?
        {
            return Err(RpcError::new(
                "stale_action",
                "action attempt is no longer running",
            ));
        }
        let result = run_model(
            state,
            &dispatch.config,
            session_id,
            turn_id,
            model_context.clone(),
        )
        .await;
        match result {
            Ok(response) => return Ok(Ok(response)),
            Err(error) => {
                let Some(provider_error) = error.downcast_ref::<agent_provider::ProviderError>()
                else {
                    return Err(anyhow::Error::from(error).into());
                };
                let retryable = provider_error.is_retryable_transient();
                if attempt >= MODEL_PROVIDER_MAX_ATTEMPTS || !retryable {
                    let error = provider_error_from_anyhow(error);
                    let attempts = if retryable { attempt } else { 1 };
                    return Ok(Err(ModelProviderFailure { error, attempts }));
                }
                if !state
                    .repo
                    .action_can_complete(session_id, &dispatch.row_id, &dispatch.attempt_id)
                    .await
                    .map_err(anyhow::Error::from)?
                {
                    return Err(RpcError::new(
                        "stale_action",
                        "action attempt is no longer running",
                    ));
                }
                let message = provider_error_retry_diagnostic(&error);
                eprintln!(
                    "model provider transient error for {session_id}/{} on attempt {attempt}/{MODEL_PROVIDER_MAX_ATTEMPTS}; retrying: {message}",
                    dispatch.row_id
                );
                tokio::time::sleep(model_retry_backoff(attempt)).await;
            }
        }
    }

    unreachable!("retry loop either returns provider result or stale action")
}

struct ModelProviderFailure {
    error: agent_provider::ProviderError,
    attempts: usize,
}

fn provider_error_from_anyhow(error: anyhow::Error) -> agent_provider::ProviderError {
    match error.downcast::<agent_provider::ProviderError>() {
        Ok(error) => error,
        Err(error) => agent_provider::ProviderError::Provider(error.to_string()),
    }
}

fn model_failure_update_result(failure: &ModelProviderFailure) -> Value {
    let mut result = json!({ "error": failure.error.to_string() });
    if failure.attempts > 1 {
        result["provider_retry_attempts"] = json!(failure.attempts);
    }
    if failure.attempts > 1 || failure.error.is_retryable_transient() {
        if let Some(diagnostic) = failure.error.retry_diagnostic() {
            result["provider_error_diagnostic"] = json!(diagnostic);
        }
    }
    result
}

fn provider_error_retry_diagnostic(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<agent_provider::ProviderError>()
        .and_then(agent_provider::ProviderError::retry_diagnostic)
        .unwrap_or_else(|| error.to_string())
}

fn model_retry_backoff(completed_attempt: usize) -> std::time::Duration {
    let millis = match completed_attempt {
        1 => 250,
        2 => 1_000,
        _ => 3_000,
    };
    std::time::Duration::from_millis(millis)
}
