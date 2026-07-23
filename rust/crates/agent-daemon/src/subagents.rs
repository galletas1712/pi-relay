use std::path::PathBuf;

use agent_store::{EventFrame, EventType, InputPriority, SessionConfig, SubagentType};
use agent_vocab::{ProviderConfig, UserMessage};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::config::stable_default_provider;
use crate::provider_runtime::{model_available_for_config, render_pi_prompt, resolve_skill_role};
use crate::runtime::{publish_events, SessionDriver};
use crate::session_start::{
    start_prepared_session, PreparedSessionDispatchMode, PreparedSessionStart, StartedSession,
};
use crate::state::AppState;
use crate::types::RpcError;

/// A subagent spawned as part of a delegation: fresh context (no
/// parent-transcript fork, no source refs), tagged with its delegation id and
/// type.
pub(crate) struct DelegationSubagentSpawn {
    pub(crate) parent_session_id: String,
    pub(crate) role: String,
    pub(crate) task: String,
    pub(crate) subagent_type: SubagentType,
    pub(crate) delegation_id: String,
}

fn select_subagent_provider(
    explicit: Option<ProviderConfig>,
    role_provider: Option<ProviderConfig>,
    parent: ProviderConfig,
) -> ProviderConfig {
    explicit.or(role_provider).unwrap_or(parent)
}

impl From<DelegationSubagentSpawn> for SubagentSpawnRequest {
    fn from(spawn: DelegationSubagentSpawn) -> Self {
        Self {
            parent_session_id: spawn.parent_session_id,
            role: spawn.role,
            task: spawn.task,
            provider: None,
            metadata: json!({}),
            subagent_type: spawn.subagent_type,
            delegation_id: Some(spawn.delegation_id),
        }
    }
}

#[derive(Debug)]
pub(crate) struct SubagentSpawnRequest {
    parent_session_id: String,
    role: String,
    task: String,
    provider: Option<ProviderConfig>,
    metadata: Value,
    subagent_type: SubagentType,
    delegation_id: Option<String>,
}

pub(crate) struct SpawnedSubagent {
    pub(crate) started: StartedSession,
}

