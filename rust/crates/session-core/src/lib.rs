use agent_protocol::{
	SessionBridgeAck, SessionBridgeCommand, SessionCoreCommandPayload, SessionCoreQueueState,
	SessionCoreRunState, SessionCoreStateSnapshot, SessionShadowSyncReason,
};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCoreShadowState {
	pub initialized: bool,
	pub disposed: bool,
	pub sync_count: usize,
	pub state: SessionCoreStateSnapshot,
}

impl Default for SessionCoreShadowState {
	fn default() -> Self {
		Self {
			initialized: false,
			disposed: false,
			sync_count: 0,
			state: SessionCoreStateSnapshot {
				run_state: SessionCoreRunState::Idle,
				queue: SessionCoreQueueState {
					steering: Vec::new(),
					follow_up: Vec::new(),
				},
			},
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEffect {
	StateSynced { reason: &'static str, pending_message_count: usize },
	QueueUpdated { reason: &'static str, pending_message_count: usize },
	RunStateUpdated { run_state: SessionCoreRunState },
	CoreDisposed,
}

pub fn apply_command(
	state: &mut SessionCoreShadowState,
	command: &SessionBridgeCommand,
) -> (SessionBridgeAck, Vec<CoreEffect>) {
	match command {
		SessionBridgeCommand::Hello { .. } => {
			state.initialized = true;
			(ack(command.kind()), Vec::new())
		}
		SessionBridgeCommand::SyncState { reason, snapshot } => {
			state.initialized = true;
			state.sync_count += 1;
			state.state = snapshot.state.clone();
			(
				ack(command.kind()),
				vec![CoreEffect::StateSynced {
					reason: match reason {
						SessionShadowSyncReason::Init => "init",
						SessionShadowSyncReason::Reset => "reset",
					},
					pending_message_count: pending_message_count(&state.state),
				}],
			)
		}
		SessionBridgeCommand::Dispatch { command } => {
			state.initialized = true;
			(ack(command.kind()), apply_session_core_command(&mut state.state, command))
		}
		SessionBridgeCommand::Dispose { .. } => {
			state.disposed = true;
			(ack(command.kind()), vec![CoreEffect::CoreDisposed])
		}
	}
}

fn apply_session_core_command(
	state: &mut SessionCoreStateSnapshot,
	command: &SessionCoreCommandPayload,
) -> Vec<CoreEffect> {
	match command {
		SessionCoreCommandPayload::EnqueueSteering { text } => {
			state.queue.steering.push(text.clone());
			vec![CoreEffect::QueueUpdated {
				reason: "enqueue-steering",
				pending_message_count: pending_message_count(state),
			}]
		}
		SessionCoreCommandPayload::EnqueueFollowUp { text } => {
			state.queue.follow_up.push(text.clone());
			vec![CoreEffect::QueueUpdated {
				reason: "enqueue-follow-up",
				pending_message_count: pending_message_count(state),
			}]
		}
		SessionCoreCommandPayload::ConsumeUserMessage { text } => {
			if remove_first_match(&mut state.queue.steering, text)
				|| remove_first_match(&mut state.queue.follow_up, text)
			{
				vec![CoreEffect::QueueUpdated {
					reason: "consume-user-message",
					pending_message_count: pending_message_count(state),
				}]
			} else {
				Vec::new()
			}
		}
		SessionCoreCommandPayload::Clear { .. } => {
			state.queue.steering.clear();
			state.queue.follow_up.clear();
			vec![CoreEffect::QueueUpdated {
				reason: "clear",
				pending_message_count: 0,
			}]
		}
		SessionCoreCommandPayload::SetRunState { run_state } => {
			if state.run_state == *run_state {
				Vec::new()
			} else {
				state.run_state = run_state.clone();
				vec![CoreEffect::RunStateUpdated {
					run_state: state.run_state.clone(),
				}]
			}
		}
	}
}

fn remove_first_match(values: &mut Vec<String>, text: &str) -> bool {
	if let Some(index) = values.iter().position(|value| value == text) {
		values.remove(index);
		true
	} else {
		false
	}
}

fn pending_message_count(state: &SessionCoreStateSnapshot) -> usize {
	state.queue.steering.len() + state.queue.follow_up.len()
}

fn ack(command: &str) -> SessionBridgeAck {
	let accepted_at = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|duration| format!("unix:{}", duration.as_secs()))
		.unwrap_or_else(|_| "unix:0".to_string());

	SessionBridgeAck {
		accepted_command: command.to_string(),
		accepted_at,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use agent_protocol::{
		SessionBridgeCommand, SessionCoreCommandPayload, SessionCoreQueueState, SessionCoreRunState,
		SessionCoreStateSnapshot, SessionShadowSnapshot, SessionShadowSyncReason,
		SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
	};

	#[test]
	fn sync_state_replaces_the_shadow_snapshot() {
		let mut state = SessionCoreShadowState::default();
		let command = SessionBridgeCommand::SyncState {
			reason: SessionShadowSyncReason::Init,
			snapshot: SessionShadowSnapshot {
				protocol_version: SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
				generated_at: "2026-04-22T00:00:00.000Z".to_string(),
				state: SessionCoreStateSnapshot {
					run_state: SessionCoreRunState::Retrying,
					queue: SessionCoreQueueState {
						steering: vec!["urgent".to_string()],
						follow_up: vec!["later".to_string()],
					},
				},
			},
		};

		let (_ack, effects) = apply_command(&mut state, &command);

		assert!(state.initialized);
		assert_eq!(state.sync_count, 1);
		assert_eq!(state.state.run_state, SessionCoreRunState::Retrying);
		assert_eq!(state.state.queue.follow_up, vec!["later".to_string()]);
		assert_eq!(
			effects,
			vec![CoreEffect::StateSynced {
				reason: "init",
				pending_message_count: 2,
			}],
		);
	}

	#[test]
	fn consume_user_message_prefers_steering_before_follow_up() {
		let mut state = SessionCoreShadowState {
			initialized: true,
			disposed: false,
			sync_count: 0,
			state: SessionCoreStateSnapshot {
				run_state: SessionCoreRunState::Idle,
				queue: SessionCoreQueueState {
					steering: vec!["same".to_string()],
					follow_up: vec!["same".to_string()],
				},
			},
		};

		let (_ack, effects) = apply_command(
			&mut state,
			&SessionBridgeCommand::Dispatch {
				command: SessionCoreCommandPayload::ConsumeUserMessage {
					text: "same".to_string(),
				},
			},
		);

		assert_eq!(state.state.queue.steering, Vec::<String>::new());
		assert_eq!(state.state.queue.follow_up, vec!["same".to_string()]);
		assert_eq!(
			effects,
			vec![CoreEffect::QueueUpdated {
				reason: "consume-user-message",
				pending_message_count: 1,
			}],
		);
	}
}
