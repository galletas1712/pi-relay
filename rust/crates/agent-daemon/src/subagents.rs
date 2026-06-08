use std::path::PathBuf;

use agent_store::{
    CreateSessionRelationship, InputPriority, SessionActivity, SessionConfig, SessionRelationship,
    SessionRelationshipControlMode, SessionRelationshipKind, SessionRelationshipStatus,
    SessionRelationshipVisibility,
};
use agent_vocab::{ProviderConfig, UserMessage};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::codec::from_params;
use crate::provider_runtime::{render_pi_prompt, resolve_skill_role};
use crate::runtime::SessionDriver;
use crate::session_start::{start_prepared_session, PreparedSessionStart, StartedSession};
use crate::state::AppState;
use crate::types::RpcError;

pub(crate) async fn subagent_spawn(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let request = SubagentSpawnRequest::from_params(params)?;
    let spawned = spawn_subagent(state, request).await?;
    Ok(json!({
        "parent_session_id": spawned.parent_session_id,
        "child_session_id": spawned.started.session_id,
        "relationship": relationship_view(&spawned.relationship),
        "activity": spawned.started.activity,
        "replayed": spawned.started.replayed,
    }))
}

#[derive(Debug)]
struct SubagentSpawnRequest {
    parent_session_id: String,
    child_session_id: Option<String>,
    role: String,
    role_workspace: Option<String>,
    task: String,
    display_name: Option<String>,
    provider: Option<ProviderConfig>,
    metadata: Value,
    workflow_id: Option<String>,
    result_variable: Option<String>,
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
            display_name: params.display_name,
            provider: params.provider,
            metadata: params.metadata.unwrap_or_else(|| json!({})),
            workflow_id: params.workflow_id,
            result_variable: params.result_variable,
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
    display_name: Option<String>,
    provider: Option<ProviderConfig>,
    metadata: Option<Value>,
    workflow_id: Option<String>,
    result_variable: Option<String>,
}

struct SpawnedSubagent {
    parent_session_id: String,
    started: StartedSession,
    relationship: SessionRelationship,
}

async fn spawn_subagent(
    state: &AppState,
    request: SubagentSpawnRequest,
) -> std::result::Result<SpawnedSubagent, RpcError> {
    let parent_driver = SessionDriver::acquire(state, &request.parent_session_id).await;
    parent_driver.ensure_idle_for_source_mutation().await?;
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
            format!("child session already exists: {child_session_id}"),
        ));
    }

    let role = resolve_skill_role(
        &PathBuf::from(&parent_config.outer_cwd),
        &parent_config.workspaces,
        &request.role,
        request.role_workspace.as_deref(),
    )
    .map_err(|error| RpcError::new("role_not_found", format!("{error:#}")))?;
    let parent_active_leaf_id = state
        .repo
        .active_leaf_id(&request.parent_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let forked = state
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
        &request.parent_session_id,
        &role.file_path,
    );
    let mut child_config = SessionConfig {
        project_id: parent_config.project_id,
        outer_cwd: forked.outer_cwd.clone(),
        workspaces: forked.workspaces,
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

    let relationship_id = format!("relationship_{}", Uuid::new_v4());
    let task = request.task;
    let started = start_prepared_session(
        state,
        PreparedSessionStart {
            session_id: child_session_id.clone(),
            config: child_config,
            priority: InputPriority::FollowUp,
            content: UserMessage::text(format!(
                "Subagent task from parent session `{}`:\n\n{task}",
                request.parent_session_id
            )),
            client_input_id: None,
        },
    )
    .await?;
    let relationship = state
        .repo
        .create_session_relationship(&CreateSessionRelationship {
            relationship_id,
            source_session_id: request.parent_session_id.clone(),
            target_session_id: child_session_id,
            root_session_id: relationship_root_session_id(state, &request.parent_session_id)
                .await?,
            kind: SessionRelationshipKind::Subagent,
            control_mode: SessionRelationshipControlMode::ParentControlled,
            visibility: SessionRelationshipVisibility::Hidden,
            role_name: Some(role.name),
            role_workspace: role.workspace,
            display_name: request.display_name,
            task,
            spawned_from_leaf_id: parent_active_leaf_id,
            spawned_from_action_row_id: None,
            workflow_id: request.workflow_id,
            result_variable: request.result_variable,
            status: relationship_status(started.activity),
            filesystem_mode: None,
            baseline_cwd: Some(forked.baseline_cwd),
            metadata: json!({
                "role_description": role.description,
                "role_file_path": role.file_path,
            }),
        })
        .await
        .map_err(anyhow::Error::from)?;

    Ok(SpawnedSubagent {
        parent_session_id: request.parent_session_id,
        started,
        relationship,
    })
}