pub(crate) async fn spawn_subagent(
    state: &AppState,
    request: impl Into<SubagentSpawnRequest>,
) -> std::result::Result<SpawnedSubagent, RpcError> {
    let request = request.into();
    let parent_driver = SessionDriver::acquire(state, &request.parent_session_id).await;
    parent_driver.recover_if_needed().await?;
    let parent_config = state
        .repo
        .load_session_config(&request.parent_session_id)
        .await?;
    if parent_config.project_id.is_none() {
        return Err(RpcError::new(
            "project_required",
            "subagents can only be spawned from project sessions",
        ));
    }

    let child_session_id = format!("session_{}", Uuid::new_v4());
    let parent_workspace_dirs = parent_config
        .workspaces
        .iter()
        .map(|workspace| workspace.workspace_dir.clone())
        .collect::<Vec<_>>();
    let runtime_context = state
        .runtime_hosts
        .read_runtime_context(
            &parent_config.runtime_id,
            &parent_config.workspace_id,
            &parent_workspace_dirs,
        )
        .await
        .map_err(|error| RpcError::new("role_not_found", format!("{error:#}")))?;
    let role = resolve_skill_role(&runtime_context.skills, &request.role)
        .map_err(|error| RpcError::new("role_not_found", format!("{error:#}")))?;
    let resolved_role_name = role.name.clone();

    // A full subagent is the durable workspace's single writer for its
    // delegation: it runs against the parent's dirs in place (no fork). A
    // read-only subagent forks the parent into its own disposable snapshot.
    let (workspace_id, workspaces) = match request.subagent_type {
        SubagentType::Full => (
            parent_config.workspace_id.clone(),
            parent_config.workspaces.clone(),
        ),
        SubagentType::ReadOnly => {
            let child_workspace_id = format!("workspace_{}", Uuid::new_v4());
            state
                .runtime_hosts
                .fork_session_from_parent(
                    &request.parent_session_id,
                    &parent_config.workspace_id,
                    &parent_config.workspaces,
                    &child_workspace_id,
                )
                .await?
        }
    };
    let child_metadata = subagent_metadata(
        request.metadata,
        &resolved_role_name,
        &request.task,
        &role.file_path,
        &parent_config.metadata,
        request.subagent_type,
    );
    let selected_role_provider = request.provider.is_none() && role.provider.is_some();
    let mut child_config = SessionConfig {
        project_id: parent_config.project_id,
        runtime_id: parent_config.runtime_id.clone(),
        workspace_id: workspace_id.clone(),
        workspaces,
        system_prompt: String::new(),
        provider: select_subagent_provider(
            request.provider,
            role.provider.clone(),
            parent_config.provider,
        ),
        metadata: child_metadata,
        mcp_manifest: parent_config.mcp_manifest.clone(),
    };
    if selected_role_provider {
        let availability =
            model_available_for_config(state, &child_config, &child_session_id).await;
        if !matches!(availability, Ok(true)) {
            match availability {
                Ok(false) => eprintln!(
                    "subagent role model {}/{} is unavailable; falling back to OpenAI gpt-5.6-sol/high",
                    child_config.provider.kind, child_config.provider.model
                ),
                Err(error) => eprintln!(
                    "could not verify subagent role model {}/{}: {error:#}; falling back to OpenAI gpt-5.6-sol/high",
                    child_config.provider.kind, child_config.provider.model
                ),
                Ok(true) => unreachable!(),
            }
            state
                .provider_connections
                .remove_session(&child_session_id)
                .await;
            child_config.provider = stable_default_provider();
        }
    }
    child_config.system_prompt = child_system_prompt(
        state,
        &child_config,
        ChildPromptRole {
            name: &resolved_role_name,
            description: &role.description,
            file_path: &role.file_path,
            content: &role.content,
            skills: &role.skills,
            parent_session_id: &request.parent_session_id,
            subagent_type: request.subagent_type,
        },
    )
    .await?;
    let task = request.task;
    let initial_task = child_initial_task_message(&request.parent_session_id, &task);
    let subagent_type = request.subagent_type;
    let started = start_prepared_session(
        state,
        PreparedSessionStart {
            session_id: child_session_id.clone(),
            config: child_config,
            priority: InputPriority::FollowUp,
            content: UserMessage::text(initial_task),
            client_input_id: None,
            parent_session_id: Some(request.parent_session_id.clone()),
            subagent_type: Some(subagent_type),
            delegation_id: request.delegation_id.clone(),
            dispatch_mode: PreparedSessionDispatchMode::Deferred,
        },
    )
    .await?;
    require_known_subagent(state, &request.parent_session_id, &child_session_id).await?;

    let parent_events = match subagent_parent_spawn_events(
        state,
        &request.parent_session_id,
        &started.session_id,
        &resolved_role_name,
    )
    .await
    {
        Ok(parent_events) => parent_events,
        Err(error) => {
            cleanup_failed_spawn(
                state,
                &started.session_id,
                subagent_type,
                "parent lifecycle event failure",
            )
            .await;
            return Err(error);
        }
    };
    publish_events(state, parent_events);

    let child_driver = SessionDriver::acquire(state, &started.session_id).await;
    if let Err(error) = child_driver.dispatch(started.dispatches.clone()).await {
        publish_subagent_parent_dispatch_failed_event(
            state,
            &request.parent_session_id,
            &started.session_id,
            &resolved_role_name,
            &error,
        )
        .await;
        cleanup_failed_spawn(
            state,
            &started.session_id,
            subagent_type,
            "initial dispatch failure",
        )
        .await;
        return Err(error);
    }

    Ok(SpawnedSubagent { started })
}

async fn subagent_parent_spawn_events(
    state: &AppState,
    parent_session_id: &str,
    child_session_id: &str,
    role: &str,
) -> std::result::Result<Vec<EventFrame>, RpcError> {
    state
        .repo
        .insert_events(
            parent_session_id,
            vec![
                (
                    EventType::SubagentSpawned,
                    json!({
                        "child_session_id": child_session_id,
                        "role": role,
                    }),
                ),
                (
                    EventType::SubagentRunning,
                    json!({
                        "child_session_id": child_session_id,
                        "role": role,
                    }),
                ),
            ],
        )
        .await
        .map_err(RpcError::from)
}

