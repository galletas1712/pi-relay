use agent_core::AgentInput;
use agent_runtime_protocol::{RuntimeCommand, RuntimeCommandResult};
use agent_session::SessionAction;
use agent_store::{ActionStatus, ActionUpdate};
use agent_tools::{limit_tool_output, ToolContext};
use agent_vocab::{ToolResultMessage, ToolResultStatus};
use serde_json::json;

use crate::delegation_tools::{is_delegation_tool_name, run_delegation_tool};
use crate::provider_runtime::{is_web_tool_name, load_skill_result, run_web_tool};
use crate::state::AppState;
use crate::types::{DispatchAction, RpcError};

use super::{agent_input_from_queued_priority, SessionDriver};

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

    let is_mcp_tool = dispatch
        .mcp_snapshot
        .manifest()
        .tool(&tool_call.tool_name)
        .is_some();
    state
        .runtime_hosts
        .ensure_session(
            &session_id,
            &dispatch.config.workspace_id,
            &dispatch.config.workspaces,
        )
        .await?;

    let tool_context = ToolContext::new(std::path::PathBuf::from("/"));
    let mut result = if is_mcp_tool {
        // MCP servers run on the session's runtime; ship the manifest + call and
        // let the runtime resolve/execute it into a ToolResultMessage.
        match state
            .runtime_hosts
            .execute_mcp_tool(
                &dispatch.config.runtime_id,
                dispatch.mcp_snapshot.manifest().clone(),
                tool_call.clone(),
            )
            .await
        {
            Ok(result) => result,
            Err(error) => ToolResultMessage::error(
                tool_call.id.clone(),
                tool_call.tool_name.clone(),
                format!("MCP tool execution failed: {error:#}"),
            ),
        }
    } else if tool_call.tool_name == "LoadSkill" {
        let workspace_dirs = dispatch
            .config
            .workspaces
            .iter()
            .map(|workspace| workspace.workspace_dir.clone())
            .collect::<Vec<_>>();
        match state
            .runtime_hosts
            .read_runtime_context(
                &dispatch.config.runtime_id,
                &dispatch.config.workspace_id,
                &workspace_dirs,
            )
            .await
        {
            Ok(runtime_context) => load_skill_result(&runtime_context.skills, &tool_call),
            Err(error) => ToolResultMessage::error(
                tool_call.id.clone(),
                tool_call.tool_name.clone(),
                format!("failed to read runtime skills: {error:#}"),
            ),
        }
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
            .runtime_hosts
            .execute(
                &dispatch.config.runtime_id,
                RuntimeCommand::ExecuteTool {
                    workspace_id: dispatch.config.workspace_id.clone(),
                    provider: dispatch.config.provider.kind,
                    tool_call: tool_call.clone(),
                },
            )
            .await
        {
            Ok(RuntimeCommandResult::Tool { result }) => result,
            Ok(_) => ToolResultMessage::error(
                tool_call.id.clone(),
                tool_call.tool_name.clone(),
                "runtime returned the wrong tool result",
            ),
            Err(error) => ToolResultMessage::error(
                tool_call.id.clone(),
                tool_call.tool_name.clone(),
                format!("runtime tool execution failed: {error:#}"),
            ),
        }
    };
    // Completion persistence acquires the SessionDriver. Release the cwd guard
    // first so cancellation and source mutation never depend on both locks.
    finalize_tool_result(&mut result);
    let status = if matches!(result.status, ToolResultStatus::Success) {
        ActionStatus::Completed
    } else {
        ActionStatus::Error
    };
    let driver = SessionDriver::acquire(&state, &session_id).await;
    if !state
        .repo
        .action_can_complete(&session_id, &dispatch.row_id, &dispatch.attempt_id, None)
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
                queued.route.apply_to(&mut runtime.config);
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
                post_compaction_dispatch_lease: None,
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

fn escape_nul_in_tool_result(result: &mut ToolResultMessage) {
    // Rust strings and JSON permit U+0000, but PostgreSQL JSONB does not.
    if result.output.contains('\0') {
        result.output = result.output.replace('\0', "\\x00");
    }
}

fn finalize_tool_result(result: &mut ToolResultMessage) {
    escape_nul_in_tool_result(result);
    result.output = limit_tool_output(std::mem::take(&mut result.output));
}

#[cfg(test)]
mod tests {
    use agent_tools::{ToolContext, ToolRegistry};
    use agent_vocab::{ProviderKind, ToolCall, ToolCallId};

    use super::*;

    #[tokio::test]
    async fn escapes_nul_emitted_by_bash() {
        let call = ToolCall {
            id: ToolCallId::new("call_1"),
            tool_name: "Bash".to_string(),
            args_json: serde_json::json!({ "command": "printf 'before\\0after'" }).to_string(),
        };
        let mut result = ToolRegistry::with_builtin_tools()
            .execute(
                ProviderKind::OpenAi,
                &call,
                &ToolContext::new(std::env::temp_dir()),
            )
            .await
            .expect("bash execution succeeds");

        finalize_tool_result(&mut result);

        assert!(result.output.contains(r"before\x00after"));
        assert!(!result.output.contains('\0'));
        assert!(!serde_json::to_string(&result)
            .expect("serialize tool result")
            .contains(r"\u0000"));
    }

    #[test]
    fn nul_expansion_is_bounded_by_the_final_tool_output_limit() {
        let mut result = ToolResultMessage::success(
            ToolCallId::new("call"),
            "mcp__fixture__nul",
            "\0".repeat(40_000),
        );

        finalize_tool_result(&mut result);

        assert!(!result.output.contains('\0'));
        assert!(result.output.chars().count() <= 40_100);
        assert!(result.output.contains("[tool output truncated:"));
    }
}
