use agent_vocab::{AssistantItem, AssistantMessage, ToolCall};
use serde_json::Value;
use thiserror::Error;

pub const CALL_DESCRIPTION_KEY: &str = "call_description";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CallDescriptionError {
    #[error("tool call {tool} ({call_id}) arguments must be a JSON object")]
    ArgumentsNotObject { tool: String, call_id: String },
    #[error("tool call {tool} ({call_id}) is missing required {CALL_DESCRIPTION_KEY}")]
    Missing { tool: String, call_id: String },
    #[error("tool call {tool} ({call_id}) {CALL_DESCRIPTION_KEY} must be a string")]
    NotString { tool: String, call_id: String },
    #[error("tool call {tool} ({call_id}) {CALL_DESCRIPTION_KEY} must not be blank")]
    Blank { tool: String, call_id: String },
    #[error("tool call {tool} ({call_id}) {CALL_DESCRIPTION_KEY} must be a single line")]
    Multiline { tool: String, call_id: String },
    #[error("tool call {tool} ({call_id}) arguments are invalid JSON: {message}")]
    InvalidJson {
        tool: String,
        call_id: String,
        message: String,
    },
}

/// Validate calls from one newly returned main-agent response.
///
/// Only the canonical `Bash` tool has a relay-owned call-description contract.
/// Every other tool's argument validation belongs to that tool's deserializer.
pub fn admit_new_tool_calls(assistant: &mut AssistantMessage) -> Result<(), CallDescriptionError> {
    for item in &mut assistant.items {
        let AssistantItem::ToolCall(call) = item else {
            continue;
        };
        if call.tool_name != "Bash" {
            continue;
        }
        let mut arguments: Value = serde_json::from_str(&call.args_json).map_err(|error| {
            CallDescriptionError::InvalidJson {
                tool: call.tool_name.clone(),
                call_id: call.id.to_string(),
                message: error.to_string(),
            }
        })?;
        let object =
            arguments
                .as_object_mut()
                .ok_or_else(|| CallDescriptionError::ArgumentsNotObject {
                    tool: call.tool_name.clone(),
                    call_id: call.id.to_string(),
                })?;
        let description = object
            .get(CALL_DESCRIPTION_KEY)
            .ok_or_else(|| CallDescriptionError::Missing {
                tool: call.tool_name.clone(),
                call_id: call.id.to_string(),
            })?
            .as_str()
            .ok_or_else(|| CallDescriptionError::NotString {
                tool: call.tool_name.clone(),
                call_id: call.id.to_string(),
            })?;
        let description = validate_description(&call.tool_name, &call.id.to_string(), description)?;
        object.insert(
            CALL_DESCRIPTION_KEY.to_string(),
            Value::String(description.to_string()),
        );
        call.args_json = serde_json::to_string(&arguments).expect("JSON value serializes");
    }
    Ok(())
}

fn validate_description<'a>(
    tool: &str,
    call_id: &str,
    description: &'a str,
) -> Result<&'a str, CallDescriptionError> {
    if description.contains(['\r', '\n']) {
        return Err(CallDescriptionError::Multiline {
            tool: tool.to_string(),
            call_id: call_id.to_string(),
        });
    }
    let description = description.trim();
    if description.is_empty() {
        return Err(CallDescriptionError::Blank {
            tool: tool.to_string(),
            call_id: call_id.to_string(),
        });
    }
    Ok(description)
}

/// Clone a persisted Bash call and remove its model-only metadata.
///
/// Missing metadata is deliberately a no-op so historical and recovered calls
/// execute without passing through new-response admission.
pub fn bash_call_for_execution(call: &ToolCall) -> ToolCall {
    if call.tool_name != "Bash" {
        return call.clone();
    }
    let Ok(Value::Object(mut arguments)) = serde_json::from_str(&call.args_json) else {
        return call.clone();
    };
    if arguments.remove(CALL_DESCRIPTION_KEY).is_none() {
        return call.clone();
    }
    let mut execution_call = call.clone();
    execution_call.args_json = serde_json::to_string(&arguments).expect("JSON object serializes");
    execution_call
}

#[cfg(test)]
mod tests {
    use agent_vocab::ToolCallId;
    use serde_json::json;

    use super::*;

    fn call(description: Value) -> AssistantMessage {
        AssistantMessage {
            items: vec![AssistantItem::ToolCall(ToolCall {
                id: ToolCallId::new("call_1"),
                tool_name: "Bash".to_string(),
                args_json: json!({
                    "command": "true",
                    CALL_DESCRIPTION_KEY: description,
                })
                .to_string(),
            })],
        }
    }

    #[test]
    fn admission_trims_valid_bash_descriptions() {
        let mut assistant = call(json!("  Check the workspace state.  "));
        admit_new_tool_calls(&mut assistant).expect("description is valid");
        let AssistantItem::ToolCall(call) = &assistant.items[0] else {
            panic!("expected tool call");
        };
        assert_eq!(
            call.args_value().expect("arguments parse")[CALL_DESCRIPTION_KEY],
            "Check the workspace state."
        );
    }

