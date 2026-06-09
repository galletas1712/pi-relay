use std::path::PathBuf;

use agent_store::{InputPriority, SessionConfig};
use agent_vocab::{ProviderConfig, UserMessage};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::codec::from_params;
use crate::provider_runtime::{render_pi_prompt, resolve_skill_role};
use crate::runtime::SessionDriver;
use crate::session_start::{
    start_prepared_session, PreparedSessionDispatchMode, PreparedSessionStart, StartedSession,
};
use crate::state::AppState;
use crate::types::RpcError;

const MAX_INITIAL_CONTEXT_BYTES: usize = 64 * 1024;

pub(crate) async fn subagent_list(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: SubagentListParams = from_params(params)?;
    let parent_session_id = params.parent_session_id.trim().to_string();
    if parent_session_id.is_empty() {
        return Err(RpcError::new(
            "invalid_params",
            "parent_session_id cannot be empty",
        ));
    }
    let child_session_ids = state
        .repo
        .list_child_session_ids(&parent_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let mut subagents = Vec::with_capacity(child_session_ids.len());
    for child_session_id in child_session_ids {
        let activity = state
            .repo
            .activity(&child_session_id)
            .await
            .map_err(anyhow::Error::from)?;
        subagents.push(json!({
            "child_session_id": child_session_id,
            "activity": activity,
        }));
    }
    Ok(json!({
        "parent_session_id": parent_session_id,
        "subagents": subagents,
    }))
}

pub(crate) async fn subagent_spawn(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let request = SubagentSpawnRequest::from_params(params)?;
    let spawned = spawn_subagent(state, request, None).await?;
    Ok(json!({
        "parent_session_id": spawned.parent_session_id,
        "child_session_id": spawned.started.session_id,
        "activity": spawned.started.activity,
        "replayed": spawned.started.replayed,
    }))
}

#[derive(Debug, Deserialize)]
struct SubagentListParams {
    parent_session_id: String,
}

#[derive(Debug)]
struct SubagentSpawnRequest {
    parent_session_id: String,
    child_session_id: Option<String>,
    role: String,
    role_workspace: Option<String>,
    task: String,
    initial_context: Option<String>,
    display_name: Option<String>,
    provider: Option<ProviderConfig>,
    metadata: Value,
}

impl SubagentSpawnRequest {
    fn from_params(params: Value) -> std::result::Result<Self, RpcError> {
        let params: SubagentSpawnParams = from_params(params)?;
        let parent_session_id = params.parent_session_id.trim().to_string();
        if parent_session_id.is_empty() {
            return Err(RpcError::new(
                "invalid_params",
                "parent_session_id cannot be empty",
            ));
        }
        let role = params.role.trim().to_string();
        if role.is_empty() {
            return Err(RpcError::new("invalid_params", "role cannot be empty"));
        }
        let role_workspace = params
            .role_workspace
            .map(|workspace| workspace.trim().to_string())
            .filter(|workspace| !workspace.is_empty());
        let task = params.task.trim().to_string();
        if task.is_empty() {
            return Err(RpcError::new("invalid_params", "task cannot be empty"));
        }
        let initial_context = params
            .initial_context
            .map(|context| context.trim().to_string())
            .filter(|context| !context.is_empty());
        if initial_context
            .as_ref()
            .map(String::len)
            .unwrap_or_default()
            > MAX_INITIAL_CONTEXT_BYTES
        {
            return Err(RpcError::new(
                "initial_context_too_large",
                format!("initial_context exceeds {MAX_INITIAL_CONTEXT_BYTES} bytes"),
            ));
        }
        let child_session_id = params
            .child_session_id
            .map(|session_id| session_id.trim().to_string())
            .filter(|session_id| !session_id.is_empty());
        Ok(Self {
            parent_session_id,
            child_session_id,
            role,
            role_workspace,
            task,
            initial_context,
            display_name: params.display_name,
            provider: params.provider,
            metadata: params.metadata.unwrap_or_else(|| json!({})),
        })
    }
}

#[derive(Debug, Deserialize)]
struct SubagentSpawnParams {
    parent_session_id: String,
    child_session_id: Option<String>,
    role: String,
    role_workspace: Option<String>,
    task: String,
    initial_context: Option<String>,
    display_name: Option<String>,
    provider: Option<ProviderConfig>,
    metadata: Option<Value>,
}

struct SpawnedSubagent {
    parent_session_id: String,
    started: StartedSession,
}

async fn spawn_subagent(
    state: &AppState,
    request: SubagentSpawnRequest,
    excluded_parent_action_row_id: Option<&str>,
) -> std::result::Result<SpawnedSubagent, RpcError> {
    let parent_config = state
        .repo
        .load_session_config(&request.parent_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    if parent_config.project_id.is_none() {
        return Err(RpcError::new(
            "project_required",
            "subagents can only be spawned from project sessions",
        ));
    }
    let existing_child_session_id = match request.child_session_id.as_deref() {
        Some(child_session_id) => state
            .repo
            .session_exists(child_session_id)
            .await
            .map_err(anyhow::Error::from)?
            .then_some(child_session_id),
        None => None,
    };
    if let Some(child_session_id) = existing_child_session_id {
        require_known_subagent(state, &request.parent_session_id, child_session_id).await?;
        let activity = state
            .repo
            .activity(child_session_id)
            .await
            .map_err(anyhow::Error::from)?;
        return Ok(SpawnedSubagent {
            parent_session_id: request.parent_session_id,
            started: StartedSession {
                session_id: child_session_id.to_string(),
                project_id: parent_config.project_id,
                activity,
                replayed: true,
                dispatches: Vec::new(),
            },
        });
    }

    let parent_driver = SessionDriver::acquire(state, &request.parent_session_id).await;
    ensure_parent_ready_for_subagent_spawn(
        state,
        &parent_driver,
        &request.parent_session_id,
        excluded_parent_action_row_id,
    )
    .await?;

    let child_session_id = request
        .child_session_id
        .unwrap_or_else(|| format!("session_{}", Uuid::new_v4()));
    if state
        .repo
        .session_exists(&child_session_id)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Err(RpcError::new(
            "session_exists",
            format!("child session already exists and is not reusable: {child_session_id}"),
        ));
    }

    let role = resolve_skill_role(
        &PathBuf::from(&parent_config.outer_cwd),
        &parent_config.workspaces,
        &request.role,
        request.role_workspace.as_deref(),
    )
    .map_err(|error| RpcError::new("role_not_found", format!("{error:#}")))?;
    let (outer_cwd, workspaces) = state
        .workspaces
        .fork_session_from_parent(
            &request.parent_session_id,
            &parent_config.outer_cwd,
            &parent_config.workspaces,
            &child_session_id,
        )
        .await
        .map_err(anyhow::Error::from)?;

    let child_metadata = subagent_metadata(
        request.metadata,
        &role.name,
        role.workspace.as_deref(),
        request.display_name.as_deref(),
        &request.task,
        &role.file_path,
        &parent_config.metadata,
    );
    let mut child_config = SessionConfig {
        project_id: parent_config.project_id,
        outer_cwd: outer_cwd.clone(),
        workspaces,
        system_prompt: String::new(),
        provider: request.provider.unwrap_or(parent_config.provider),
        metadata: child_metadata,
    };
    child_config.system_prompt = child_system_prompt(
        state,
        &child_config,
        ChildPromptRole {
            name: &role.name,
            workspace: role.workspace.as_deref(),
            description: &role.description,
            content: &role.content,
            parent_session_id: &request.parent_session_id,
        },
    )?;

    let task = request.task;
    let initial_task = child_initial_task_message(
        &request.parent_session_id,
        &task,
        request.initial_context.as_deref(),
    );
    let started = start_prepared_session(
        state,
        PreparedSessionStart {
            session_id: child_session_id.clone(),
            config: child_config,
            priority: InputPriority::FollowUp,
            content: UserMessage::text(initial_task),
            client_input_id: None,
            parent_session_id: Some(request.parent_session_id.clone()),
            dispatch_mode: PreparedSessionDispatchMode::Deferred,
        },
    )
    .await?;
    require_known_subagent(state, &request.parent_session_id, &child_session_id).await?;

    let child_driver = SessionDriver::acquire(state, &started.session_id).await;
    if let Err(error) = child_driver.dispatch(started.dispatches.clone()).await {
        cleanup_failed_spawn(state, &started.session_id, "initial dispatch failure").await;
        return Err(error);
    }

    Ok(SpawnedSubagent {
        parent_session_id: request.parent_session_id,
        started,
    })
}

async fn ensure_parent_ready_for_subagent_spawn(
    state: &AppState,
    parent_driver: &SessionDriver,
    parent_session_id: &str,
    excluded_action_row_id: Option<&str>,
) -> std::result::Result<(), RpcError> {
    if let Some(excluded_action_row_id) = excluded_action_row_id {
        if state
            .repo
            .has_queued_inputs(parent_session_id)
            .await
            .map_err(anyhow::Error::from)?
            || state
                .repo
                .has_unfinished_actions_except(parent_session_id, excluded_action_row_id)
                .await
                .map_err(anyhow::Error::from)?
        {
            return Err(RpcError::new(
                "session_busy",
                "subagent spawn requires an exclusive filesystem point; retry after sibling tools finish",
            ));
        }
        return Ok(());
    }

    parent_driver.ensure_idle_for_source_mutation().await
}

pub(crate) async fn require_known_subagent(
    state: &AppState,
    parent_session_id: &str,
    child_session_id: &str,
) -> std::result::Result<(), RpcError> {
    let actual_parent_session_id = state
        .repo
        .session_parent_id(child_session_id)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| RpcError::new("subagent_not_found", "subagent is not in scope"))?;
    if actual_parent_session_id != parent_session_id {
        return Err(RpcError::new(
            "subagent_not_found",
            "subagent is not in scope",
        ));
    }
    Ok(())
}

