use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::message::ToolResultStatus;

/// A daemon-authored tool observation that should be durable in the transcript
/// but must not imply the model chose a tool call.
///
/// Provider adapters render this typed item as a plain user-role message built
/// from [`DaemonToolObservation::render_text`]. The internal transcript remains
/// honest: the daemon authored the observation, not the assistant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonToolObservation {
    pub tool_name: String,
    pub args_json: String,
    pub result_json: Value,
    pub status: ToolResultStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl DaemonToolObservation {
    pub fn new(
        tool_name: impl Into<String>,
        args_json: impl Into<String>,
        result_json: Value,
        status: ToolResultStatus,
        summary: Option<String>,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            args_json: args_json.into(),
            result_json,
            status,
            summary,
        }
    }

    /// The daemon's observation of one delegation's state. `tool_name` is the
    /// observation subject, not an invocable tool: the model has no delegation
    /// inspection tool.
    pub fn delegation_status(
        delegation_id: impl Into<String>,
        summary: Option<String>,
        snapshot: Value,
    ) -> Self {
        let delegation_id = delegation_id.into();
        Self {
            tool_name: "delegation_status".to_string(),
            args_json: json!({ "delegation_id": delegation_id }).to_string(),
            result_json: snapshot,
            status: ToolResultStatus::Success,
            summary,
        }
    }

    pub fn render_text(&self) -> Result<String, serde_json::Error> {
        daemon_observation_text(
            &self.tool_name,
            &self.args_json,
            self.summary.as_deref(),
            &self.result_json,
        )
    }
}

/// The model-visible form of a daemon observation: a plain, self-describing
/// text block that names the daemon as its author. Both provider adapters and
/// the handoff markdown render observations through this.
fn daemon_observation_text(
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
    text.push_str("It records daemon-observed state for ");
    text.push_str(&args_inline);
    text.push_str(" at observation time; there is no tool to re-run it.\n");
    text.push_str("Full transcript contents and large prompts/messages are not inlined; artifact paths in the snapshot point to files to read with ordinary file tools only if needed.");
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
        let observation = DaemonToolObservation::delegation_status(
            "delegation_1",
            Some("completed with status done: 1 ok, 0 failed".to_string()),
            json!({
                "delegation_id": "delegation_1",
                "status": "done",
            }),
        );

        let value = serde_json::to_value(&observation).expect("serialize");

        assert_eq!(value["tool_name"], "delegation_status");
        assert_eq!(value["status"], "Success");
        assert_eq!(value["args_json"], "{\"delegation_id\":\"delegation_1\"}");
        let round_trip: DaemonToolObservation = serde_json::from_value(value).expect("deserialize");
        assert_eq!(round_trip, observation);
    }

    #[test]
    fn delegation_observation_renders_as_daemon_authored_text() {
        let observation = DaemonToolObservation::delegation_status(
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

        assert!(text.starts_with("Daemon observation: delegation_status"));
        assert!(text.contains("not by an assistant tool call"));
        assert!(text.contains("state for {\"delegation_id\":\"delegation_1\"}"));
        assert!(text.contains("there is no tool to re-run it"));
        assert!(text.contains("large prompts/messages are not inlined"));
        assert!(text.contains("Snapshot JSON follows"));
        assert!(text.contains("\"delegation_id\": \"delegation_1\""));
        assert!(text.contains("\"transcript_file\""));
    }

    /// Historical transcripts carry observations named after the retired
    /// `inspect_delegation` tool and a `tool_call_id` field that no longer
    /// exists. They must still deserialize and render.
    #[test]
    fn historical_inspect_delegation_observation_still_renders() {
        let observation: DaemonToolObservation = serde_json::from_value(json!({
            "tool_call_id": "call_inspect_delegation_deadbeef",
            "tool_name": "inspect_delegation",
            "args_json": "{\"delegation_id\":\"delegation_1\"}",
            "result_json": { "delegation_id": "delegation_1", "status": "done" },
            "status": "Success",
            "summary": "Delegation delegation_1 completed",
        }))
        .expect("historical observation deserializes");

        let text = observation.render_text().expect("observation renders");

        assert!(text.starts_with("Daemon observation: inspect_delegation"));
        assert!(text.contains("Summary: Delegation delegation_1 completed"));
        assert!(text.contains("\"status\": \"done\""));
    }

    #[test]
    fn subject_id_is_escaped_in_rendered_args() {
        let observation = DaemonToolObservation::delegation_status(
            "delegation_\"quoted\"",
            None,
            json!({ "delegation_id": "delegation_\"quoted\"" }),
        );

        let text = observation.render_text().expect("observation renders");

        assert!(text.contains("state for {\"delegation_id\":\"delegation_\\\"quoted\\\"\"}"));
    }
}