    #[test]
    fn admission_preserves_any_length_description_without_changing_arguments() {
        for character in ["x", "🦀"] {
            let description = format!("  {}  ", character.repeat(161));
            let mut assistant = call(json!(description));
            admit_new_tool_calls(&mut assistant).expect("any-length description is valid");
            let AssistantItem::ToolCall(call) = &assistant.items[0] else {
                panic!("expected tool call");
            };
            let expected_description = character.repeat(161);
            assert_eq!(
                call.args_value().expect("arguments parse"),
                json!({
                    "command": "true",
                    CALL_DESCRIPTION_KEY: expected_description,
                })
            );
        }
    }

    #[test]
    fn admission_rejects_invalid_descriptions() {
        for (description, expected) in [
            (json!(" "), "must not be blank"),
            (json!("first\nsecond"), "must be a single line"),
            (json!(42), "must be a string"),
        ] {
            let error = admit_new_tool_calls(&mut call(description)).expect_err("must reject");
            assert!(error.to_string().contains(expected), "{error}");
        }
        let mut missing = call(json!("valid"));
        let AssistantItem::ToolCall(call) = &mut missing.items[0] else {
            panic!("expected tool call");
        };
        call.args_json = json!({ "command": "true" }).to_string();
        assert!(matches!(
            admit_new_tool_calls(&mut missing),
            Err(CallDescriptionError::Missing { .. })
        ));
    }

    #[test]
    fn admission_rejects_non_object_bash_arguments() {
        let mut assistant = AssistantMessage {
            items: vec![AssistantItem::ToolCall(ToolCall {
                id: ToolCallId::new("call_string"),
                tool_name: "Bash".to_string(),
                args_json: json!("not an object").to_string(),
            })],
        };
        assert!(matches!(
            admit_new_tool_calls(&mut assistant),
            Err(CallDescriptionError::ArgumentsNotObject { .. })
        ));
    }

    #[test]
    fn admission_rejects_malformed_bash_json() {
        let mut assistant = AssistantMessage {
            items: vec![AssistantItem::ToolCall(ToolCall {
                id: ToolCallId::new("call_invalid_json"),
                tool_name: "Bash".to_string(),
                args_json: "{".to_string(),
            })],
        };
        assert!(matches!(
            admit_new_tool_calls(&mut assistant),
            Err(CallDescriptionError::InvalidJson { .. })
        ));
    }

    #[test]
    fn admission_leaves_non_bash_calls_unchanged() {
        let edit_args = json!({ "input": "patch" }).to_string();
        let mcp_args = json!({
            CALL_DESCRIPTION_KEY: "server operation mode",
            "value": 7,
        })
        .to_string();
        let mut assistant = AssistantMessage {
            items: vec![
                AssistantItem::ToolCall(ToolCall {
                    id: ToolCallId::new("call_edit"),
                    tool_name: "Edit".to_string(),
                    args_json: edit_args.clone(),
                }),
                AssistantItem::ToolCall(ToolCall {
                    id: ToolCallId::new("call_mcp"),
                    tool_name: "mcp__fixture__operation".to_string(),
                    args_json: mcp_args.clone(),
                }),
                AssistantItem::ToolCall(ToolCall {
                    id: ToolCallId::new("call_unknown"),
                    tool_name: "future_tool".to_string(),
                    args_json: "not json".to_string(),
                }),
            ],
        };
        admit_new_tool_calls(&mut assistant).expect("non-Bash calls are outside admission");
        let [AssistantItem::ToolCall(edit), AssistantItem::ToolCall(mcp), AssistantItem::ToolCall(unknown)] =
            assistant.items.as_slice()
        else {
            panic!("expected three tool calls");
        };
        assert_eq!(edit.args_json, edit_args);
        assert_eq!(mcp.args_json, mcp_args);
        assert_eq!(unknown.args_json, "not json");
    }

    #[test]
    fn execution_removes_metadata_only_from_bash() {
        let call = ToolCall {
            id: ToolCallId::new("call_1"),
            tool_name: "Bash".to_string(),
            args_json: json!({
                "command": "true",
                CALL_DESCRIPTION_KEY: "Run the command.",
            })
            .to_string(),
        };
        assert_eq!(
            bash_call_for_execution(&call)
                .args_value()
                .expect("arguments parse"),
            json!({ "command": "true" })
        );
        let historical = ToolCall {
            args_json: json!({ "command": "true" }).to_string(),
            ..call.clone()
        };
        assert_eq!(bash_call_for_execution(&historical), historical);

        let non_bash = ToolCall {
            tool_name: "Edit".to_string(),
            args_json: json!({
                CALL_DESCRIPTION_KEY: "an operational argument",
                "input": "patch",
            })
            .to_string(),
            ..call
        };
        assert_eq!(bash_call_for_execution(&non_bash), non_bash);
    }
}
