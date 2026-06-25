use std::collections::BTreeSet;

use agent_core::AgentInput;
use agent_session::SessionAction;
use agent_store::{ActionStatus, ActionUpdate, EventType};
use agent_tools::ToolContext;
use agent_vocab::{ToolResultMessage, ToolResultStatus, TranscriptItem};
use serde_json::json;

use crate::delegation_tools::{is_delegation_tool_name, run_delegation_tool};
use crate::provider_runtime::{is_web_tool_name, load_skill_result, prompt_profile, run_web_tool};
use crate::state::AppState;
use crate::types::{DispatchAction, RpcError};

use super::{agent_input_from_queued_priority, publish_events, SessionDriver};

pub(super) async fn run_tool_turn(
    state: AppState,
    session_id: String,
    dispatch: DispatchAction,
) -> std::result::Result<(), RpcError> {
    let SessionAction::RequestTool {
        action_id,
        turn_id,
        tool_call,
    } = dispatch.action
    else {
        return Ok(());
    };

    let events = state
        .repo
        .mark_action_running_and_event(
            &session_id,
            &dispatch.row_id,
            &dispatch.attempt_id,
            EventType::ToolStarted,
        )
        .await?;
    if events.is_empty() {
        return Ok(());
    }
    publish_events(&state, events);
    state
        .workspaces
        .ensure_session(
            &session_id,
            &dispatch.config.outer_cwd,
            &dispatch.config.workspaces,
        )
        .await?;

    let tool_context =
        ToolContext::new(std::path::PathBuf::from(dispatch.config.outer_cwd.clone()));
    let result = if tool_call.tool_name == "LoadSkill" {
        let loaded_skills = loaded_skills_for_session(&state, &session_id).await;
        load_skill_result(
            &state.prompt_root,
            &tool_context.cwd,
            &dispatch.config.workspaces,
            &loaded_skills,
            &tool_call,
            prompt_profile(&dispatch.config),
        )
    } else if is_web_tool_name(&tool_call.tool_name) {
        run_web_tool(
            &state,
            &dispatch.config,
            &session_id,
            &tool_call,
            &tool_context,
        )
        .await
    } else if is_delegation_tool_name(&tool_call.tool_name) {
        run_delegation_tool(&state, &session_id, &tool_call).await
    } else {
        match state
            .tools
            .execute(dispatch.config.provider.kind, &tool_call, &tool_context)
            .await
        {
            Ok(result) => result,
            Err(error) => ToolResultMessage::error(
                tool_call.id.clone(),
                tool_call.tool_name.clone(),
                format!("tool execution failed: {error}"),
            ),
        }
    };
    let status = if matches!(result.status, ToolResultStatus::Success) {
        ActionStatus::Completed
    } else {
        ActionStatus::Error
    };
    let driver = SessionDriver::acquire(&state, &session_id).await;
    if !state
        .repo
        .action_can_complete(&session_id, &dispatch.row_id, &dispatch.attempt_id)
        .await?
    {
        return Ok(());
    }
    let active = driver
        .active_session()
        .await
        .ok_or_else(|| RpcError::new("stale_action", "session is not active"))?;
    let mut consumed_input = None;
    {
        let mut runtime = active.lock().await;
        runtime
            .session
            .enqueue_input(AgentInput::ToolCompleted {
                action_id,
                turn_id,
                result: result.clone(),
            })
            .map_err(|error| RpcError::new("invalid_input", error.to_string()))?;
        runtime.session.drive();
    }
    let is_ready_to_continue = {
        let runtime = active.lock().await;
        runtime.session.is_ready_to_continue()
    };
    if is_ready_to_continue {
        if let Some(queued) = state.repo.take_next_queued_steer_input(&session_id).await? {
            let agent_input =
                agent_input_from_queued_priority(queued.priority, queued.content.clone());
            let enqueue_result = {
                let mut runtime = active.lock().await;
                runtime.session.enqueue_input(agent_input)
            };
            if let Err(error) = enqueue_result {
                state
                    .repo
                    .reset_consuming_input(&session_id, &queued.id, &queued.claim_id)
                    .await?;
                return Err(RpcError::new("invalid_input", error.to_string()));
            }
            consumed_input = Some(queued);
        }
        {
            let mut runtime = active.lock().await;
            runtime.session.drive();
        }
    }
    let dispatches = driver
        .persist_active_outputs(
            active,
            Some(ActionUpdate {
                row_id: dispatch.row_id,
                attempt_id: dispatch.attempt_id,
                status,
                result: serde_json::to_value(&result).unwrap_or_else(|_| json!({})),
            }),
            consumed_input,
            None,
            Vec::new(),
        )
        .await?;
    driver.dispatch(dispatches).await?;
    driver.drive_until_blocked().await?;
    Ok(())
}

async fn loaded_skills_for_session(state: &AppState, session_id: &str) -> BTreeSet<String> {
    let Some(active) = state.active.lock().await.get(session_id).cloned() else {
        return BTreeSet::new();
    };
    let runtime = active.lock().await;
    runtime
        .session
        .model_context()
        .transcript_items()
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ToolResult(result) if result.tool_name == "LoadSkill" => {
                loaded_skill_identifier(&result.output)
            }
            _ => None,
        })
        .collect()
}

fn loaded_skill_identifier(output: &str) -> Option<String> {
    loaded_skill_identifier_json(output)
}

fn loaded_skill_identifier_json(output: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    let name = value
        .get("skill_name")
        .and_then(serde_json::Value::as_str)?;
    let workspace = value
        .get("workspace")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|workspace| !workspace.is_empty());
    Some(crate::provider_runtime::skill_identifier(workspace, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_skill_identifier_accepts_json_output() {
        let output = serde_json::json!({
            "status": "loaded",
            "name": "repo/rust-refactor",
            "skill_name": "rust-refactor",
            "workspace": "repo",
            "content": "Prefer small, tested changes."
        })
        .to_string();

        assert_eq!(
            loaded_skill_identifier(&output),
            Some(crate::provider_runtime::skill_identifier(
                Some("repo"),
                "rust-refactor"
            ))
        );
    }

    #[test]
    fn loaded_skill_identifier_rejects_non_json_output() {
        let output = "not json";

        assert_eq!(loaded_skill_identifier(output), None);
    }

    #[test]
    fn loaded_skill_identifier_requires_current_json_shape() {
        let output = serde_json::json!({
            "status": "loaded",
            "name": "repo/rust-refactor",
            "workspace": "repo",
            "content": "Prefer small, tested changes."
        })
        .to_string();

        assert_eq!(loaded_skill_identifier(&output), None);
    }
}
