use serde::{Deserialize, Serialize};

pub const RELAY_CORE_BRIDGE_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSummary {
	pub id: String,
	pub parent_id: Option<String>,
	pub role: String,
	pub status: String,
	pub depth: usize,
	pub child_count: usize,
	pub session_file: Option<String>,
	pub last_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorShadowSnapshot {
	pub protocol_version: u32,
	pub root_agent_id: String,
	pub generated_at: String,
	pub agents: Vec<AgentSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelayCoreBridgeMode {
	Shadow,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShadowSyncReason {
	Init,
	Change,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeCommand {
	Hello {
		#[serde(rename = "protocolVersion")]
		protocol_version: u32,
		mode: RelayCoreBridgeMode,
	},
	SyncSnapshot {
		reason: ShadowSyncReason,
		snapshot: OrchestratorShadowSnapshot,
	},
	Dispose {},
}

impl BridgeCommand {
	pub fn kind(&self) -> &'static str {
		match self {
			Self::Hello { .. } => "hello",
			Self::SyncSnapshot { .. } => "sync_snapshot",
			Self::Dispose { .. } => "dispose",
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BridgeAck {
	pub accepted_command: String,
	pub accepted_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BridgeError {
	pub message: String,
	pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
	Info,
	Warn,
	Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeEvent {
	Diagnostic {
		level: DiagnosticLevel,
		message: String,
		details: Option<serde_json::Value>,
	},
	ShadowDiff {
		summary: String,
		#[serde(rename = "mismatchCount")]
		mismatch_count: usize,
		details: Option<serde_json::Value>,
	},
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeMessage {
	Call { id: u64, command: BridgeCommand },
	Result { id: u64, value: BridgeAck },
	Error { id: u64, error: BridgeError },
	Event { event: BridgeEvent },
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn serializes_sync_snapshot_calls() {
		let message = BridgeMessage::Call {
			id: 1,
			command: BridgeCommand::SyncSnapshot {
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
			},
		};

		let encoded = serde_json::to_string(&message).expect("encode bridge message");
		assert!(encoded.contains("\"sync_snapshot\""));

		let decoded: BridgeMessage = serde_json::from_str(&encoded).expect("decode bridge message");
		assert_eq!(decoded, message);
	}
}
