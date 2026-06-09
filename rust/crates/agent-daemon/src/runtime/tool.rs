use std::collections::BTreeSet;

use agent_core::AgentInput;
use agent_session::SessionAction;
use agent_store::{ActionStatus, ActionUpdate, EventType};
use agent_tools::ToolContext;
use agent_vocab::{ToolResultMessage, ToolResultStatus, TranscriptItem};
use serde_json::json;

use crate::provider_runtime::{is_web_tool_name, load_skill_result, run_web_tool};
use crate::state::AppState;
use crate::subagents::{
    subagent_list_for_parent, subagent_send_for_parent, subagent_spawn_for_parent,
    subagent_tail_for_parent,
};
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
        .await
        .map_err(anyhow::Error::from)?;
    if events.is_empty() {
        return Ok(());
    }
    publish_events(&state, events);
    let tool_action_row_id = dispatch.row_id.clone();
    state
        .workspaces
        .ensure_session(
            &session_id,
            &dispatch.config.outer_cwd,
            &dispatch.config.workspaces,
        )
        .await
        .map_err(anyhow::Error::from)?;

    let tool_context =
        ToolContext::new(std::path::PathBuf::from(dispatch.config.outer_cwd.clone()));
    let result = if tool_call.tool_name == "LoadSkill" {
        let loaded_skills = loaded_skills_for_session(&state, &session_id).await;
        load_skill_result(
            &tool_context.cwd,
            &dispatch.config.workspaces,
            &loaded_skills,
            &tool_call,
        )
    } else if is_subagent_tool_name(&tool_call.tool_name) {
        run_subagent_tool(&state, &session_id, &tool_action_row_id, &tool_call).await
    } else if is_web_tool_name(&tool_call.tool_name) {
        run_web_tool(
            &state,
            &dispatch.config,
            &session_id,
            &tool_call,
            &tool_context,
        )
        .await
    } else {
        match state
            .tools
            .execute(dispatch.config.provider.kind, &tool_call, &tool_context)
            .await
        {
            Ok(result) => result,
            Err(_) => ToolResultMessage::crashed(tool_call.id.clone(), tool_call.tool_name.clone()),
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
        .await
        .map_err(anyhow::Error::from)?
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
        if let Some(queued) = state
            .repo
            .take_next_queued_steer_input(&session_id)
            .await
            .map_err(anyhow::Error::from)?
        {
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
                    .await
                    .map_err(anyhow::Error::from)?;
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
    let rest = output.strip_prefix("<loaded_skill>\n<name>")?;
    let end = rest.find("</name>")?;
    let name = xml_unescape(&rest[..end]);
    let after_name = &rest[end + "</name>".len()..];
    let workspace = if let Some(workspace_rest) = after_name.strip_prefix("\n<workspace>") {
        let workspace_end = workspace_rest.find("</workspace>")?;
        Some(xml_unescape(&workspace_rest[..workspace_end]))
    } else {
        None
    };
    Some(crate::provider_runtime::skill_identifier(
        workspace.as_deref(),
        &name,
    ))
}

fn is_subagent_tool_name(name: &str) -> bool {
    matches!(
        name,
        "SubagentSpawn" | "SubagentList" | "SubagentSend" | "SubagentTail"
    )
}

async fn run_subagent_tool(
    state: &AppState,
    session_id: &str,
    action_row_id: &str,
    call: &agent_vocab::ToolCall,
) -> ToolResultMessage {
    let args = match call.args_value() {
        Ok(args) => args,
        Err(error) => {
            return ToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                format!("subagent tool arguments were invalid JSON: {error}"),
            )
        }
    };
    let result = match call.tool_name.as_str() {
        "SubagentSpawn" => {
            subagent_spawn_for_parent(state, session_id, args, Some(action_row_id)).await
        }
        "SubagentList" => subagent_list_for_parent(state, session_id).await,
        "SubagentSend" => subagent_send_for_parent(state, session_id, args).await,
        "SubagentTail" => subagent_tail_for_parent(state, session_id, args).await,
        _ => Err(RpcError::new("unknown_tool", "unknown subagent tool")),
    };
    match result {
        Ok(value) => ToolResultMessage::success(
            call.id.clone(),
            &call.tool_name,
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
        ),
        Err(error) => ToolResultMessage::error(call.id.clone(), &call.tool_name, error.message),
    }
}

fn xml_unescape(input: &str) -> String {
    input
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}
