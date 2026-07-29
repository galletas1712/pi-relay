use std::collections::BTreeSet;

use agent_vocab::{AssistantItem, AssistantMessage, ToolCall};
use serde_json::{json, Value};
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
    #[error(
        "apply_patch output must begin with `call_description: ` followed by one short sentence"
    )]
    MissingPatchHeader,
}

/// Validate calls from one newly returned main-agent response.
///
/// MCP calls are exempt by their frozen snapshot names and remain untouched.
pub fn admit_new_tool_calls(
    assistant: &mut AssistantMessage,
    mcp_tool_names: &BTreeSet<String>,
) -> Result<(), CallDescriptionError> {
    for item in &mut assistant.items {
        let AssistantItem::ToolCall(call) = item else {
            continue;
        };
        if mcp_tool_names.contains(&call.tool_name) {
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

/// Parse a new OpenAI apply_patch custom call into the persisted JSON shape.
pub fn normalize_apply_patch_input(input: &str) -> Result<String, CallDescriptionError> {
    let (header, patch) = input
        .split_once('\n')
        .ok_or(CallDescriptionError::MissingPatchHeader)?;
    let description = header
        .strip_prefix(&format!("{CALL_DESCRIPTION_KEY}: "))
        .ok_or(CallDescriptionError::MissingPatchHeader)?;
    let description = validate_description("Edit", "custom", description)?;
    Ok(json!({
        CALL_DESCRIPTION_KEY: description,
        "input": patch,
    })
    .to_string())
}

/// Clone a persisted first-party call and remove model-only metadata.
///
/// Missing metadata is deliberately a no-op so historical and recovered calls
/// execute without passing through new-response admission.
pub fn tool_call_for_execution(call: &ToolCall) -> ToolCall {
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
    fn admission_trims_valid_descriptions() {
        let mut assistant = call(json!("  Check the workspace state.  "));
        admit_new_tool_calls(&mut assistant, &BTreeSet::new()).expect("description is valid");
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
            admit_new_tool_calls(&mut assistant, &BTreeSet::new())
                .expect("any-length description is valid");
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
            let error = admit_new_tool_calls(&mut call(description), &BTreeSet::new())
                .expect_err("must reject");
            assert!(error.to_string().contains(expected), "{error}");
        }
        let mut missing = call(json!("valid"));
        let AssistantItem::ToolCall(call) = &mut missing.items[0] else {
            panic!("expected tool call");
        };
        call.args_json = json!({ "command": "true" }).to_string();
        assert!(matches!(
            admit_new_tool_calls(&mut missing, &BTreeSet::new()),
            Err(CallDescriptionError::Missing { .. })
        ));
    }

    #[test]
    fn mixed_admission_preserves_mcp_arguments_and_checks_first_party_calls() {
        let mcp_name = "mcp__fixture__operation";
        let original_args = json!({
            CALL_DESCRIPTION_KEY: null,
            "value": 7,
        })
        .to_string();
        let mut assistant = AssistantMessage {
            items: vec![
                AssistantItem::ToolCall(ToolCall {
                    id: ToolCallId::new("call_mcp"),
                    tool_name: mcp_name.to_string(),
                    args_json: original_args.clone(),
                }),
                AssistantItem::ToolCall(ToolCall {
                    id: ToolCallId::new("call_first_party"),
                    tool_name: "Bash".to_string(),
                    args_json: json!({ "command": "true" }).to_string(),
                }),
            ],
        };
        assert!(matches!(
            admit_new_tool_calls(&mut assistant, &BTreeSet::from([mcp_name.to_string()])),
            Err(CallDescriptionError::Missing { .. })
        ));
        let AssistantItem::ToolCall(call) = &assistant.items[0] else {
            unreachable!()
        };
        assert_eq!(call.args_json, original_args);
    }

    #[test]
    fn execution_removes_reserved_metadata_but_accepts_historical_calls() {
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
            tool_call_for_execution(&call)
                .args_value()
                .expect("arguments parse"),
            json!({ "command": "true" })
        );
        let historical = ToolCall {
            args_json: json!({ "command": "true" }).to_string(),
            ..call
        };
        assert_eq!(tool_call_for_execution(&historical), historical);
    }

    #[test]
    fn patch_header_normalizes_to_description_and_raw_input() {
        let arguments = normalize_apply_patch_input(
            "call_description: Add the fixture file.\n*** Begin Patch\n*** End Patch\n",
        )
        .expect("patch input normalizes");
        assert_eq!(
            serde_json::from_str::<Value>(&arguments).expect("arguments parse"),
            json!({
                CALL_DESCRIPTION_KEY: "Add the fixture file.",
                "input": "*** Begin Patch\n*** End Patch\n",
            })
        );
        assert!(normalize_apply_patch_input("*** Begin Patch\n*** End Patch\n").is_err());
    }

    #[test]
    fn patch_header_preserves_any_length_description() {
        let patch = "*** Begin Patch\n*** End Patch\n";
        let arguments = normalize_apply_patch_input(&format!(
            "call_description:   {}  \n{patch}",
            "🦀".repeat(161)
        ))
        .expect("any-length patch description is valid");
        assert_eq!(
            serde_json::from_str::<Value>(&arguments).expect("arguments parse"),
            json!({
                CALL_DESCRIPTION_KEY: "🦀".repeat(161),
                "input": patch,
            })
        );
    }
}