pub(crate) async fn subagent_lifecycle_payload(
    state: &AppState,
    child_session_id: &str,
) -> std::result::Result<Value, RpcError> {
    let config = state.repo.load_session_config(child_session_id).await?;
    Ok(json!({
        "child_session_id": child_session_id,
        "role": config.metadata.get("role_name").and_then(Value::as_str),
    }))
}

pub(crate) async fn publish_subagent_parent_running_if_child(
    state: &AppState,
    child_session_id: &str,
) {
    let parent_session_id = match state.repo.session_parent_id(child_session_id).await {
        Ok(Some(parent_session_id)) => parent_session_id,
        Ok(None) => return,
        Err(error) => {
            eprintln!(
                "failed to load parent for subagent running event child={child_session_id}: {error:#}"
            );
            return;
        }
    };
    let payload = match subagent_lifecycle_payload(state, child_session_id).await {
        Ok(payload) => payload,
        Err(error) => {
            eprintln!(
                "failed to build subagent running event child={child_session_id}: {}: {}",
                error.code, error.message
            );
            return;
        }
    };
    match state
        .repo
        .insert_event(&parent_session_id, EventType::SubagentRunning, payload)
        .await
    {
        Ok(event) => publish_events(state, vec![event]),
        Err(error) => eprintln!(
            "failed to publish parent subagent running event parent={parent_session_id} child={child_session_id}: {error:#}"
        ),
    }
}

async fn publish_subagent_parent_dispatch_failed_event(
    state: &AppState,
    parent_session_id: &str,
    child_session_id: &str,
    role: &str,
    error: &RpcError,
) {
    // A delegation member's failure is owned by the delegation:
    // delegation_tools spawn-failure compensation fails the delegation and the
    // tool returns Err synchronously.
    // Suppress the per-child idle so the parent never sees a per-child
    // notification for a delegation member (matching the live idle gate).
    match state.repo.session_delegation_id(child_session_id).await {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(delegation_error) => eprintln!(
            "failed to load delegation id for dispatch-failed child={child_session_id}: {delegation_error:#}"
        ),
    }
    let summary_preview = format!("initial dispatch failed: {}: {}", error.code, error.message);
    match state
        .repo
        .insert_subagent_idle_event_once(
            parent_session_id,
            child_session_id,
            "initial-dispatch-failed",
            json!({
                "child_session_id": child_session_id,
                "role": role,
                "outcome": "Crashed",
                "summary_preview": summary_preview,
            }),
        )
        .await
    {
        Ok(Some(event)) => publish_events(state, vec![event]),
        Ok(None) => {}
        Err(event_error) => eprintln!(
            "failed to publish parent subagent dispatch-failed event parent={parent_session_id} child={child_session_id}: {event_error:#}"
        ),
    }
}

#[cfg(test)]
pub(crate) async fn publish_subagent_parent_dispatch_failed_event_for_test(
    state: &AppState,
    parent_session_id: &str,
    child_session_id: &str,
    role: &str,
) {
    publish_subagent_parent_dispatch_failed_event(
        state,
        parent_session_id,
        child_session_id,
        role,
        &RpcError::new("provider_error", "simulated initial dispatch failure"),
    )
    .await;
}

pub(crate) async fn require_known_subagent(
    state: &AppState,
    parent_session_id: &str,
    child_session_id: &str,
) -> std::result::Result<(), RpcError> {
    let actual_parent_session_id = state
        .repo
        .session_parent_id(child_session_id)
        .await?
        .ok_or_else(|| RpcError::new("subagent_not_found", "subagent is not in scope"))?;
    if actual_parent_session_id != parent_session_id {
        return Err(RpcError::new(
            "subagent_not_found",
            "subagent is not in scope",
        ));
    }
    Ok(())
}

