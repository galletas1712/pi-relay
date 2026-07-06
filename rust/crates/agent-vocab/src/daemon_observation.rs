use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ids::ToolCallId;
use crate::message::{ToolCall, ToolResultMessage, ToolResultStatus};

/// A daemon-authored tool observation that should be durable in the transcript
/// but must not imply the model chose a tool call.
///
/// Provider adapters that support synthetic historical observations render this
/// typed item as an adjacent tool call/result pair. The internal transcript
/// remains honest: the daemon authored the observation, not the assistant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonToolObservation {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub args_json: String,
    pub result_json: Value,
    pub status: ToolResultStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl DaemonToolObservation {
    pub fn new(
        tool_call_id: ToolCallId,
        tool_name: impl Into<String>,
        args_json: impl Into<String>,
        result_json: Value,
        status: ToolResultStatus,
        summary: Option<String>,
    ) -> Self {
        Self {
            tool_call_id,
            tool_name: tool_name.into(),
            args_json: args_json.into(),
            result_json,
            status,
            summary,
        }
    }

    pub fn inspect_delegation(
        tool_call_id: ToolCallId,
        delegation_id: impl Into<String>,
        summary: Option<String>,
        snapshot: Value,
    ) -> Self {
        let delegation_id = delegation_id.into();
        Self {
            tool_call_id,
            tool_name: "inspect_delegation".to_string(),
            args_json: json!({ "delegation_id": delegation_id }).to_string(),
            result_json: snapshot,
            status: ToolResultStatus::Success,
            summary,
        }
    }

    pub fn args_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.args_json)
    }

    pub fn result_text(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&self.result_json)
    }

    pub fn as_tool_call(&self) -> ToolCall {
        ToolCall {
            id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            args_json: self.args_json.clone(),
        }
    }

    pub fn as_tool_result(&self) -> Result<ToolResultMessage, serde_json::Error> {
        Ok(ToolResultMessage {
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            output: self.result_text()?,
            status: self.status.clone(),
        })
    }

    pub fn render_text(&self) -> Result<String, serde_json::Error> {
        daemon_observation_fallback_text(
            &self.tool_name,
            &self.args_json,
            self.summary.as_deref(),
            &self.result_json,
        )
    }
}

fn daemon_observation_fallback_text(
    tool_name: &str,
    args_json: &str,
    summary: Option<&str>,
    result_json: &Value,
) -> Result<String, serde_json::Error> {
    let result_json = serde_json::to_string_pretty(result_json)?;
    let args_value: Value =
        serde_json::from_str(args_json).unwrap_or_else(|_| Value::String(args_json.to_string()));
    let args_inline = serde_json::to_string(&args_value)?;
    let mut text = String::new();
    text.push_str("Daemon observation: ");
    text.push_str(tool_name);
    text.push('\n');
    text.push_str(
        "This message was authored by the pi-relay daemon, not by an assistant tool call.\n",
    );
    text.push_str("It records daemon-observed state equivalent to `");
    text.push_str(tool_name);
    text.push('(');
    text.push_str(&args_inline);
    text.push_str(")` at observation time.\n");
    text.push_str("Full transcript contents and large prompts/messages are not inlined; artifact paths in the snapshot point to files to inspect only if needed.");
    if let Some(summary) = summary.filter(|summary| !summary.trim().is_empty()) {
        text.push_str("\n\nSummary: ");
        text.push_str(summary.trim());
    }
    text.push_str("\n\nSnapshot JSON follows:\n```json\n");
    text.push_str(&result_json);
    text.push_str("\n```");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn daemon_tool_observation_round_trips() {
        let observation = DaemonToolObservation::inspect_delegation(
            ToolCallId::new("call_delegation_1_attempt_1"),
            "delegation_1",
            Some("completed with status done: 1 ok, 0 failed".to_string()),
            json!({
                "delegation_id": "delegation_1",
                "status": "done",
            }),
        );

        let value = serde_json::to_value(&observation).expect("serialize");

        assert_eq!(value["tool_call_id"], "call_delegation_1_attempt_1");
        assert_eq!(value["tool_name"], "inspect_delegation");
        assert_eq!(value["status"], "Success");
        assert_eq!(value["args_json"], "{\"delegation_id\":\"delegation_1\"}");
        let round_trip: DaemonToolObservation = serde_json::from_value(value).expect("deserialize");
        assert_eq!(round_trip, observation);
    }

    #[test]
    fn inspect_delegation_observation_renders_as_daemon_authored_text() {
        let observation = DaemonToolObservation::inspect_delegation(
            ToolCallId::new("call_delegation_1_attempt_1"),
            "delegation_1",
            Some("completed with status done: 1 ok, 0 failed".to_string()),
            json!({
                "delegation_id": "delegation_1",
                "status": "done",
                "subagents": [{
                    "id": "child_1",
                    "transcript_file": "child_1/transcript.md",
                }],
            }),
        );

        let text = observation.render_text().expect("observation renders");

        assert!(text.starts_with("Daemon observation: inspect_delegation"));
        assert!(text.contains("not by an assistant tool call"));
        assert!(text
            .contains("equivalent to `inspect_delegation({\"delegation_id\":\"delegation_1\"})`"));
        assert!(text.contains("large prompts/messages are not inlined"));
        assert!(text.contains("Snapshot JSON follows"));
        assert!(text.contains("\"delegation_id\": \"delegation_1\""));
        assert!(text.contains("\"transcript_file\""));
    }

    #[test]
    fn subject_id_is_escaped_in_inline_equivalent_call() {
        let observation = DaemonToolObservation::inspect_delegation(
            ToolCallId::new("call_quoted"),
            "delegation_\"quoted\"",
            None,
            json!({ "delegation_id": "delegation_\"quoted\"" }),
        );

        let text = observation.render_text().expect("observation renders");

        assert!(
            text.contains("inspect_delegation({\"delegation_id\":\"delegation_\\\"quoted\\\"\"})")
        );
    }

    #[test]
    fn converts_to_canonical_tool_call_and_result() {
        let observation = DaemonToolObservation::inspect_delegation(
            ToolCallId::new("call_delegation_1_attempt_1"),
            "delegation_1",
            None,
            json!({ "ok": true }),
        );

        let call = observation.as_tool_call();
        let result = observation.as_tool_result().expect("result text");

        assert_eq!(call.id.as_str(), "call_delegation_1_attempt_1");
        assert_eq!(call.tool_name, "inspect_delegation");
        assert_eq!(call.args_json, "{\"delegation_id\":\"delegation_1\"}");
        assert_eq!(result.tool_call_id.as_str(), "call_delegation_1_attempt_1");
        assert_eq!(result.tool_name, "inspect_delegation");
        assert_eq!(result.status, ToolResultStatus::Success);
        assert!(result.output.contains("\"ok\": true"));
    }
}