fn subagent_metadata(metadata: Value, parent_session_id: &str, role_file_path: &PathBuf) -> Value {
    let mut metadata = match metadata {
        Value::Object(map) => Value::Object(map),
        _ => json!({}),
    };
    let Value::Object(map) = &mut metadata else {
        unreachable!("metadata was forced to an object");
    };
    map.insert("hidden".to_string(), json!(true));
    map.insert("subagent".to_string(), json!(true));
    map.insert("parent_session_id".to_string(), json!(parent_session_id));
    map.insert("role_file_path".to_string(), json!(role_file_path));
    metadata
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

async fn relationship_root_session_id(
    state: &AppState,
    parent_session_id: &str,
) -> std::result::Result<String, RpcError> {
    let relationship = state
        .repo
        .session_relationship_for_target(parent_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(relationship
        .map(|relationship| relationship.root_session_id)
        .unwrap_or_else(|| parent_session_id.to_string()))
}

fn relationship_status(activity: SessionActivity) -> SessionRelationshipStatus {
    match activity {
        SessionActivity::Idle => SessionRelationshipStatus::Idle,
        SessionActivity::Queued | SessionActivity::Running => SessionRelationshipStatus::Running,
    }
}

fn relationship_view(relationship: &SessionRelationship) -> Value {
    json!({
        "relationship_id": relationship.relationship_id,
        "source_session_id": relationship.source_session_id,
        "target_session_id": relationship.target_session_id,
        "root_session_id": relationship.root_session_id,
        "kind": relationship.kind,
        "control_mode": relationship.control_mode,
        "visibility": relationship.visibility,
        "role_name": relationship.role_name,
        "role_workspace": relationship.role_workspace,
        "display_name": relationship.display_name,
        "task": relationship.task,
        "spawned_from_leaf_id": relationship.spawned_from_leaf_id,
        "spawned_from_action_row_id": relationship.spawned_from_action_row_id,
        "workflow_id": relationship.workflow_id,
        "result_variable": relationship.result_variable,
        "status": relationship.status,
        "filesystem_mode": relationship.filesystem_mode,
        "baseline_cwd": relationship.baseline_cwd,
        "metadata": relationship.metadata,
        "created_at": relationship.created_at,
        "updated_at": relationship.updated_at,
    })
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
        }))
        .expect("request parses");
        assert_eq!(request.parent_session_id, "parent");
        assert_eq!(request.role, "reviewer");
        assert_eq!(request.role_workspace.as_deref(), Some("repo"));
        assert_eq!(request.task, "Review this");

        let error = SubagentSpawnRequest::from_params(json!({
            "parent_session_id": "parent",
            "role": "reviewer",
            "task": "  ",
        }))
        .expect_err("empty task rejected");
        assert_eq!(error.code, "invalid_params");
    }

    #[test]
    fn subagent_metadata_marks_session_hidden_and_parented() {
        let metadata = subagent_metadata(
            json!({ "custom": true }),
            "parent",
            &PathBuf::from("/tmp/reviewer/SKILL.md"),
        );
        assert_eq!(
            metadata,
            json!({
                "custom": true,
                "hidden": true,
                "subagent": true,
                "parent_session_id": "parent",
                "role_file_path": "/tmp/reviewer/SKILL.md",
            })
        );
    }

    #[test]
    fn relationship_status_follows_session_activity() {
        assert_eq!(
            relationship_status(SessionActivity::Idle),
            SessionRelationshipStatus::Idle
        );
        assert_eq!(
            relationship_status(SessionActivity::Queued),
            SessionRelationshipStatus::Running
        );
        assert_eq!(
            relationship_status(SessionActivity::Running),
            SessionRelationshipStatus::Running
        );
    }
}