async fn cleanup_failed_spawn(state: &AppState, child_session_id: &str, reason: &str) {
    state.active.lock().await.remove(child_session_id);
    if let Err(delete_error) = state.repo.delete_session(child_session_id).await {
        eprintln!(
            "failed to clean up child session {child_session_id} after {reason}: {delete_error:#}"
        );
    }
    if let Err(workspace_error) = state.workspaces.remove_session_dir(child_session_id).await {
        eprintln!(
            "failed to clean up child workspace {child_session_id} after {reason}: {workspace_error:#}"
        );
    }
    state
        .provider_connections
        .remove_session(child_session_id)
        .await;
}

fn subagent_metadata(
    metadata: Value,
    role_name: &str,
    role_workspace: Option<&str>,
    display_name: Option<&str>,
    task: &str,
    role_file_path: &PathBuf,
    parent_metadata: &Value,
) -> Value {
    let mut metadata = match metadata {
        Value::Object(map) => Value::Object(map),
        _ => json!({}),
    };
    let Value::Object(map) = &mut metadata else {
        unreachable!("metadata was forced to an object");
    };
    if parent_metadata
        .get("harness")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        map.entry("harness".to_string())
            .or_insert_with(|| json!(true));
    }
    map.insert("hidden".to_string(), json!(true));
    map.insert("subagent".to_string(), json!(true));
    map.insert("role_name".to_string(), json!(role_name));
    if let Some(role_workspace) = role_workspace {
        map.insert("role_workspace".to_string(), json!(role_workspace));
    }
    if let Some(display_name) = display_name {
        map.insert("display_name".to_string(), json!(display_name));
    }
    map.insert("task".to_string(), json!(task));
    map.insert("role_file_path".to_string(), json!(role_file_path));
    metadata
}

