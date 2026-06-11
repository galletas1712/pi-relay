use agent_vocab::{ToolCall, ToolResultMessage};
use serde::Deserialize;
use serde_json::Value;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct PythonReplArgs {
    code: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub(crate) fn is_repl_tool_name(name: &str) -> bool {
    name == "PythonRepl"
}

pub(crate) async fn run_repl_tool(
    state: &AppState,
    session_id: &str,
    call: &ToolCall,
) -> ToolResultMessage {
    let args: PythonReplArgs = match serde_json::from_str(&call.args_json) {
        Ok(args) => args,
        Err(error) => {
            return ToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                format!("PythonRepl arguments were invalid JSON: {error}"),
            )
        }
    };
    if args.code.trim().is_empty() {
        return ToolResultMessage::error(
            call.id.clone(),
            &call.tool_name,
            "PythonRepl code cannot be empty".to_string(),
        );
    }

    match state
        .repls
        .execute(state, session_id, args.code, args.timeout_ms)
        .await
    {
        Ok(result) => ToolResultMessage::success(
            call.id.clone(),
            &call.tool_name,
            format_repl_tool_output(&result),
        ),
        Err(error) => ToolResultMessage::error(
            call.id.clone(),
            &call.tool_name,
            format!("{}: {}", error.code, error.message),
        ),
    }
}

fn format_repl_tool_output(result: &Value) -> String {
    let stdout = result.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = result.get("stderr").and_then(Value::as_str).unwrap_or("");
    let result_repr = result
        .get("result_repr")
        .and_then(Value::as_str)
        .unwrap_or("None");
    let result_json = result.get("result_json").unwrap_or(&Value::Null);

    let mut sections = Vec::new();
    if !stdout.is_empty() {
        sections.push(format!("stdout:\n{stdout}"));
    }
    if !stderr.is_empty() {
        sections.push(format!("stderr:\n{stderr}"));
    }
    sections.push(format!("result_repr:\n{result_repr}"));
    if !result_json.is_null() {
        let rendered =
            serde_json::to_string_pretty(result_json).unwrap_or_else(|_| result_json.to_string());
        sections.push(format!("result_json:\n{rendered}"));
    }
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_repl_output_with_json_result() {
        let output = format_repl_tool_output(&json!({
            "stdout": "hello\n",
            "stderr": "",
            "result_repr": "SubagentResult(session_id='child')",
            "result_json": {
                "session_id": "child",
                "text": "done"
            }
        }));

        assert!(output.contains("stdout:\nhello\n"));
        assert!(output.contains("result_repr:\nSubagentResult"));
        assert!(output.contains("\"session_id\": \"child\""));
    }
}