async fn cleanup_failed_spawn(
    state: &AppState,
    child_session_id: &str,
    subagent_type: SubagentType,
    reason: &str,
) {
    state.active.lock().await.remove(child_session_id);
    if let Err(delete_error) = state.repo.delete_session(child_session_id).await {
        eprintln!(
            "failed to clean up child session {child_session_id} after {reason}: {delete_error:#}"
        );
    }
    // A full subagent shares the parent's session root/cwd in place; its
    // session dir was never created, so tearing it down would delete the
    // parent's durable workspace. Only a forked read-only child owns a private
    // dir that is safe to reclaim.
    match subagent_type {
        SubagentType::Full => {}
        SubagentType::ReadOnly => {
            if let Err(workspace_error) = state
                .runtime_hosts
                .destroy_session_workspaces(child_session_id)
                .await
            {
                eprintln!(
                    "failed to clean up child workspace {child_session_id} after {reason}: {workspace_error:#}"
                );
            }
        }
    }
    state
        .provider_connections
        .remove_session(child_session_id)
        .await;
}

fn subagent_metadata(
    metadata: Value,
    role_name: &str,
    task: &str,
    role_file_path: &PathBuf,
    parent_metadata: &Value,
    subagent_type: SubagentType,
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
    map.insert("prompt_profile".to_string(), json!("subagent"));
    map.insert("subagent_type".to_string(), json!(subagent_type.as_str()));
    map.insert("role_name".to_string(), json!(role_name));
    map.insert("task".to_string(), json!(task));
    map.insert("role_file_path".to_string(), json!(role_file_path));
    metadata
}

fn child_initial_task_message(parent_session_id: &str, task: &str) -> String {
    format!(
        "# Delegated task\n\nParent session: `{parent_session_id}`\n\n{task}\n\n# Parent active context\n\n\
A subagent runs with fresh context: no parent transcript snapshot is included. \
Use the delegated task, role instructions, workspace/project context, and any files/tools you inspect."
    )
}

fn subagent_workspace_semantics(subagent_type: SubagentType) -> &'static str {
    match subagent_type {
        SubagentType::Full => {
            "You are a full subagent. Your filesystem edits are made in the parent workspace in place and affect what the parent will see."
        }
        SubagentType::ReadOnly => {
            "You are a read-only subagent. Writes under your session cwd stay in a disposable snapshot and do not reach the parent. Absolute runtime-host paths are shared and must be treated as read-only."
        }
    }
}

fn subagent_contract_text(parent_session_id: &str, subagent_type: SubagentType) -> String {
    let workspace_semantics = subagent_workspace_semantics(subagent_type);
    format!(
        "# Subagent contract\n\n\
You are a child agent spawned by parent session `{parent_session_id}`.\n\
The parent can inspect your transcript, send follow-up messages, interrupt you, and decide whether to merge your filesystem changes.\n\
Keep your own context focused on the delegated task. Do not assume your changes are merged automatically.\n\
You cannot spawn nested delegations. Do not call `delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`, `cancel_delegation`, or `steer_subagent`; those parent orchestration tools are unavailable to subagents.\n\
Answer only the delegated task. Your final message/report is the durable handoff to the parent, so include the evidence, changed files, commands, risks, and follow-up work the parent needs.\n\
{workspace_semantics}"
    )
}

struct ChildPromptRole<'a> {
    name: &'a str,
    description: &'a str,
    file_path: &'a PathBuf,
    content: &'a str,
    skills: &'a [crate::provider_runtime::ResolvedPreloadedSkill],
    parent_session_id: &'a str,
    subagent_type: SubagentType,
}

