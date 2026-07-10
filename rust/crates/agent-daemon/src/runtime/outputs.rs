use agent_core::AgentInput;
use agent_session::{SessionAction, SessionEvent, TranscriptStorageNode};
use agent_store::{InputPriority, PersistedAction, QueuedInputContent, SessionConfig};
use agent_vocab::{ProviderReplayItem, TranscriptItem};

use crate::types::{DispatchAction, RpcError, RuntimeSession};

pub(crate) fn agent_input_from_queued_priority(
    priority: InputPriority,
    content: QueuedInputContent,
) -> AgentInput {
    match content {
        QueuedInputContent::UserMessage(message) => match priority {
            InputPriority::Steer => AgentInput::steer_message(message),
            InputPriority::FollowUp => AgentInput::follow_up_message(message),
        },
        QueuedInputContent::DaemonToolObservation(observation) => {
            AgentInput::daemon_observation(observation)
        }
        QueuedInputContent::SubagentControl => {
            unreachable!("subagent control ledger rows are not dispatchable queue inputs")
        }
    }
}

pub(crate) fn collect_runtime_outputs(
    runtime: &mut RuntimeSession,
) -> (
    Vec<TranscriptStorageNode>,
    Vec<SessionEvent>,
    Vec<SessionAction>,
    Option<String>,
) {
    runtime.session.drive();
    let events = runtime.session.drain_events();
    let actions = runtime.session.drain_actions();
    let mut entries = Vec::new();
    for event in &events {
        if let SessionEvent::TranscriptItemAppended { entry_id, .. } = event {
            if let Some(entry) = runtime.session.transcript_store().get_entry(entry_id) {
                entries.push(entry.clone());
            }
        }
    }
    let active_leaf_id = runtime
        .session
        .transcript_store()
        .active_leaf_id()
        .map(str::to_string);
    (entries, events, actions, active_leaf_id)
}

pub(super) fn attach_provider_replay(
    entries: &mut [TranscriptStorageNode],
    provider_replay: Vec<ProviderReplayItem>,
) -> std::result::Result<(), RpcError> {
    if provider_replay.is_empty() {
        return Ok(());
    }
    let Some(entry) = entries
        .iter_mut()
        .rev()
        .find(|entry| matches!(entry.item, TranscriptItem::AssistantMessage(_)))
    else {
        return Err(RpcError::new(
            "invalid_provider_output",
            "provider replay sidecar had no assistant transcript entry",
        ));
    };
    entry.provider_replay.extend(provider_replay);
    Ok(())
}

pub(crate) fn attach_dispatch_config(
    persisted_actions: Vec<PersistedAction>,
    config: &SessionConfig,
) -> Vec<DispatchAction> {
    let mcp_snapshot = crate::provider_runtime::mcp_snapshot_for_session(config)
        .expect("persisted MCP manifest was validated before dispatch");
    persisted_actions
        .into_iter()
        .map(|action| DispatchAction {
            row_id: action.row_id,
            attempt_id: action.attempt_id,
            post_compaction_dispatch_lease: None,
            action: action.action,
            config: config.clone(),
            mcp_snapshot: mcp_snapshot.clone(),
        })
        .collect()
}
