use std::path::PathBuf;
use std::sync::Arc;

use agent_session::AgentSession;
use agent_store::{
    InputPriority, QueuedInputContent, SessionActivity, SessionConfig, SubagentType,
};
use agent_vocab::{ProviderConfig, UserMessage};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::codec::{from_params, parse_user_message};
use crate::provider_runtime::render_pi_prompt;
use crate::runtime::{
    agent_input_from_queued_priority, attach_dispatch_config, collect_runtime_outputs,
    publish_events, SessionDriver,
};
use crate::state::AppState;
use crate::types::{DispatchAction, RpcError, RuntimeSession};
use crate::workspaces::{RequestedWorkspace, WorkspaceSelection};

pub(crate) async fn session_start(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: StartSessionParams = from_params(params)?;
    let session_id = params
        .session_id
        .unwrap_or_else(|| format!("session_{}", Uuid::new_v4()));
    let project_id = params.project_id;
    let priority = params.priority.unwrap_or(InputPriority::FollowUp);
    let content = parse_user_message(params.content)?;

    let driver = SessionDriver::acquire(state, &session_id).await;
    if state.repo.session_exists(&session_id).await? {
        let current = state.repo.load_session_config(&session_id).await?;
        state
            .workspaces
            .ensure_session(&session_id, &current.outer_cwd, &current.workspaces)
            .await?;
        return Ok(json!({
            "session_id": session_id,
            "project_id": current.project_id,
            "activity": state.repo.activity(&session_id).await?,
            "replayed": true,
        }));
    }

    let (outer_cwd, workspaces) = if let Some(project_id) = project_id {
        let project = state.repo.get_project(project_id).await?;
        let selection = WorkspaceSelection::from_requested(
            params
                .workspaces
                .map(|workspaces| workspaces.into_iter().map(Into::into).collect()),
        );
        let selected = selection
            .resolve(&project.workspaces)
            .map_err(|error| RpcError::new("invalid_params", error.to_string()))?;
        state
            .workspaces
            .materialize_session(project_id, &session_id, &project.workspaces, &selected)
            .await?
    } else {
        let cwd = home_dir_for_ephemeral_session()?
            .to_string_lossy()
            .into_owned();
        (cwd, Vec::new())
    };
    let mut config = SessionConfig {
        project_id,
        outer_cwd,
        workspaces,
        system_prompt: String::new(),
        provider: params.provider,
        metadata: parent_session_metadata(params.metadata.unwrap_or_else(|| json!({}))),
    };
    config.system_prompt = render_pi_prompt(state, &config)?;

    let started = start_prepared_session_with_driver(
        state,
        &driver,
        PreparedSessionStart {
            session_id,
            config,
            priority,
            content,
            client_input_id: params.client_input_id,
            parent_session_id: None,
            subagent_type: None,
            delegation_id: None,
            dispatch_mode: PreparedSessionDispatchMode::Auto,
        },
    )
    .await?;
    Ok(json!({
        "session_id": started.session_id,
        "project_id": started.project_id,
        "activity": started.activity,
        "replayed": started.replayed,
    }))
}

fn parent_session_metadata(metadata: Value) -> Value {
    let mut metadata = match metadata {
        Value::Object(map) => Value::Object(map),
        _ => json!({}),
    };
    let Value::Object(map) = &mut metadata else {
        unreachable!("metadata was forced to an object");
    };
    map.insert("prompt_profile".to_string(), json!("parent"));
    metadata
}

pub(crate) struct PreparedSessionStart {
    pub(crate) session_id: String,
    pub(crate) config: SessionConfig,
    pub(crate) priority: InputPriority,
    pub(crate) content: UserMessage,
    pub(crate) client_input_id: Option<String>,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) subagent_type: Option<SubagentType>,
    pub(crate) delegation_id: Option<String>,
    pub(crate) dispatch_mode: PreparedSessionDispatchMode,
}

pub(crate) enum PreparedSessionDispatchMode {
    Auto,
    Deferred,
}

pub(crate) struct StartedSession {
    pub(crate) session_id: String,
    pub(crate) project_id: Option<Uuid>,
    pub(crate) activity: SessionActivity,
    pub(crate) replayed: bool,
    pub(crate) dispatches: Vec<DispatchAction>,
}

pub(crate) async fn start_prepared_session(
    state: &AppState,
    request: PreparedSessionStart,
) -> std::result::Result<StartedSession, RpcError> {
    let driver = SessionDriver::acquire(state, &request.session_id).await;
    start_prepared_session_with_driver(state, &driver, request).await
}

