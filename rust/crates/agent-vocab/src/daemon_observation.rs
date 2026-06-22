use serde_json::Value;

use crate::message::UserMessage;

/// A daemon-authored observation that should be durable in the transcript but
/// must not imply the model chose a tool call.
///
/// Providers have stricter, provider-specific invariants around tool-call and
/// tool-result adjacency. Until a provider explicitly supports daemon-authored
/// synthetic tool pairs, observations render as ordinary user-visible text with
/// a stable heading and embedded JSON payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonObservation {
    action: String,
    subject_id: String,
    summary: Option<String>,
    json_payload: Value,
}

impl DaemonObservation {
    pub fn inspect_delegation(
        delegation_id: impl Into<String>,
        summary: Option<String>,
        snapshot: Value,
    ) -> Self {
        Self {
            action: "inspect_delegation".to_string(),
            subject_id: delegation_id.into(),
            summary,
            json_payload: snapshot,
        }
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    pub fn subject_id(&self) -> &str {
        &self.subject_id
    }

    pub fn json_payload(&self) -> &Value {
        &self.json_payload
    }

    pub fn render_text(&self) -> Result<String, serde_json::Error> {
        let json_payload = serde_json::to_string_pretty(&self.json_payload)?;
        let subject_id = serde_json::to_string(&self.subject_id)?;
        let mut text = String::new();
        text.push_str("Daemon observation: ");
        text.push_str(&self.action);
        text.push('\n');
        text.push_str(
            "This message was authored by the pi-relay daemon, not by an assistant tool call.\n",
        );
        text.push_str("It records daemon-observed state equivalent to `");
        text.push_str(&self.action);
        text.push_str("({ \"delegation_id\": ");
        text.push_str(&subject_id);
        text.push_str(" })` at observation time.\n");
        text.push_str("Full transcript contents are not inlined; artifact paths in the snapshot point to files to inspect only if needed.");
        if let Some(summary) = self
            .summary
            .as_deref()
            .filter(|summary| !summary.trim().is_empty())
        {
            text.push_str("\n\nSummary: ");
            text.push_str(summary.trim());
        }
        text.push_str("\n\nSnapshot JSON follows:\n```json\n");
        text.push_str(&json_payload);
        text.push_str("\n```");
        Ok(text)
    }

    pub fn into_user_message(self) -> Result<UserMessage, serde_json::Error> {
        self.render_text().map(UserMessage::text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inspect_delegation_observation_renders_as_daemon_authored_text() {
        let observation = DaemonObservation::inspect_delegation(
            "delegation_1",
            Some("completed with status done: 1 ok, 0 failed".to_string()),
            json!({
                "delegation_id": "delegation_1",
                "status": "done",
                "subagents": [{
                    "id": "child_1",
                    "transcript_path": "/tmp/.pi-handoff/delegation_1/child_1/transcript.md",
                }],
            }),
        );

        let text = observation.render_text().expect("observation renders");

        assert!(text.starts_with("Daemon observation: inspect_delegation"));
        assert!(text.contains("not by an assistant tool call"));
        assert!(text.contains(
            "equivalent to `inspect_delegation({ \"delegation_id\": \"delegation_1\" })`"
        ));
        assert!(text.contains("Full transcript contents are not inlined"));
        assert!(text.contains("Snapshot JSON follows"));
        assert!(text.contains("\"delegation_id\": \"delegation_1\""));
        assert!(text.contains("\"transcript_path\""));
    }

    #[test]
    fn subject_id_is_escaped_in_inline_equivalent_call() {
        let observation = DaemonObservation::inspect_delegation(
            "delegation_\"quoted\"",
            None,
            json!({ "delegation_id": "delegation_\"quoted\"" }),
        );

        let text = observation.render_text().expect("observation renders");

        assert!(text
            .contains("inspect_delegation({ \"delegation_id\": \"delegation_\\\"quoted\\\"\" })"));
    }
}
