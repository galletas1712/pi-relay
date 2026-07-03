use agent_core::AgentInput;
use agent_session::{ModelContext, SessionAction, SessionInput};
use agent_store::{ActionStatus, ActionUpdate};
use agent_vocab::TurnId;
use serde_json::{json, Value};

use crate::provider_runtime::{run_model, schedule_session_title_refresh_for_model_turn};
use crate::state::AppState;
use crate::types::{DispatchAction, RpcError};

use super::SessionDriver;

const MODEL_PROVIDER_MAX_ATTEMPTS: usize = 5;

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
    // Title generation is a sidecar fork at the same transcript checkpoint as
    // the model turn: immediately after the user message, before assistant
    // output is persisted.
    schedule_session_title_refresh_for_model_turn(
        &state,
        session_id.clone(),
        &dispatch.config,
        turn_id,
        &model_context,
    );

    let result =
        run_model_for_action_with_retries(&state, &session_id, &dispatch, turn_id, model_context)
            .await?;
    let driver = SessionDriver::acquire(&state, &session_id).await;
    if !state
        .repo
        .action_can_complete(
            &session_id,
            &dispatch.row_id,
            &dispatch.attempt_id,
            dispatch.post_compaction_dispatch_lease.as_ref(),
        )
        .await?
    {
        return Ok(());
    }
    let active = driver
        .active_session()
        .await
        .ok_or_else(|| RpcError::new("stale_action", "session is not active"))?;
    let dispatches = match result {
        Ok(response) => {
            apply_model_response(
                &state,
                &session_id,
                &driver,
                active,
                &dispatch,
                action_id,
                turn_id,
                response,
            )
            .await?
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
                        row_id: dispatch.row_id.clone(),
                        attempt_id: dispatch.attempt_id.clone(),
                        post_compaction_dispatch_lease: dispatch
                            .post_compaction_dispatch_lease
                            .clone(),
                        status: ActionStatus::Error,
                        result: update_result,
                    }),
                )
                .await?
        }
    };
    pause_after_model_transition_for_test(&dispatch).await;
    driver.dispatch(dispatches).await?;
    driver.drive_until_blocked().await?;
    Ok(())
}

fn model_provider_max_attempts_for_test(_config: &agent_store::SessionConfig) -> usize {
    #[cfg(test)]
    if let Some(attempts) = _config
        .metadata
        .pointer("/fault_injection/model_provider_max_attempts")
        .and_then(serde_json::Value::as_u64)
    {
        return usize::try_from(attempts).unwrap_or(1).max(1);
    }
    MODEL_PROVIDER_MAX_ATTEMPTS
}

async fn pause_after_model_transition_for_test(_dispatch: &DispatchAction) {
    #[cfg(test)]
    if let Some(milliseconds) = _dispatch
        .config
        .metadata
        .pointer("/fault_injection/pause_after_model_transition_ms")
        .and_then(serde_json::Value::as_u64)
    {
        tokio::time::sleep(std::time::Duration::from_millis(milliseconds)).await;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_model_response(
    state: &AppState,
    session_id: &str,
    driver: &SessionDriver,
    active: std::sync::Arc<tokio::sync::Mutex<crate::types::RuntimeSession>>,
    dispatch: &DispatchAction,
    action_id: agent_vocab::ActionId,
    turn_id: TurnId,
    response: agent_provider::ModelResponse,
) -> std::result::Result<Vec<DispatchAction>, RpcError> {
    if response.usage.is_some()
        && matches!(
            response.stop_reason,
            agent_provider::ModelStopReason::Complete
                | agent_provider::ModelStopReason::MaxOutputTokens
        )
    {
        state
            .repo
            .reset_auto_compaction_failures(session_id)
            .await?;
    }
    let stop_reason = response.stop_reason;
    let error = model_response_error(&response);
    let action_status = if stop_reason == agent_provider::ModelStopReason::Complete {
        ActionStatus::Completed
    } else {
        ActionStatus::Error
    };
    let action_update = Some(ActionUpdate {
        row_id: dispatch.row_id.clone(),
        attempt_id: dispatch.attempt_id.clone(),
        post_compaction_dispatch_lease: dispatch.post_compaction_dispatch_lease.clone(),
        status: action_status,
        result: json!({
            "source": "provider",
            "usage": response.usage,
            "stop_reason": stop_reason,
            "stop_details": response.stop_details,
            "error": error,
        }),
    });
    let provider_replay = response.provider_replay;
    match stop_reason {
        agent_provider::ModelStopReason::Complete => {
            driver
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
                .await
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
                        error: error.unwrap_or_else(|| {
                            "provider response hit max_output_tokens".to_string()
                        }),
                    },
                    action_update,
                    Vec::new(),
                )
                .await
        }
        agent_provider::ModelStopReason::Compaction => {
            let error =
                "unexpected provider compaction stop during an ordinary model turn".to_string();
            driver
                .apply_agent_input(
                    active,
                    AgentInput::ModelFailed {
                        action_id,
                        turn_id,
                        error,
                    },
                    action_update,
                )
                .await
        }
        agent_provider::ModelStopReason::Refusal => {
            let error = error.unwrap_or_else(|| "provider refused the request".to_string());
            eprintln!(
                "model provider refusal for {session_id}/{}: {error}",
                dispatch.row_id
            );
            driver
                .apply_agent_input(
                    active,
                    AgentInput::ModelFailed {
                        action_id,
                        turn_id,
                        error,
                    },
                    action_update,
                )
                .await
        }
    }
}