async fn start_prepared_session_with_driver(
    state: &AppState,
    driver: &SessionDriver,
    request: PreparedSessionStart,
) -> std::result::Result<StartedSession, RpcError> {
    let PreparedSessionStart {
        session_id,
        config,
        priority,
        content,
        client_input_id,
        parent_session_id,
        subagent_type,
        delegation_id,
        dispatch_mode,
    } = request;
    let project_id = config.project_id;

    if state.repo.session_exists(&session_id).await? {
        let current = state.repo.load_session_config(&session_id).await?;
        state
            .workspaces
            .ensure_session(&session_id, &current.outer_cwd, &current.workspaces)
            .await?;
        return Ok(StartedSession {
            session_id: session_id.clone(),
            project_id: current.project_id,
            activity: state.repo.activity(&session_id).await?,
            replayed: true,
            dispatches: Vec::new(),
        });
    }

    state
        .workspaces
        .ensure_session(&session_id, &config.outer_cwd, &config.workspaces)
        .await?;

    let mut session = AgentSession::new();
    session
        .enqueue_input(agent_input_from_queued_priority(
            priority,
            QueuedInputContent::user_message(content.clone()),
        ))
        .map_err(|error| RpcError::new("invalid_input", error.to_string()))?;
    let mut runtime = RuntimeSession {
        session,
        config,
        persisted_active_leaf_id: None,
    };
    let (entries, events, actions, active_leaf_id) = collect_runtime_outputs(&mut runtime);
    let config = runtime.config.clone();
    let (frames, persisted_actions) = state
        .repo
        .start_session_outputs_with_parent(
            &session_id,
            &config,
            &entries,
            active_leaf_id.as_deref(),
            &events,
            &actions,
            priority,
            &content,
            client_input_id.as_deref(),
            parent_session_id.as_deref(),
            subagent_type,
            delegation_id.as_deref(),
        )
        .await?;
    runtime.persisted_active_leaf_id.clone_from(&active_leaf_id);

    if frames.is_empty() {
        return Ok(StartedSession {
            session_id: session_id.clone(),
            project_id,
            activity: state.repo.activity(&session_id).await?,
            replayed: true,
            dispatches: Vec::new(),
        });
    }
    let dispatches = attach_dispatch_config(persisted_actions, &config);

    state
        .active
        .lock()
        .await
        .insert(session_id.clone(), Arc::new(Mutex::new(runtime)));
    publish_events(state, frames);
    match dispatch_mode {
        PreparedSessionDispatchMode::Auto => driver.dispatch(dispatches.clone()).await?,
        PreparedSessionDispatchMode::Deferred => {}
    }

    Ok(StartedSession {
        session_id: session_id.clone(),
        project_id,
        activity: state.repo.activity(&session_id).await?,
        replayed: false,
        dispatches,
    })
}

#[derive(Debug, Deserialize)]
struct StartSessionParams {
    session_id: Option<String>,
    project_id: Option<Uuid>,
    provider: ProviderConfig,
    metadata: Option<Value>,
    client_input_id: Option<String>,
    priority: Option<InputPriority>,
    content: Value,
    /// Optional subset of the project's workspaces to materialize for this session,
    /// each with an optional per-session git branch override. Omit to materialize
    /// every project workspace at its default branch. Ignored for ephemeral sessions.
    workspaces: Option<Vec<StartSessionWorkspace>>,
}

#[derive(Debug, Deserialize)]
struct StartSessionWorkspace {
    workspace_dir: String,
    #[serde(default)]
    branch: Option<String>,
}

impl From<StartSessionWorkspace> for RequestedWorkspace {
    fn from(value: StartSessionWorkspace) -> Self {
        let branch = value
            .branch
            .map(|branch| branch.trim().to_string())
            .filter(|branch| !branch.is_empty());
        Self {
            workspace_dir: value.workspace_dir,
            branch,
        }
    }
}

fn home_dir_for_ephemeral_session() -> std::result::Result<PathBuf, RpcError> {
    let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) else {
        return Err(RpcError::new(
            "home_unavailable",
            "HOME is required for ephemeral sessions",
        ));
    };
    let home = PathBuf::from(home);
    if !home.is_dir() {
        return Err(RpcError::new(
            "home_unavailable",
            format!("HOME is not a directory: {}", home.display()),
        ));
    }
    Ok(home)
}