async fn child_system_prompt(
    state: &AppState,
    config: &SessionConfig,
    role: ChildPromptRole<'_>,
) -> std::result::Result<String, RpcError> {
    let base = render_pi_prompt(state, config).await?;
    let contract = subagent_contract_text(role.parent_session_id, role.subagent_type);
    let preloaded = role
        .skills
        .iter()
        .map(|skill| {
            format!(
                "# Preloaded skill: {}\n\nSKILL.md: `{}`\n\n{}",
                skill.name,
                skill.file_path.display(),
                skill.content.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(format!(
        "{base}\n\n{contract}\n\n\
# Subagent role\n\n\
Role: `{}`\n\
Description: {}\n\n\
SKILL.md: `{}`\n\n\
{}\n\n\
{}\n",
        role.name,
        role.description.trim(),
        role.file_path.display(),
        role.content.trim(),
        preloaded
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_metadata_marks_session_hidden() {
        let metadata = subagent_metadata(
            json!({ "custom": true }),
            "reviewer",
            "Review this",
            &PathBuf::from("/tmp/reviewer/SKILL.md"),
            &json!({ "harness": true, "auto_title_disabled": true }),
            SubagentType::ReadOnly,
        );
        assert_eq!(
            metadata,
            json!({
                "custom": true,
                "harness": true,
                "auto_title_disabled": true,
                "hidden": true,
                "subagent": true,
                "prompt_profile": "subagent",
                "subagent_type": "read_only",
                "role_name": "reviewer",
                "task": "Review this",
                "role_file_path": "/tmp/reviewer/SKILL.md",
            })
        );
    }

    #[test]
    fn child_initial_task_message_marks_fresh_context() {
        let message = child_initial_task_message("parent", "Inspect the repo.");

        assert!(message.contains("# Delegated task"));
        assert!(message.contains("Parent session: `parent`"));
        assert!(message.contains("Inspect the repo."));
        assert!(message.contains("A subagent runs with fresh context"));
    }

    #[test]
    fn subagent_workspace_semantics_distinguish_full_and_read_only() {
        let full = subagent_workspace_semantics(SubagentType::Full);
        assert!(full.contains("full subagent"));
        assert!(full.contains("parent workspace in place"));
        assert!(full.contains("affect what the parent will see"));

        let read_only = subagent_workspace_semantics(SubagentType::ReadOnly);
        assert!(read_only.contains("read-only subagent"));
        assert!(read_only.contains("disposable snapshot"));
        assert!(read_only.contains("do not reach the parent"));
    }

    #[test]
    fn subagent_contract_forbids_nested_delegation() {
        let contract = subagent_contract_text("parent-session", SubagentType::ReadOnly);

        assert!(contract.contains("parent session `parent-session`"));
        assert!(contract.contains("cannot spawn nested delegations"));
        assert!(contract.contains("Do not call `delegate_writing_task`"));
        assert!(contract.contains("`delegate_readonly_tasks`"));
        assert!(contract.contains("`inspect_delegation`"));
        assert!(contract.contains("`cancel_delegation`"));
        assert!(contract.contains("`steer_subagent`"));
        assert!(contract.contains("final message/report is the durable handoff"));
        assert!(contract.contains("read-only subagent"));
    }

    #[test]
    fn subagent_provider_precedence_preserves_full_config() {
        let explicit = ProviderConfig {
            kind: agent_vocab::ProviderKind::Claude,
            model: "explicit".to_string(),
            reasoning_effort: agent_vocab::ReasoningEffort::Low,
            max_tokens: Some(10),
            prompt_cache: Some(json!({"key": "explicit"})),
        };
        let configured = ProviderConfig {
            kind: agent_vocab::ProviderKind::OpenAi,
            model: "configured".to_string(),
            reasoning_effort: agent_vocab::ReasoningEffort::High,
            max_tokens: Some(20),
            prompt_cache: Some(json!({"key": "configured"})),
        };
        let parent = ProviderConfig {
            kind: agent_vocab::ProviderKind::OpenAi,
            model: "parent".to_string(),
            reasoning_effort: agent_vocab::ReasoningEffort::XHigh,
            max_tokens: Some(30),
            prompt_cache: Some(json!({"key": "parent"})),
        };
        let selected = select_subagent_provider(
            Some(explicit.clone()),
            Some(configured.clone()),
            parent.clone(),
        );
        assert_eq!(
            serde_json::to_value(selected).expect("serialize"),
            serde_json::to_value(explicit).expect("serialize")
        );

        let selected = select_subagent_provider(None, Some(configured.clone()), parent.clone());
        assert_eq!(
            serde_json::to_value(selected).expect("serialize"),
            serde_json::to_value(configured).expect("serialize")
        );

        let selected = select_subagent_provider(None, None, parent.clone());
        assert_eq!(
            serde_json::to_value(selected).expect("serialize"),
            serde_json::to_value(parent).expect("serialize")
        );
    }
}