fn model_response_error(response: &agent_provider::ModelResponse) -> Option<String> {
    match response.stop_reason {
        agent_provider::ModelStopReason::Complete => None,
        agent_provider::ModelStopReason::MaxOutputTokens => {
            Some("provider response hit max_output_tokens".to_string())
        }
        agent_provider::ModelStopReason::Refusal => response.refusal_error(),
        agent_provider::ModelStopReason::Compaction => {
            Some("unexpected provider compaction stop during an ordinary model turn".to_string())
        }
    }
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
    let max_attempts = model_provider_max_attempts_for_test(&dispatch.config);
    for attempt in 1..=max_attempts {
        if attempt > 1
            && !state
                .repo
                .action_can_complete(
                    session_id,
                    &dispatch.row_id,
                    &dispatch.attempt_id,
                    dispatch.post_compaction_dispatch_lease.as_ref(),
                )
                .await?
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
                let provider_error = match error.downcast::<agent_provider::ProviderError>() {
                    Ok(error) => error,
                    Err(error) => return Err(error.into()),
                };
                if attempt >= max_attempts {
                    return Ok(Err(ModelProviderFailure {
                        error: provider_error,
                        attempts: attempt,
                    }));
                }
                if !state
                    .repo
                    .action_can_complete(
                        session_id,
                        &dispatch.row_id,
                        &dispatch.attempt_id,
                        dispatch.post_compaction_dispatch_lease.as_ref(),
                    )
                    .await?
                {
                    return Err(RpcError::new(
                        "stale_action",
                        "action attempt is no longer running",
                    ));
                }
                let message = provider_error_retry_diagnostic(&provider_error);
                eprintln!(
                    "model provider error for {session_id}/{} on attempt {attempt}/{MODEL_PROVIDER_MAX_ATTEMPTS}; retrying: {message}",
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

fn model_failure_update_result(failure: &ModelProviderFailure) -> Value {
    let mut result = json!({ "error": failure.error.to_string() });
    if failure.attempts > 1 {
        result["provider_retry_attempts"] = json!(failure.attempts);
    }
    if let Some(diagnostic) = failure.error.retry_diagnostic() {
        result["provider_error_diagnostic"] = json!(diagnostic);
    }
    result
}

fn provider_error_retry_diagnostic(error: &agent_provider::ProviderError) -> String {
    error
        .retry_diagnostic()
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_provider::{ModelResponse, ModelStopDetails, ModelStopReason};
    use agent_vocab::AssistantMessage;

    #[test]
    fn refusal_terminal_error_surfaces_category_and_explanation() {
        let response = ModelResponse {
            assistant: AssistantMessage { items: Vec::new() },
            provider_replay: Vec::new(),
            usage: None,
            stop_reason: ModelStopReason::Refusal,
            stop_details: Some(ModelStopDetails {
                category: Some("cyber".to_string()),
                explanation: Some(
                    "This request was declined because it could enable cyber harm.".to_string(),
                ),
            }),
        };

        assert_eq!(
            model_response_error(&response).as_deref(),
            Some(
                "provider refused the request (cyber): This request was declined because it could enable cyber harm."
            )
        );
    }
}
