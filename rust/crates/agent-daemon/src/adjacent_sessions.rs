use agent_store::{
    CreateSessionRelationship, InputPriority, SessionActivity, SessionConfig, SessionRelationship,
    SessionRelationshipControlMode, SessionRelationshipFilesystemMode, SessionRelationshipKind,
    SessionRelationshipPatch, SessionRelationshipStatus, SessionRelationshipVisibility,
};
use agent_vocab::{ProviderConfig, UserMessage};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::codec::from_params;
use crate::provider_runtime::render_pi_prompt;
use crate::rpc_views;
use crate::runtime::SessionDriver;
use crate::session_start::{
    start_prepared_session, PreparedSessionDispatchMode, PreparedSessionStart, StartedSession,
};
use crate::state::AppState;
use crate::types::RpcError;
use crate::workspaces::{RequestedWorkspace, WorkspaceSelection};

const MAX_INITIAL_CONTEXT_BYTES: usize = 64 * 1024;

pub(crate) async fn adjacent_session_list(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: AdjacentSessionListParams = from_params(params)?;
    let source_session_id = validate_non_empty(params.source_session_id, "source_session_id")?;
    let relationships = state
        .repo
        .list_session_relationships_by_source(&source_session_id, None)
        .await
        .map_err(anyhow::Error::from)?;
    let relationships = relationships
        .into_iter()
        .filter(is_adjacent_relationship)
        .collect::<Vec<_>>();
    let mut adjacent_sessions = Vec::with_capacity(relationships.len());
    for relationship in relationships {
        let activity = state
            .repo
            .activity(&relationship.target_session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let relationship = refresh_relationship_status(state, relationship, activity).await?;
        adjacent_sessions.push(json!({
            "relationship": rpc_views::session_relationship(&relationship),
            "activity": activity,
        }));
    }
    Ok(json!({
        "source_session_id": source_session_id,
        "adjacent_sessions": adjacent_sessions,
    }))
}

pub(crate) async fn adjacent_session_spawn(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let request = AdjacentSessionSpawnRequest::from_params(params)?;
    let spawned = spawn_adjacent_session(state, request).await?;
    Ok(spawned_adjacent_session_view(spawned))
}

fn spawned_adjacent_session_view(spawned: SpawnedAdjacentSession) -> Value {
    json!({
        "source_session_id": spawned.source_session_id,
        "session_id": spawned.started.session_id,
        "relationship": rpc_views::session_relationship(&spawned.relationship),
        "activity": spawned.started.activity,
        "replayed": spawned.started.replayed,
    })
}

#[derive(Debug, Deserialize)]
struct AdjacentSessionListParams {
    source_session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdjacentWorkspaceMode {
    SameProjectFresh,
    ForkCurrent,
}

impl AdjacentWorkspaceMode {
    fn from_param(value: Option<String>) -> std::result::Result<Self, RpcError> {
        match value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("same_project_fresh")
        {
            "same_project_fresh" => Ok(Self::SameProjectFresh),
            "fork_current" => Ok(Self::ForkCurrent),
            other => Err(RpcError::new(
                "invalid_params",
                format!(
                    "workspace_mode must be `same_project_fresh` or `fork_current`, got `{other}`"
                ),
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::SameProjectFresh => "same_project_fresh",
            Self::ForkCurrent => "fork_current",
        }
    }

    fn relationship_kind(self) -> SessionRelationshipKind {
        match self {
            Self::SameProjectFresh => SessionRelationshipKind::Related,
            Self::ForkCurrent => SessionRelationshipKind::RelatedFork,
        }
    }
}

#[derive(Debug)]
struct AdjacentSessionSpawnRequest {
    source_session_id: String,
    session_id: Option<String>,
    task: String,
    initial_context: Option<String>,
    workspace_mode: AdjacentWorkspaceMode,
    display_name: Option<String>,
    provider: Option<ProviderConfig>,
    metadata: Value,
}

impl AdjacentSessionSpawnRequest {
    fn from_params(params: Value) -> std::result::Result<Self, RpcError> {
        let params: AdjacentSessionSpawnParams = from_params(params)?;
        let source_session_id = validate_non_empty(params.source_session_id, "source_session_id")?;
        let task = validate_non_empty(params.task, "task")?;
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
        let session_id = params
            .session_id
            .map(|session_id| session_id.trim().to_string())
            .filter(|session_id| !session_id.is_empty());
        Ok(Self {
            source_session_id,
            session_id,
            task,
            initial_context,
            workspace_mode: AdjacentWorkspaceMode::from_param(params.workspace_mode)?,
            display_name: params.display_name,
            provider: params.provider,
            metadata: params.metadata.unwrap_or_else(|| json!({})),
        })
    }
}

#[derive(Debug, Deserialize)]
struct AdjacentSessionSpawnParams {
    source_session_id: String,
    session_id: Option<String>,
    task: String,
    initial_context: Option<String>,
    workspace_mode: Option<String>,
    display_name: Option<String>,
    provider: Option<ProviderConfig>,
    metadata: Option<Value>,
}

struct SpawnedAdjacentSession {
    source_session_id: String,
    started: StartedSession,
    relationship: SessionRelationship,
}

async fn spawn_adjacent_session(
    state: &AppState,
    request: AdjacentSessionSpawnRequest,
) -> std::result::Result<SpawnedAdjacentSession, RpcError> {
    let source_config = state
        .repo
        .load_session_config(&request.source_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let Some(project_id) = source_config.project_id else {
        return Err(RpcError::new(
            "project_required",
            "adjacent sessions can only be spawned from project sessions",
        ));
    };
    let existing_session_id = match request.session_id.as_deref() {
        Some(session_id) => state
            .repo
            .session_exists(session_id)
            .await
            .map_err(anyhow::Error::from)?
            .then_some(session_id),
        None => None,
    };
    if let Some(session_id) = existing_session_id {
        let relationship =
            require_adjacent_relationship(state, &request.source_session_id, session_id).await?;
        let activity = state
            .repo
            .activity(session_id)
            .await
            .map_err(anyhow::Error::from)?;
        let relationship = refresh_relationship_status(state, relationship, activity).await?;
        return Ok(SpawnedAdjacentSession {
            source_session_id: request.source_session_id,
            started: StartedSession {
                session_id: session_id.to_string(),
                project_id: Some(project_id),
                activity,
                replayed: true,
                dispatches: Vec::new(),
            },
            relationship,
        });
    }

    let source_driver = SessionDriver::acquire(state, &request.source_session_id).await;
    if request.workspace_mode == AdjacentWorkspaceMode::ForkCurrent {
        ensure_source_ready_for_fork_current(&source_driver).await?;
    }

    let session_id = request
        .session_id
        .unwrap_or_else(|| format!("session_{}", Uuid::new_v4()));
    if state
        .repo
        .session_exists(&session_id)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Err(RpcError::new(
            "session_exists",
            format!("session already exists and is not reusable: {session_id}"),
        ));
    }

    let source_active_leaf_id = state
        .repo
        .active_leaf_id(&request.source_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let materialized = materialize_adjacent_workspaces(
        state,
        &request.source_session_id,
        &source_config,
        project_id,
        &session_id,
        request.workspace_mode,
    )
    .await?;
    let child_metadata = adjacent_session_metadata(
        request.metadata,
        &request.source_session_id,
        request.workspace_mode,
        &source_config.metadata,
    );
    let mut child_config = SessionConfig {
        project_id: Some(project_id),
        outer_cwd: materialized.outer_cwd,
        workspaces: materialized.workspaces,
        system_prompt: String::new(),
        provider: request.provider.unwrap_or(source_config.provider),
        metadata: child_metadata,
    };
    child_config.system_prompt = adjacent_session_system_prompt(
        state,
        &child_config,
        &request.source_session_id,
        request.workspace_mode,
    )?;

    let relationship_id = format!("relationship_{}", Uuid::new_v4());
    let root_session_id = relationship_root_session_id(state, &request.source_session_id).await?;
    let task = request.task;
    let initial_task = adjacent_initial_task_message(
        &request.source_session_id,
        &task,
        request.initial_context.as_deref(),
    );
    let started = start_prepared_session(
        state,
        PreparedSessionStart {
            session_id: session_id.clone(),
            config: child_config,
            priority: InputPriority::FollowUp,
            content: UserMessage::text(initial_task),
            client_input_id: None,
            dispatch_mode: PreparedSessionDispatchMode::Deferred,
        },
    )
    .await?;
    let relationship = match state
        .repo
        .create_session_relationship(&CreateSessionRelationship {
            relationship_id,
            source_session_id: request.source_session_id.clone(),
            target_session_id: session_id.clone(),
            root_session_id,
            kind: request.workspace_mode.relationship_kind(),
            control_mode: SessionRelationshipControlMode::NoControl,
            visibility: SessionRelationshipVisibility::TopLevel,
            role_name: None,
            role_workspace: None,
            display_name: request.display_name,
            task,
            spawned_from_leaf_id: source_active_leaf_id,
            spawned_from_action_row_id: None,
            workflow_id: None,
            result_variable: None,
            status: relationship_status(started.activity),
            filesystem_mode: materialized.filesystem_mode,
            metadata: json!({
                "workspace_mode": request.workspace_mode.as_str(),
            }),
        })
        .await
    {
        Ok(relationship) => relationship,
        Err(error) => {
            cleanup_failed_spawn(state, &session_id, "relationship insert failure").await;
            return Err(anyhow::Error::from(error).into());
        }
    };

    let child_driver = SessionDriver::acquire(state, &started.session_id).await;
    if let Err(error) = child_driver.dispatch(started.dispatches.clone()).await {
        cleanup_failed_spawn(state, &started.session_id, "initial dispatch failure").await;
        return Err(error);
    }

    Ok(SpawnedAdjacentSession {
        source_session_id: request.source_session_id,
        started,
        relationship,
    })
}

struct MaterializedAdjacentWorkspaces {
    outer_cwd: String,
    workspaces: Vec<agent_store::SessionWorkspace>,
    filesystem_mode: Option<SessionRelationshipFilesystemMode>,
}

async fn materialize_adjacent_workspaces(
    state: &AppState,
    source_session_id: &str,
    source_config: &SessionConfig,
    project_id: Uuid,
    session_id: &str,
    workspace_mode: AdjacentWorkspaceMode,
) -> std::result::Result<MaterializedAdjacentWorkspaces, RpcError> {
    match workspace_mode {
        AdjacentWorkspaceMode::SameProjectFresh => {
            let project = state
                .repo
                .get_project(project_id)
                .await
                .map_err(anyhow::Error::from)?;
            let requested_workspaces = source_config
                .workspaces
                .iter()
                .map(|workspace| RequestedWorkspace {
                    workspace_dir: workspace.workspace_dir.clone(),
                    branch: workspace.remote_branch.clone(),
                })
                .collect::<Vec<_>>();
            let selected = WorkspaceSelection::Subset(requested_workspaces)
                .resolve(&project.workspaces)
                .map_err(|error| RpcError::new("invalid_params", error.to_string()))?;
            let (outer_cwd, workspaces) = state
                .workspaces
                .materialize_session(project_id, session_id, &project.workspaces, &selected)
                .await
                .map_err(anyhow::Error::from)?;
            Ok(MaterializedAdjacentWorkspaces {
                outer_cwd,
                workspaces,
                filesystem_mode: None,
            })
        }
        AdjacentWorkspaceMode::ForkCurrent => {
            let (outer_cwd, workspaces) = state
                .workspaces
                .fork_session_from_parent(
                    source_session_id,
                    &source_config.outer_cwd,
                    &source_config.workspaces,
                    session_id,
                )
                .await
                .map_err(anyhow::Error::from)?;
            Ok(MaterializedAdjacentWorkspaces {
                outer_cwd,
                workspaces,
                filesystem_mode: None,
            })
        }
    }
}

async fn ensure_source_ready_for_fork_current(
    source_driver: &SessionDriver,
) -> std::result::Result<(), RpcError> {
    source_driver.ensure_idle_for_source_mutation().await
}
async fn require_adjacent_relationship(
    state: &AppState,
    source_session_id: &str,
    session_id: &str,
) -> std::result::Result<SessionRelationship, RpcError> {
    let relationship = state
        .repo
        .session_relationship_for_target(session_id)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| {
            RpcError::new(
                "adjacent_session_not_found",
                "adjacent session relationship not found",
            )
        })?;
    if relationship.source_session_id != source_session_id
        || relationship.control_mode != SessionRelationshipControlMode::NoControl
        || !is_adjacent_relationship(&relationship)
    {
        return Err(RpcError::new(
            "adjacent_session_not_found",
            "adjacent session relationship not found",
        ));
    }
    Ok(relationship)
}

fn is_adjacent_relationship(relationship: &SessionRelationship) -> bool {
    matches!(
        relationship.kind,
        SessionRelationshipKind::Related | SessionRelationshipKind::RelatedFork
    )
}

async fn refresh_relationship_status(
    state: &AppState,
    relationship: SessionRelationship,
    activity: SessionActivity,
) -> std::result::Result<SessionRelationship, RpcError> {
    let status = relationship_status(activity);
    if relationship.status == SessionRelationshipStatus::Completed || relationship.status == status
    {
        return Ok(relationship);
    }
    state
        .repo
        .update_session_relationship(
            &relationship.relationship_id,
            SessionRelationshipPatch {
                status: Some(status),
                display_name: None,
                metadata: None,
            },
        )
        .await
        .map_err(anyhow::Error::from)
        .map_err(Into::into)
}

async fn cleanup_failed_spawn(state: &AppState, session_id: &str, reason: &str) {
    state.active.lock().await.remove(session_id);
    if let Err(delete_error) = state.repo.delete_session(session_id).await {
        eprintln!(
            "failed to clean up adjacent session {session_id} after {reason}: {delete_error:#}"
        );
    }
    if let Err(workspace_error) = state.workspaces.remove_session_dir(session_id).await {
        eprintln!(
            "failed to clean up adjacent workspace {session_id} after {reason}: {workspace_error:#}"
        );
    }
    state.provider_connections.remove_session(session_id).await;
}

fn adjacent_session_metadata(
    metadata: Value,
    source_session_id: &str,
    workspace_mode: AdjacentWorkspaceMode,
    source_metadata: &Value,
) -> Value {
    let mut metadata = match metadata {
        Value::Object(map) => Value::Object(map),
        _ => json!({}),
    };
    let Value::Object(map) = &mut metadata else {
        unreachable!("metadata was forced to an object");
    };
    if source_metadata
        .get("harness")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        map.entry("harness".to_string())
            .or_insert_with(|| json!(true));
    }
    map.insert("adjacent_session".to_string(), json!(true));
    map.insert("source_session_id".to_string(), json!(source_session_id));
    map.insert("workspace_mode".to_string(), json!(workspace_mode.as_str()));
    metadata
}

fn adjacent_initial_task_message(
    source_session_id: &str,
    task: &str,
    initial_context: Option<&str>,
) -> String {
    let mut message =
        format!("Adjacent session task spawned from session `{source_session_id}`:\n\n{task}");
    message.push_str(
        "\n\nThis is a visible top-level adjacent session, not a hidden parent-controlled subagent. \
Work independently on this related objective and report results clearly in this session.",
    );
    if let Some(initial_context) = initial_context {
        message.push_str("\n\n# Initial context\n\n");
        message.push_str(initial_context);
    }
    message
}

fn adjacent_session_system_prompt(
    state: &AppState,
    config: &SessionConfig,
    source_session_id: &str,
    workspace_mode: AdjacentWorkspaceMode,
) -> std::result::Result<String, RpcError> {
    let base = render_pi_prompt(state, config).map_err(anyhow::Error::from)?;
    Ok(format!(
        "{base}\n\n# Adjacent session contract\n\n\
You are a visible top-level adjacent session spawned from source session `{source_session_id}`.\n\
This task is related to the source session's project but is not a parent-controlled subtask. \
Do not assume the source session can inspect, steer, or merge your work like a subagent.\n\
Your workspace mode is `{}`. Keep your context focused on this adjacent objective and report concise results clearly.\n",
        workspace_mode.as_str()
    ))
}

async fn relationship_root_session_id(
    state: &AppState,
    source_session_id: &str,
) -> std::result::Result<String, RpcError> {
    let relationship = state
        .repo
        .session_relationship_for_target(source_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(relationship
        .map(|relationship| relationship.root_session_id)
        .unwrap_or_else(|| source_session_id.to_string()))
}

fn relationship_status(activity: SessionActivity) -> SessionRelationshipStatus {
    match activity {
        SessionActivity::Idle => SessionRelationshipStatus::Idle,
        SessionActivity::Queued | SessionActivity::Running => SessionRelationshipStatus::Running,
    }
}

fn validate_non_empty(value: String, field: &str) -> std::result::Result<String, RpcError> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(RpcError::new(
            "invalid_params",
            format!("{field} cannot be empty"),
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_request_defaults_to_same_project_fresh_and_trims_context() {
        let request = AdjacentSessionSpawnRequest::from_params(json!({
            "source_session_id": " source ",
            "session_id": " child ",
            "task": " Train a CIFAR VAE ",
            "initial_context": " context ",
        }))
        .expect("request parses");

        assert_eq!(request.source_session_id, "source");
        assert_eq!(request.session_id.as_deref(), Some("child"));
        assert_eq!(request.task, "Train a CIFAR VAE");
        assert_eq!(request.initial_context.as_deref(), Some("context"));
        assert_eq!(
            request.workspace_mode,
            AdjacentWorkspaceMode::SameProjectFresh
        );
    }

    #[test]
    fn spawn_request_accepts_fork_current() {
        let request = AdjacentSessionSpawnRequest::from_params(json!({
            "source_session_id": "source",
            "task": "Use dirty training utilities from the source session",
            "workspace_mode": "fork_current",
        }))
        .expect("request parses");

        assert_eq!(request.workspace_mode, AdjacentWorkspaceMode::ForkCurrent);
    }

    #[test]
    fn adjacent_metadata_is_visible_and_inherits_harness() {
        let metadata = adjacent_session_metadata(
            json!({ "custom": true }),
            "source",
            AdjacentWorkspaceMode::ForkCurrent,
            &json!({ "harness": true }),
        );

        assert_eq!(
            metadata,
            json!({
                "custom": true,
                "harness": true,
                "adjacent_session": true,
                "source_session_id": "source",
                "workspace_mode": "fork_current",
            })
        );
        assert!(metadata.get("hidden").is_none());
    }
}