fn child_initial_task_message(
    parent_session_id: &str,
    task: &str,
    initial_context: Option<&str>,
) -> String {
    let mut message = format!("Subagent task from parent session `{parent_session_id}`:\n\n{task}");
    if let Some(initial_context) = initial_context {
        message.push_str("\n\n# Initial context\n\n");
        message.push_str(initial_context);
    }
    message
}

struct ChildPromptRole<'a> {
    name: &'a str,
    workspace: Option<&'a str>,
    description: &'a str,
    content: &'a str,
    parent_session_id: &'a str,
}

fn child_system_prompt(
    state: &AppState,
    config: &SessionConfig,
    role: ChildPromptRole<'_>,
) -> std::result::Result<String, RpcError> {
    let base = render_pi_prompt(state, config).map_err(anyhow::Error::from)?;
    let workspace = role
        .workspace
        .map(|workspace| format!("workspace `{workspace}`"))
        .unwrap_or_else(|| "global role".to_string());
    Ok(format!(
        "{base}\n\n# Subagent contract\n\n\
You are a child agent spawned by parent session `{}`.\n\
The parent can inspect your transcript, send follow-up messages, interrupt you, and decide whether to merge your filesystem changes.\n\
Keep your own context focused on the delegated task. Do not assume your changes are merged automatically.\n\
When you finish, report concise results and any follow-up work clearly.\n\n\
# Subagent role\n\n\
Role: `{}` ({workspace})\n\
Description: {}\n\n\
{}\n",
        role.parent_session_id,
        role.name,
        role.description.trim(),
        role.content.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_validation_trims_role_and_rejects_empty_task() {
        let request = SubagentSpawnRequest::from_params(json!({
            "parent_session_id": " parent ",
            "role": " reviewer ",
            "role_workspace": " repo ",
            "task": " Review this ",
            "initial_context": " Context ",
        }))
        .expect("request parses");
        assert_eq!(request.parent_session_id, "parent");
        assert_eq!(request.role, "reviewer");
        assert_eq!(request.role_workspace.as_deref(), Some("repo"));
        assert_eq!(request.task, "Review this");
        assert_eq!(request.initial_context.as_deref(), Some("Context"));

        let error = SubagentSpawnRequest::from_params(json!({
            "parent_session_id": "parent",
            "role": "reviewer",
            "task": "  ",
        }))
        .expect_err("empty task rejected");
        assert_eq!(error.code, "invalid_params");
    }

    #[test]
    fn subagent_metadata_marks_session_hidden() {
        let metadata = subagent_metadata(
            json!({ "custom": true }),
            "reviewer",
            Some("repo"),
            Some("Review"),
            "Review this",
            &PathBuf::from("/tmp/reviewer/SKILL.md"),
            &json!({ "harness": true }),
        );
        assert_eq!(
            metadata,
            json!({
                "custom": true,
                "harness": true,
                "hidden": true,
                "subagent": true,
                "role_name": "reviewer",
                "role_workspace": "repo",
                "display_name": "Review",
                "task": "Review this",
                "role_file_path": "/tmp/reviewer/SKILL.md",
            })
        );
    }
}
