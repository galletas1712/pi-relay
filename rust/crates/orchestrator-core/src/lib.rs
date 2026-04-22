use agent_protocol::{BridgeAck, BridgeCommand, ShadowSyncReason};
use std::time::{SystemTime, UNIX_EPOCH};

pub use agent_protocol::OrchestratorShadowSnapshot;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrchestratorCoreState {
	pub initialized: bool,
	pub disposed: bool,
	pub sync_count: usize,
	pub last_snapshot: Option<OrchestratorShadowSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEffect {
	SnapshotUpdated { reason: &'static str, agent_count: usize },
	CoreDisposed,
}

pub fn apply_command(state: &mut OrchestratorCoreState, command: &BridgeCommand) -> (BridgeAck, Vec<CoreEffect>) {
	match command {
		BridgeCommand::Hello { .. } => {
			state.initialized = true;
			(ack(command.kind()), Vec::new())
		}
		BridgeCommand::SyncSnapshot { reason, snapshot } => {
			state.initialized = true;
			state.sync_count += 1;
			state.last_snapshot = Some(snapshot.clone());
			(
				ack(command.kind()),
				vec![CoreEffect::SnapshotUpdated {
					reason: match reason {
						ShadowSyncReason::Init => "init",
						ShadowSyncReason::Change => "change",
					},
					agent_count: snapshot.agents.len(),
				}],
			)
		}
		BridgeCommand::Dispose { .. } => {
			state.disposed = true;
			(ack(command.kind()), vec![CoreEffect::CoreDisposed])
		}
	}
}

fn ack(command: &str) -> BridgeAck {
	let accepted_at = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|duration| format!("unix:{}", duration.as_secs()))
		.unwrap_or_else(|_| "unix:0".to_string());

	BridgeAck {
		accepted_command: command.to_string(),
		accepted_at,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use agent_protocol::{
		AgentSummary, BridgeCommand, OrchestratorShadowSnapshot, RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
		ShadowSyncReason,
	};

	#[test]
	fn stores_the_latest_shadow_snapshot() {
		let mut state = OrchestratorCoreState::default();
		let command = BridgeCommand::SyncSnapshot {
			reason: ShadowSyncReason::Init,
			snapshot: OrchestratorShadowSnapshot {
				protocol_version: RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
				root_agent_id: "root".to_string(),
				generated_at: "2026-04-22T00:00:00.000Z".to_string(),
				agents: vec![AgentSummary {
					id: "root".to_string(),
					parent_id: None,
					role: "root".to_string(),
					status: "idle".to_string(),
					depth: 0,
					child_count: 0,
					session_file: None,
					last_output: None,
				}],
			},
		};

		let (_ack, effects) = apply_command(&mut state, &command);

		assert!(state.initialized);
		assert_eq!(state.sync_count, 1);
		assert_eq!(state.last_snapshot.as_ref().map(|snapshot| snapshot.root_agent_id.as_str()), Some("root"));
		assert_eq!(effects, vec![CoreEffect::SnapshotUpdated { reason: "init", agent_count: 1 }]);
	}
}
