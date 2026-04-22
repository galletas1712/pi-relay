use agent_protocol::{
	BridgeAck, BridgeCommand, BridgeError, ShadowSyncReason, RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
};
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

pub fn apply_command(
	state: &mut OrchestratorCoreState,
	command: &BridgeCommand,
) -> Result<(BridgeAck, Vec<CoreEffect>), BridgeError> {
	match command {
		BridgeCommand::Hello { protocol_version, .. } => {
			ensure_protocol_version(command.kind(), *protocol_version)?;
			state.initialized = true;
			Ok((ack(command.kind()), Vec::new()))
		}
		BridgeCommand::SyncSnapshot { reason, snapshot } => {
			ensure_protocol_version(command.kind(), snapshot.protocol_version)?;
			state.initialized = true;
			state.sync_count += 1;
			state.last_snapshot = Some(snapshot.clone());
			Ok((
				ack(command.kind()),
				vec![CoreEffect::SnapshotUpdated {
					reason: match reason {
						ShadowSyncReason::Init => "init",
						ShadowSyncReason::Change => "change",
					},
					agent_count: snapshot.agents.len(),
				}],
			))
		}
		BridgeCommand::Dispose { .. } => {
			state.disposed = true;
			Ok((ack(command.kind()), vec![CoreEffect::CoreDisposed]))
		}
	}
}

fn ensure_protocol_version(command: &str, protocol_version: u32) -> Result<(), BridgeError> {
	if protocol_version == RELAY_CORE_BRIDGE_PROTOCOL_VERSION {
		return Ok(());
	}

	Err(BridgeError {
		message: format!(
			"relay-core bridge protocol mismatch for {command}: expected v{RELAY_CORE_BRIDGE_PROTOCOL_VERSION}, got v{protocol_version}"
		),
		data: None,
	})
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

		let (_ack, effects) = apply_command(&mut state, &command).expect("accept matching protocol version");

		assert!(state.initialized);
		assert_eq!(state.sync_count, 1);
		assert_eq!(state.last_snapshot.as_ref().map(|snapshot| snapshot.root_agent_id.as_str()), Some("root"));
		assert_eq!(effects, vec![CoreEffect::SnapshotUpdated { reason: "init", agent_count: 1 }]);
	}

	#[test]
	fn rejects_mismatched_hello_protocol_versions() {
		let mut state = OrchestratorCoreState::default();
		let error = apply_command(
			&mut state,
			&BridgeCommand::Hello {
				protocol_version: RELAY_CORE_BRIDGE_PROTOCOL_VERSION + 1,
				mode: agent_protocol::RelayCoreBridgeMode::Shadow,
			},
		)
		.expect_err("reject mismatched protocol version");

		assert_eq!(
			error.message,
			format!(
				"relay-core bridge protocol mismatch for hello: expected v{RELAY_CORE_BRIDGE_PROTOCOL_VERSION}, got v{}",
				RELAY_CORE_BRIDGE_PROTOCOL_VERSION + 1,
			),
		);
		assert!(!state.initialized);
	}

	#[test]
	fn rejects_mismatched_snapshot_protocol_versions_before_mutating_state() {
		let mut state = OrchestratorCoreState::default();
		let error = apply_command(
			&mut state,
			&BridgeCommand::SyncSnapshot {
				reason: ShadowSyncReason::Change,
				snapshot: OrchestratorShadowSnapshot {
					protocol_version: RELAY_CORE_BRIDGE_PROTOCOL_VERSION + 7,
					root_agent_id: "root".to_string(),
					generated_at: "2026-04-22T00:00:00.000Z".to_string(),
					agents: Vec::new(),
				},
			},
		)
		.expect_err("reject mismatched snapshot protocol version");

		assert_eq!(
			error.message,
			format!(
				"relay-core bridge protocol mismatch for sync_snapshot: expected v{RELAY_CORE_BRIDGE_PROTOCOL_VERSION}, got v{}",
				RELAY_CORE_BRIDGE_PROTOCOL_VERSION + 7,
			),
		);
		assert!(!state.initialized);
		assert_eq!(state.sync_count, 0);
		assert!(state.last_snapshot.is_none());
	}
}
