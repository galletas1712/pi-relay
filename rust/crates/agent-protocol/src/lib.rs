use serde::{Deserialize, Serialize};

pub const RELAY_CORE_BRIDGE_PROTOCOL_VERSION: u32 = 1;
pub const SESSION_CORE_BRIDGE_PROTOCOL_VERSION: u32 = 1;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCoreQueueState {
	pub steering: Vec<String>,
	#[serde(rename = "followUp")]
	pub follow_up: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionCoreRunState {
	Idle,
	Running,
	Retrying,
	Compacting,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SessionCoreQueueKind {
	Steering,
	FollowUp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SessionCoreQueueChangeReason {
	EnqueueSteering,
	EnqueueFollowUp,
	ConsumeUserMessage,
	Clear,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCoreStateSnapshot {
	pub run_state: SessionCoreRunState,
	pub queue: SessionCoreQueueState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionShadowSyncReason {
	Init,
	Reset,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionShadowSnapshot {
	pub protocol_version: u32,
	pub generated_at: String,
	pub state: SessionCoreStateSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum SessionCoreCommandPayload {
	#[serde(rename = "queue/enqueue-steering")]
	EnqueueSteering { text: String },
	#[serde(rename = "queue/enqueue-follow-up")]
	EnqueueFollowUp { text: String },
	#[serde(rename = "queue/consume-user-message")]
	ConsumeUserMessage { text: String },
	#[serde(rename = "queue/clear")]
	Clear {},
	#[serde(rename = "run-state/set")]
	SetRunState {
		#[serde(rename = "runState")]
		run_state: SessionCoreRunState,
	},
}

impl SessionCoreCommandPayload {
	pub fn command_type(&self) -> &'static str {
		match self {
			Self::EnqueueSteering { .. } => "queue/enqueue-steering",
			Self::EnqueueFollowUp { .. } => "queue/enqueue-follow-up",
			Self::ConsumeUserMessage { .. } => "queue/consume-user-message",
			Self::Clear { .. } => "queue/clear",
			Self::SetRunState { .. } => "run-state/set",
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionBridgeCommand {
	Hello {
		#[serde(rename = "protocolVersion")]
		protocol_version: u32,
		mode: RelayCoreBridgeMode,
	},
	SyncState {
		reason: SessionShadowSyncReason,
		snapshot: SessionShadowSnapshot,
	},
	Dispatch {
		command: SessionCoreCommandPayload,
	},
	Dispose {},
}

impl SessionBridgeCommand {
	pub fn kind(&self) -> &'static str {
		match self {
			Self::Hello { .. } => "hello",
			Self::SyncState { .. } => "sync_state",
			Self::Dispatch { .. } => "dispatch",
			Self::Dispose { .. } => "dispose",
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionBridgeAck {
	pub accepted_command: String,
	pub accepted_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionBridgeError {
	pub message: String,
	pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionBridgeEvent {
	Diagnostic {
		level: DiagnosticLevel,
		message: String,
		details: Option<serde_json::Value>,
	},
	StateSynced {
		reason: SessionShadowSyncReason,
		#[serde(rename = "pendingMessageCount")]
		pending_message_count: usize,
		#[serde(rename = "runState")]
		run_state: SessionCoreRunState,
	},
	CommandApplied {
		#[serde(rename = "commandType")]
		command_type: String,
		#[serde(rename = "pendingMessageCount")]
		pending_message_count: usize,
		#[serde(rename = "runState")]
		run_state: SessionCoreRunState,
	},
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionBridgeMessage {
	Call { id: u64, command: SessionBridgeCommand },
	Result { id: u64, value: SessionBridgeAck },
	Error { id: u64, error: SessionBridgeError },
	Event { event: SessionBridgeEvent },
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

	#[test]
	fn serializes_session_dispatch_calls() {
		let message = SessionBridgeMessage::Call {
			id: 7,
			command: SessionBridgeCommand::Dispatch {
				command: SessionCoreCommandPayload::EnqueueFollowUp {
					text: "later".to_string(),
				},
			},
		};

		let encoded = serde_json::to_string(&message).expect("encode session bridge message");
		assert!(encoded.contains("\"dispatch\""));
		assert!(encoded.contains("\"queue/enqueue-follow-up\""));

		let decoded: SessionBridgeMessage =
			serde_json::from_str(&encoded).expect("decode session bridge message");
		assert_eq!(decoded, message);
	}
}
