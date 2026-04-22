use agent_protocol::{SessionBridgeError, SessionBridgeMessage};
use session_core::{apply_command, SessionCoreShadowState};
use std::io::{self, BufRead, Write};

pub fn handle_message(
	state: &mut SessionCoreShadowState,
	message: SessionBridgeMessage,
) -> Vec<SessionBridgeMessage> {
	match message {
		SessionBridgeMessage::Call { id, command } => {
			let (ack, _effects) = apply_command(state, &command);
			vec![SessionBridgeMessage::Result { id, value: ack }]
		}
		SessionBridgeMessage::Result { .. }
		| SessionBridgeMessage::Error { .. }
		| SessionBridgeMessage::Event { .. } => Vec::new(),
	}
}

pub fn run_stdio<R: BufRead, W: Write>(reader: R, mut writer: W) -> io::Result<()> {
	let mut state = SessionCoreShadowState::default();

	for line in reader.lines() {
		let line = line?;
		if line.trim().is_empty() {
			continue;
		}

		let frames = match serde_json::from_str::<SessionBridgeMessage>(&line) {
			Ok(message) => handle_message(&mut state, message),
			Err(error) => vec![SessionBridgeMessage::Error {
				id: 0,
				error: SessionBridgeError {
					message: format!("invalid session-core bridge frame: {error}"),
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
	use agent_protocol::{
		RelayCoreBridgeMode, SessionBridgeCommand, SessionBridgeMessage,
		SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
	};
	use std::io::Cursor;

	#[test]
	fn acknowledges_hello_frames_over_stdio() {
		let input = format!(
			"{}\n",
			serde_json::to_string(&SessionBridgeMessage::Call {
				id: 17,
				command: SessionBridgeCommand::Hello {
					protocol_version: SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
					mode: RelayCoreBridgeMode::Shadow,
				},
			})
			.expect("serialize hello frame"),
		);
		let mut output = Vec::new();

		run_stdio(Cursor::new(input), &mut output).expect("run stdio loop");

		let response = String::from_utf8(output).expect("utf8 output");
		let frame: SessionBridgeMessage =
			serde_json::from_str(response.trim()).expect("deserialize response frame");

		match frame {
			SessionBridgeMessage::Result { id, value } => {
				assert_eq!(id, 17);
				assert_eq!(value.accepted_command, "hello");
			}
			other => panic!("expected result frame, got {other:?}"),
		}
	}
}
