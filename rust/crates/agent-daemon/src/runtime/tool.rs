use agent_core::AgentInput;
use agent_runtime_protocol::{RuntimeCommand, RuntimeCommandResult};
use agent_session::SessionAction;
use agent_store::{ActionStatus, ActionUpdate};
use agent_tools::{
    bash_call_for_execution, finalize_tool_result_content_with_max_tokens,
    requested_tool_output_limit, ToolContext,
};
use agent_vocab::{InlineToolResultMessage, ToolResultMessage, ToolResultStatus};
use cap_std::{ambient_authority, fs::Dir};
use serde_json::json;

use crate::delegation_tools::is_delegation_tool_name;
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
    let execution_call = bash_call_for_execution(&tool_call);
    let max_output_tokens = requested_tool_output_limit(&execution_call);
    state
        .runtime_hosts
        .ensure_session(
            &session_id,
            &dispatch.config.workspace_id,
            &dispatch.config.workspaces,
        )
        .await?;

    let tool_context = ToolContext::new(
        std::path::PathBuf::from("/"),
        Dir::open_ambient_dir("/", ambient_authority())
            .map_err(|error| RpcError::new("tool_context_unavailable", error.to_string()))?,
    );
    let mut result = if is_mcp_tool {
        // MCP servers run on the session's runtime; ship the manifest + call and
        // let the runtime resolve/execute it into a ToolResultMessage.
        match state
            .runtime_hosts
            .execute_mcp_tool(
                &dispatch.config.runtime_id,
                dispatch.mcp_snapshot.manifest().clone(),
                execution_call.clone(),
            )
            .await
        {
            Ok(result) => result,
            Err(error) => InlineToolResultMessage::error(
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
        match crate::provider_runtime::home_project_key(&state.repo, dispatch.config.project_id)
            .await
        {
            Ok(project_key) => match state
                .runtime_hosts
                .read_runtime_context(
                    &dispatch.config.runtime_id,
                    &dispatch.config.workspace_id,
                    &workspace_dirs,
                    project_key,
                )
                .await
            {
                Ok(runtime_context) => {
                    load_skill_result(&runtime_context.skills, &execution_call).into_inline_text()
                }
                Err(error) => InlineToolResultMessage::error(
                    tool_call.id.clone(),
                    tool_call.tool_name.clone(),
                    format!("failed to read runtime skills: {error:#}"),
                ),
            },
            Err(error) => ToolResultMessage::error(
                tool_call.id.clone(),
                tool_call.tool_name.clone(),
                format!("failed to resolve project skills: {error:#}"),
            )
            .into_inline_text(),
        }
    } else if is_web_tool_name(&tool_call.tool_name) {
        run_web_tool(
            &state,
            &dispatch.config,
            &session_id,
            &execution_call,
            &tool_context,
        )
        .await
        .into_inline_text()
    } else if is_delegation_tool_name(&tool_call.tool_name) {
        crate::delegation_tools::run_delegation_tool_with_launch_key(
            &state,
            &session_id,
            &format!("action:{}", dispatch.row_id),
            &execution_call,
        )
        .await
        .into_inline_text()
    } else {
        match state
            .runtime_hosts
            .execute(
                &dispatch.config.runtime_id,
                RuntimeCommand::ExecuteTool {
                    workspace_id: dispatch.config.workspace_id.clone(),
                    provider: dispatch.config.provider.kind,
                    tool_call: execution_call,
                },
                None,
            )
            .await
        {
            Ok(RuntimeCommandResult::Tool { result }) => result,
            Ok(_) => InlineToolResultMessage::error(
                tool_call.id.clone(),
                tool_call.tool_name.clone(),
                "runtime returned the wrong tool result",
            ),
            Err(error) => InlineToolResultMessage::error(
                tool_call.id.clone(),
                tool_call.tool_name.clone(),
                format!("runtime tool execution failed: {error:#}"),
            ),
        }
    };
    // Completion persistence acquires the SessionDriver. Release the cwd guard
    // first so cancellation and source mutation never depend on both locks.
    finalize_tool_result(&mut result, max_output_tokens);
    let driver = SessionDriver::acquire(&state, &session_id).await;
    if !state
        .repo
        .action_can_complete(&session_id, &dispatch.row_id, &dispatch.attempt_id, None)
        .await?
    {
        return Ok(());
    }
    let result = state
        .repo
        .ingest_tool_result(result)
        .await
        .map_err(anyhow::Error::new)?;
    let status = if matches!(result.status, ToolResultStatus::Success) {
        ActionStatus::Completed
    } else {
        ActionStatus::Error
    };
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

fn finalize_tool_result(result: &mut InlineToolResultMessage, max_output_tokens: Option<usize>) {
    // Rust strings and JSON permit U+0000, but PostgreSQL JSONB does not.
    // Shared finalize also truncates text and applies the image policy.
    finalize_tool_result_content_with_max_tokens(result, max_output_tokens);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_provider::{normalize_transcript_for_provider, ModelTranscriptEntry};
    use agent_store::PostgresAgentStore;
    use agent_tools::{ToolContext, ToolRegistry};
    use agent_vocab::{
        ContentBlock, InlineContentBlock, ProviderKind, ToolCall, ToolCallId, TranscriptItem,
    };

    use super::*;

    fn test_tool_context() -> ToolContext {
        let cwd = std::env::temp_dir();
        ToolContext::new(
            &cwd,
            Dir::open_ambient_dir(&cwd, ambient_authority()).expect("open test temp directory"),
        )
    }

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(70_000);

    #[tokio::test]
    async fn escapes_nul_emitted_by_bash() {
        let call = ToolCall {
            id: ToolCallId::new("call_1"),
            tool_name: "Bash".to_string(),
            args_json: serde_json::json!({ "command": "printf 'before\\0after'" }).to_string(),
        };
        let mut result = ToolRegistry::with_builtin_tools()
            .execute(ProviderKind::OpenAi, &call, &test_tool_context())
            .await
            .expect("bash execution succeeds");

        finalize_tool_result(&mut result, None);

        let text = result.display_text();
        assert!(text.contains(r"before\x00after"));
        assert!(!text.contains('\0'));
        assert!(!serde_json::to_string(&result)
            .expect("serialize tool result")
            .contains(r"\u0000"));
    }

    #[test]
    fn nul_expansion_is_bounded_by_the_final_tool_output_limit() {
        let mut result = InlineToolResultMessage::success(
            ToolCallId::new("call"),
            "mcp__fixture__nul",
            "\0".repeat(40_000),
        );

        finalize_tool_result(&mut result, None);

        let text = result.display_text();
        assert!(!text.contains('\0'));
        assert!(text.chars().count() <= 40_100);
        assert!(text.contains("[tool output truncated:"));
    }

    #[tokio::test]
    async fn oversized_bash_keeps_original_head_tail_and_count_across_refinalization() {
        let call = oversized_bash_call();
        let mut result = ToolRegistry::with_builtin_tools()
            .execute(ProviderKind::OpenAi, &call, &test_tool_context())
            .await
            .expect("bash execution succeeds");
        let original = result.display_text();
        let total = original.chars().count();
        let expected_head = original.chars().take(24_000).collect::<String>();
        let expected_tail = original
            .chars()
            .skip(total.saturating_sub(16_000))
            .collect::<String>();

        finalize_tool_result(&mut result, requested_tool_output_limit(&call));
        let once = serde_json::to_vec(&result).expect("serialize finalized result");
        finalize_tool_result(&mut result, requested_tool_output_limit(&call));

        assert_eq!(
            serde_json::to_vec(&result).expect("serialize refinalized result"),
            once
        );
        let finalized = result.display_text();
        assert!(finalized.starts_with(&expected_head));
        assert!(finalized.ends_with(&expected_tail));
        assert!(finalized.contains(&format!(
            "[tool output truncated: {} characters omitted]",
            total - 40_000
        )));
    }

    #[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
    #[tokio::test]
    async fn oversized_local_and_mcp_results_stay_stable_through_store_and_provider() {
        let Ok(admin_url) = std::env::var("PI_RELAY_TEST_DATABASE_URL") else {
            eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
            return;
        };
        let name = format!(
            "pi_relay_tool_chain_test_{}_{}",
            std::process::id(),
            TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = sqlx::PgPool::connect(&admin_url)
            .await
            .expect("connect test administrator database");
        sqlx::query(&format!(r#"create database "{name}""#))
            .execute(&admin)
            .await
            .expect("create isolated tool-chain test database");
        admin.close().await;
        let store = PostgresAgentStore::connect(&database_url_with_name(&admin_url, &name))
            .await
            .expect("connect isolated tool-chain database");
        store.migrate().await.expect("install real schema");

        let bash_call = oversized_bash_call();
        let mut bash = ToolRegistry::with_builtin_tools()
            .execute(ProviderKind::OpenAi, &bash_call, &test_tool_context())
            .await
            .expect("bash execution succeeds");
        let original_bash = bash.display_text();
        let original_bash_chars = original_bash.chars().count();
        let expected_bash_head = original_bash.chars().take(24_000).collect::<String>();
        let expected_bash_tail = original_bash
            .chars()
            .skip(original_bash_chars.saturating_sub(16_000))
            .collect::<String>();
        finalize_tool_result(&mut bash, requested_tool_output_limit(&bash_call));
        let bash_text = bash.display_text();
        assert!(bash_text.starts_with(&expected_bash_head));
        assert!(bash_text.ends_with(&expected_bash_tail));
        assert!(bash_text.contains(&format!(
            "[tool output truncated: {} characters omitted]",
            original_bash_chars - 40_000
        )));
        let durable_bash = store
            .ingest_tool_result(bash)
            .await
            .expect("ingest finalized Bash result");
        let durable_bash_bytes =
            serde_json::to_vec(&durable_bash).expect("serialize durable Bash result");
        let provider_bash = normalize_transcript_for_provider(vec![ModelTranscriptEntry::from(
            TranscriptItem::ToolResult(durable_bash),
        )]);
        let TranscriptItem::ToolResult(provider_bash) = &provider_bash[0].item else {
            panic!("provider transcript must retain the Bash result");
        };
        assert_eq!(
            serde_json::to_vec(provider_bash).expect("serialize provider Bash result"),
            durable_bash_bytes
        );

        let png = agent_vocab::encode_base64(&tiny_png());
        let mut result = InlineToolResultMessage::success_content(
            ToolCallId::new("call_mcp_mixed"),
            "mcp__fixture__mixed",
            vec![
                InlineContentBlock::text("h".repeat(24_000)),
                InlineContentBlock::image("image/png", png.clone()),
                InlineContentBlock::text("x".repeat(5_000)),
                InlineContentBlock::image("image/png", png),
                InlineContentBlock::text("t".repeat(16_000)),
            ],
        );
        finalize_tool_result(&mut result, None);
        let once = serde_json::to_vec(&result).expect("serialize finalized MCP result");
        finalize_tool_result(&mut result, None);
        assert_eq!(
            serde_json::to_vec(&result).expect("serialize refinalized MCP result"),
            once
        );

        let durable = store
            .ingest_tool_result(result)
            .await
            .expect("artifactize finalized MCP result");
        assert!(matches!(
            durable.content.as_slice(),
            [
                ContentBlock::Text { text: head },
                ContentBlock::Image { .. },
                ContentBlock::Text { text: note },
                ContentBlock::Image { .. },
                ContentBlock::Text { text: tail },
            ] if head == &"h".repeat(24_000)
                && note == "[tool output truncated: 5000 characters omitted]"
                && tail == &"t".repeat(16_000)
        ));
        let durable_bytes = serde_json::to_vec(&durable).expect("serialize durable MCP result");

        let provider = normalize_transcript_for_provider(vec![ModelTranscriptEntry::from(
            TranscriptItem::ToolResult(durable),
        )]);
        let TranscriptItem::ToolResult(provider_result) = &provider[0].item else {
            panic!("provider transcript must retain the tool result");
        };
        assert_eq!(
            serde_json::to_vec(provider_result).expect("serialize provider MCP result"),
            durable_bytes
        );

        store.close().await;
        let admin = sqlx::PgPool::connect(&admin_url)
            .await
            .expect("reconnect test administrator database");
        sqlx::query(&format!(r#"drop database "{name}""#))
            .execute(&admin)
            .await
            .expect("drop only isolated tool-chain test database");
        admin.close().await;
    }

    fn tiny_png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ]
    }

    fn oversized_bash_call() -> ToolCall {
        ToolCall {
            id: ToolCallId::new("call_oversized_bash"),
            tool_name: "Bash".to_string(),
            args_json: serde_json::json!({
                "command": concat!(
                    "printf '%*s' 24000 '' | tr ' ' h; ",
                    "printf '%*s' 5000 '' | tr ' ' x; ",
                    "printf '%*s' 16000 '' | tr ' ' t"
                )
            })
            .to_string(),
        }
    }

    fn database_url_with_name(base: &str, name: &str) -> String {
        let (prefix, query) = base
            .split_once('?')
            .map(|(prefix, query)| (prefix, format!("?{query}")))
            .unwrap_or((base, String::new()));
        let Some((root, _)) = prefix.rsplit_once('/') else {
            return format!("{base}_{name}");
        };
        format!("{root}/{name}{query}")
    }
}
