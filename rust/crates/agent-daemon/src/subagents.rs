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
        // Best-effort: surface the child's role so list() handles carry it.
        let role = state
            .repo
            .load_session_config(&child_session_id)
            .await
            .ok()
            .and_then(|config| {
                config
                    .metadata
                    .get("role_name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        subagents.push(json!({
            "child_session_id": child_session_id,
            "activity": activity,
            "role": role,
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
    let spawned = spawn_subagent(state, request, ParentSpawnState::MustBeIdle).await?;
    Ok(spawned_subagent_view(spawned))
}

pub(crate) async fn subagent_spawn_from_active_parent(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let request = SubagentSpawnRequest::from_params(params)?;
    let spawned = spawn_subagent(state, request, ParentSpawnState::MayBeActive).await?;
    Ok(spawned_subagent_view(spawned))
}

fn spawned_subagent_view(spawned: SpawnedSubagent) -> Value {
    json!({
        "parent_session_id": spawned.parent_session_id,
        "child_session_id": spawned.started.session_id,
        "activity": spawned.started.activity,
        "replayed": spawned.started.replayed,
    })
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
    sources: Vec<String>,
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
        let mut sources = Vec::new();
        for source in params.sources.unwrap_or_default() {
            let Some(source_session_id) = source_session_id(source) else {
                return Err(RpcError::new(
                    "invalid_params",
                    "each source must be a session id string or an object containing session_id or child_session_id",
                ));
            };
            sources.push(source_session_id);
        }
        Ok(Self {
            parent_session_id,
            child_session_id,
            role,
            role_workspace,
            task,
            initial_context,
            sources,
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
    sources: Option<Vec<Value>>,
    display_name: Option<String>,
    provider: Option<ProviderConfig>,
    metadata: Option<Value>,
}

struct SpawnedSubagent {
    parent_session_id: String,
    started: StartedSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentSpawnState {
    MustBeIdle,
    MayBeActive,
}

async fn spawn_subagent(
    state: &AppState,
    request: SubagentSpawnRequest,
    parent_spawn_state: ParentSpawnState,
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
    match parent_spawn_state {
        ParentSpawnState::MustBeIdle => parent_driver.ensure_idle_for_source_mutation().await?,
        ParentSpawnState::MayBeActive => parent_driver.recover_if_needed().await?,
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
            format!("child session already exists and is not reusable: {child_session_id}"),
        ));
    }

    let role = resolve_skill_role(
        &state.prompt_root,
        &PathBuf::from(&parent_config.outer_cwd),
        &parent_config.workspaces,
        &request.role,
        request.role_workspace.as_deref(),
    )
    .map_err(|error| RpcError::new("role_not_found", format!("{error:#}")))?;
    let source_configs = load_source_configs(state, &request.parent_session_id, &request.sources)
        .await?
        .into_iter()
        .collect::<Vec<_>>();
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
    let source_refs = match state
        .workspaces
        .import_source_refs(&outer_cwd, &child_config.workspaces, &source_configs)
        .await
    {
        Ok(source_refs) => source_refs,
        Err(error) => {
            let _ = state.workspaces.remove_session_dir(&child_session_id).await;
            return Err(anyhow::Error::from(error).into());
        }
    };

    let task = request.task;
    let initial_task = child_initial_task_message(
        &request.parent_session_id,
        &task,
        request.initial_context.as_deref(),
        &source_refs,
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

fn source_session_id(value: Value) -> Option<String> {
    match value {
        Value::String(session_id) => {
            let session_id = session_id.trim();
            (!session_id.is_empty()).then(|| session_id.to_string())
        }
        Value::Object(map) => map
            .get("session_id")
            .or_else(|| map.get("child_session_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

async fn load_source_configs(
    state: &AppState,
    parent_session_id: &str,
    source_session_ids: &[String],
) -> std::result::Result<Vec<(String, SessionConfig)>, RpcError> {
    let mut sources = Vec::with_capacity(source_session_ids.len());
    for source_session_id in source_session_ids {
        require_known_subagent(state, parent_session_id, source_session_id).await?;
        let source_driver = SessionDriver::acquire(state, source_session_id).await;
        source_driver.ensure_idle_for_source_mutation().await?;
        let config = state
            .repo
            .load_session_config(source_session_id)
            .await
            .map_err(anyhow::Error::from)?;
        sources.push((source_session_id.clone(), config));
    }
    Ok(sources)
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
    if parent_metadata
        .get("auto_title_disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        map.entry("auto_title_disabled".to_string())
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
    source_refs: &[crate::workspaces::SourceRefSpec],
) -> String {
    let mut message = format!(
        "# Delegated task\n\nParent session: `{parent_session_id}`\n\n{task}\n\n# Parent active context\n\n"
    );
    if let Some(initial_context) = initial_context {
        message.push_str(
            "The parent requested `fork_context=True`; the following is a bounded textual snapshot \
of the parent's active branch at delegation time.\n\n",
        );
        message.push_str(initial_context.trim());
    } else {
        message.push_str(
            "No parent transcript/context snapshot was included for this call (`fork_context=False`). \
Use the delegated task, role instructions, workspace/project context, and any files/tools you inspect.",
        );
    }
    if !source_refs.is_empty() {
        message.push_str("\n\n# Source child sessions\n\n");
        message.push_str(
            "The following child session outputs are available as local git refs in your workspace. \
Inspect or merge them with git commands as needed; do not assume they are already applied.\n",
        );
        let mut current_source = "";
        for source_ref in source_refs {
            if current_source != source_ref.source_id {
                current_source = &source_ref.source_id;
                message.push_str(&format!(
                    "\n## {}\n\n- Session: `{}`\n- Git refs:\n",
                    source_ref.source_id, source_ref.session_id
                ));
            }
            message.push_str(&format!(
                "  - workspace `{}`: `{}`\n",
                source_ref.workspace_dir, source_ref.git_ref
            ));
        }
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
Your role instructions are already included below; do not call `LoadSkill` for this same role unless you explicitly need to inspect another skill.\n\
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

        let request = SubagentSpawnRequest::from_params(json!({
            "parent_session_id": "parent",
            "role": "merger",
            "task": "Merge this",
            "sources": [
                " child-a ",
                { "session_id": " child-b " },
                { "child_session_id": " child-c " }
            ]
        }))
        .expect("request parses sources");
        assert_eq!(request.sources, ["child-a", "child-b", "child-c"]);

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
            &json!({ "harness": true, "auto_title_disabled": true }),
        );
        assert_eq!(
            metadata,
            json!({
                "custom": true,
                "harness": true,
                "auto_title_disabled": true,
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

    #[test]
    fn child_initial_task_message_marks_absent_parent_context() {
        let message = child_initial_task_message("parent", "Inspect the repo.", None, &[]);

        assert!(message.contains("# Delegated task"));
        assert!(message.contains("Parent session: `parent`"));
        assert!(message.contains("Inspect the repo."));
        assert!(message.contains(
            "No parent transcript/context snapshot was included for this call (`fork_context=False`)."
        ));
    }

    #[test]
    fn child_initial_task_message_labels_forked_parent_context() {
        let message = child_initial_task_message(
            "parent",
            "Continue the investigation.",
            Some("Parent session `parent` active context:\n\nAssistant:\nprior answer"),
            &[],
        );

        assert!(message.contains("# Delegated task"));
        assert!(message.contains("The parent requested `fork_context=True`"));
        assert!(message.contains("Parent session `parent` active context:"));
        assert!(message.contains("prior answer"));
    }

    #[test]
    fn child_initial_task_message_lists_source_refs_without_diff_payload() {
        let refs = vec![crate::workspaces::SourceRefSpec {
            source_id: "source-1-implementer-abc123".to_string(),
            session_id: "session_abc123".to_string(),
            workspace_dir: "repo".to_string(),
            git_ref: "refs/pi-relay/sources/source-1-implementer-abc123".to_string(),
            commit: "deadbeef".to_string(),
        }];
        let message = child_initial_task_message("parent", "Merge this.", None, &refs);

        assert!(message.contains("# Source child sessions"));
        assert!(message.contains("## source-1-implementer-abc123"));
        assert!(message.contains("- Session: `session_abc123`"));
        assert!(message
            .contains("- workspace `repo`: `refs/pi-relay/sources/source-1-implementer-abc123`"));
        assert!(!message.contains("deadbeef"));
        assert!(!message.contains("```diff"));
    }
}
