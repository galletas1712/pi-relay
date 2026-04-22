use agent_protocol::{BridgeError, BridgeMessage};
use orchestrator_core::{apply_command, OrchestratorCoreState};
use std::io::{self, BufRead, Write};

pub fn handle_message(state: &mut OrchestratorCoreState, message: BridgeMessage) -> Vec<BridgeMessage> {
	match message {
		BridgeMessage::Call { id, command } => {
			match apply_command(state, &command) {
				Ok((ack, _effects)) => vec![BridgeMessage::Result { id, value: ack }],
				Err(error) => vec![BridgeMessage::Error { id, error }],
			}
		}
		BridgeMessage::Result { .. } | BridgeMessage::Error { .. } | BridgeMessage::Event { .. } => Vec::new(),
	}
}

pub fn run_stdio<R: BufRead, W: Write>(reader: R, mut writer: W) -> io::Result<()> {
	let mut state = OrchestratorCoreState::default();

	for line in reader.lines() {
		let line = line?;
		if line.trim().is_empty() {
			continue;
		}

		let frames = match serde_json::from_str::<BridgeMessage>(&line) {
			Ok(message) => handle_message(&mut state, message),
			Err(error) => vec![BridgeMessage::Error {
				id: 0,
				error: BridgeError {
					message: format!("invalid relay-core bridge frame: {error}"),
					data: None,
				},
			}],
		};

		for frame in frames {
			let encoded = serde_json::to_string(&frame)
				.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
			writer.write_all(encoded.as_bytes())?;
			writer.write_all(b"\n")?;
		}
		writer.flush()?;

		if state.disposed {
			break;
		}
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use agent_protocol::{BridgeCommand, BridgeMessage, RelayCoreBridgeMode, RELAY_CORE_BRIDGE_PROTOCOL_VERSION};
	use std::io::Cursor;

	#[test]
	fn acknowledges_hello_frames_over_stdio() {
		let input = format!(
			"{}\n",
			serde_json::to_string(&BridgeMessage::Call {
				id: 41,
				command: BridgeCommand::Hello {
					protocol_version: RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
					mode: RelayCoreBridgeMode::Shadow,
				},
			})
			.expect("serialize hello frame"),
		);
		let mut output = Vec::new();

		run_stdio(Cursor::new(input), &mut output).expect("run stdio loop");

		let response = String::from_utf8(output).expect("utf8 output");
		let frame: BridgeMessage = serde_json::from_str(response.trim()).expect("deserialize response frame");

		match frame {
			BridgeMessage::Result { id, value } => {
				assert_eq!(id, 41);
				assert_eq!(value.accepted_command, "hello");
			}
			other => panic!("expected result frame, got {other:?}"),
		}
	}

	#[test]
	fn returns_an_error_frame_for_protocol_version_mismatches() {
		let input = format!(
			"{}\n",
			serde_json::to_string(&BridgeMessage::Call {
				id: 42,
				command: BridgeCommand::Hello {
					protocol_version: RELAY_CORE_BRIDGE_PROTOCOL_VERSION + 1,
					mode: RelayCoreBridgeMode::Shadow,
				},
			})
			.expect("serialize hello frame"),
		);
		let mut output = Vec::new();

		run_stdio(Cursor::new(input), &mut output).expect("run stdio loop");

		let response = String::from_utf8(output).expect("utf8 output");
		let frame: BridgeMessage = serde_json::from_str(response.trim()).expect("deserialize response frame");

		match frame {
			BridgeMessage::Error { id, error } => {
				assert_eq!(id, 42);
				assert!(error.message.contains("protocol mismatch for hello"));
			}
			other => panic!("expected error frame, got {other:?}"),
		}
	}
}
