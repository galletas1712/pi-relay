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
        Ok(result) => {
            let output = format_repl_tool_output(&result);
            if result.get("ok").and_then(Value::as_bool) == Some(false) {
                ToolResultMessage::error(call.id.clone(), &call.tool_name, output)
            } else {
                ToolResultMessage::success(call.id.clone(), &call.tool_name, output)
            }
        }
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
    let result_repr = result.get("result_repr").and_then(Value::as_str);
    let result_json = result.get("result_json").unwrap_or(&Value::Null);
    let error = result.get("error").unwrap_or(&Value::Null);

    let mut sections = Vec::new();
    if !stdout.is_empty() {
        sections.push(format!("stdout:\n{stdout}"));
    }
    if !stderr.is_empty() {
        sections.push(format!("stderr:\n{stderr}"));
    }
    if !error.is_null() {
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            sections.push(format!("error:\n{message}"));
        }
        if let Some(traceback) = error.get("traceback").and_then(Value::as_str) {
            sections.push(format!("traceback:\n{traceback}"));
        }
        if !error.is_object() {
            sections.push(format!("error_json:\n{}", render_json(error)));
        }
    }
    if let Some(result_repr) = result_repr {
        sections.push(format!("result_repr:\n{result_repr}"));
    } else if error.is_null() {
        sections.push("result_repr:\nNone".to_string());
    }
    if !result_json.is_null() {
        sections.push(format!("result_json:\n{}", render_json(result_json)));
    }
    sections.join("\n\n")
}

fn render_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
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

    #[test]
    fn formats_repl_exception_with_traceback() {
        let output = format_repl_tool_output(&json!({
            "stdout": "before\n",
            "stderr": "",
            "result_repr": null,
            "result_json": null,
            "error": {
                "message": "division by zero",
                "traceback": "Traceback..."
            }
        }));

        assert!(output.contains("stdout:\nbefore\n"));
        assert!(output.contains("error:\ndivision by zero"));
        assert!(output.contains("traceback:\nTraceback..."));
        assert!(!output.contains("result_repr:\nNone"));
    }
}
