use agent_store::CreateForkRequest;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::history::{ensure_history_source_idle, parse_history_target, prepare_history_target};
use crate::provider_runtime::render_pi_prompt;
use crate::runtime::{
    clear_event_buffer_after_commit, map_source_mutation_error, publish_events, SessionDriver,
};
use crate::state::AppState;
use crate::types::RpcError;

pub(crate) async fn fork(state: &AppState, params: Value) -> Result<Value, RpcError> {
    let target = parse_history_target(&params, &[])?;
    let driver = SessionDriver::acquire(state, &target.session_id).await;
    ensure_history_source_idle(state, &driver, &target.session_id).await?;
    let mut config = state.repo.load_session_config(&target.session_id).await?;
    if config.project_id.is_none() {
        return Err(RpcError::new(
            "project_required",
            "history.fork requires a managed project session",
        ));
    }
    prepare_history_target(state, &target).await?;
    let child_session_id = loop {
        let candidate = format!("session_{}", Uuid::new_v4());
        if !state.repo.session_exists(&candidate).await? {
            break candidate;
        }
    };

    let child_workspace_id = format!("workspace_{}", Uuid::new_v4());
    let (workspace_id, workspaces) = state
        .runtime_hosts
        .fork_session_from_parent(
            &target.session_id,
            &config.workspace_id,
            &config.workspaces,
            &child_workspace_id,
        )
        .await?;
    config.workspace_id = workspace_id;
    config.workspaces = workspaces;
    config.metadata = fork_metadata(
        config.metadata,
        &target.session_id,
        target.leaf_id.as_deref(),
    );
    let result = match render_pi_prompt(state, &config).await {
        Ok(system_prompt) => {
            config.system_prompt = system_prompt;
            state
                .repo
                .create_fork(CreateForkRequest {
                    source_session_id: &target.session_id,
                    child_session_id: &child_session_id,
                    config: &config,
                    target: target.as_store_target(),
                })
                .await
                .map_err(map_source_mutation_error)
        }
        Err(error) => Err(error.into()),
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            if let Err(cleanup_error) = state
                .runtime_hosts
                .execute(
                    &config.runtime_id,
                    agent_runtime_protocol::RuntimeCommand::DestroySession {
                        workspace_id: child_workspace_id,
                    },
                )
                .await
            {
                eprintln!(
                    "failed to clean up child workspace {child_session_id} after history.fork: {cleanup_error:#}"
                );
            }
            return Err(error);
        }
    };
    publish_events(state, result.events);
    clear_event_buffer_after_commit(state, &child_session_id, "history.fork").await;
    Ok(json!({
        "session_id": result.session_id,
        "source_session_id": result.source_session_id,
        "source_leaf_id": result.source_leaf_id,
        "active_leaf_id": result.active_leaf_id,
        "session_revision": result.session_revision,
        "queue_revision": result.queue_revision,
        "transcript_revision": result.transcript_revision,
        "last_event_id": result.last_event_id,
    }))
}

fn fork_metadata(
    mut metadata: Value,
    source_session_id: &str,
    source_leaf_id: Option<&str>,
) -> Value {
    let object = if let Value::Object(object) = &mut metadata {
        object
    } else {
        metadata = json!({});
        metadata
            .as_object_mut()
            .expect("metadata was forced to an object")
    };
    for key in [
        "archived",
        "hidden",
        "subagent",
        "subagent_type",
        "role_name",
        "task",
        "role_file_path",
        "subagent_parent_idle_notification_key",
    ] {
        object.remove(key);
    }
    if let Some(compaction) = object.get_mut("compaction").and_then(Value::as_object_mut) {
        compaction.remove("auto_state");
    }
    // A fork keeps the source's title and auto-title preference so it remains
    // recognizable, but lifecycle state above always starts fresh.
    object.insert("prompt_profile".to_string(), json!("parent"));
    object.insert(
        "fork".to_string(),
        json!({
            "source_session_id": source_session_id,
            "source_leaf_id": source_leaf_id,
        }),
    );
    metadata
}
