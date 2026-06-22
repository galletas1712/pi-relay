use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

use crate::codec::from_params;
use crate::state::AppState;
use crate::types::RpcError;

#[derive(Clone, Default)]
pub(crate) struct ReplRegistry {
    repls: Arc<Mutex<HashMap<String, Arc<PythonRepl>>>>,
}

impl ReplRegistry {
    pub(crate) async fn execute(
        &self,
        session_id: &str,
        code: String,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<Value, RpcError> {
        let repl = self.get_or_start(session_id).await?;
        let run = repl.execute(code);
        let result = if let Some(timeout_ms) = timeout_ms {
            match timeout(Duration::from_millis(timeout_ms), run).await {
                Ok(result) => result,
                Err(_) => {
                    self.remove_and_kill(session_id).await;
                    return Err(RpcError::new(
                        "repl_timeout",
                        format!("Python REPL execution exceeded {timeout_ms}ms"),
                    ));
                }
            }
        } else {
            run.await
        };
        match result {
            Ok(result) => Ok(result),
            Err(error) => {
                if matches!(error.code.as_str(), "repl_exited" | "repl_protocol_error") {
                    self.remove_and_kill(session_id).await;
                }
                Err(error)
            }
        }
    }

    async fn get_or_start(
        &self,
        session_id: &str,
    ) -> std::result::Result<Arc<PythonRepl>, RpcError> {
        let mut repls = self.repls.lock().await;
        if let Some(repl) = repls.get(session_id) {
            return Ok(repl.clone());
        }
        let repl = Arc::new(PythonRepl::start().await?);
        repls.insert(session_id.to_string(), repl.clone());
        Ok(repl)
    }

    pub(crate) async fn remove_and_kill(&self, session_id: &str) {
        let repl = self.repls.lock().await.remove(session_id);
        if let Some(repl) = repl {
            repl.kill().await;
        }
    }

    pub(crate) async fn kill_all(&self) {
        let repls = std::mem::take(&mut *self.repls.lock().await);
        for repl in repls.into_values() {
            repl.kill().await;
        }
    }
}

struct PythonRepl {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    lines: Mutex<Lines<BufReader<ChildStdout>>>,
    exec_lock: Mutex<()>,
    next_exec_id: AtomicU64,
}

impl PythonRepl {
    async fn start() -> std::result::Result<Self, RpcError> {
        let python = std::env::var("PI_RELAY_PYTHON").unwrap_or_else(|_| "python3".to_string());
        let mut child = Command::new(&python)
            .arg("-u")
            .arg("-c")
            .arg(PYTHON_REPL_BOOTSTRAP)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                RpcError::new(
                    "repl_start_failed",
                    format!("failed to start Python REPL with {python}: {error}"),
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RpcError::new("repl_start_failed", "Python stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RpcError::new("repl_start_failed", "Python stdout was not piped"))?;
        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            lines: Mutex::new(BufReader::new(stdout).lines()),
            exec_lock: Mutex::new(()),
            next_exec_id: AtomicU64::new(1),
        })
    }

    async fn execute(&self, code: String) -> std::result::Result<Value, RpcError> {
        let _guard = self.exec_lock.lock().await;
        let exec_id = self.next_exec_id.fetch_add(1, Ordering::Relaxed);
        self.write_control(json!({
            "type": "exec",
            "id": exec_id,
            "code": code,
        }))
        .await?;

        let message = self.read_control().await?;
        let message_type = message.get("type").and_then(Value::as_str).unwrap_or("");
        match message_type {
            "exec_result" => {
                if message.get("id").and_then(Value::as_u64) != Some(exec_id) {
                    return Err(RpcError::new(
                        "repl_protocol_error",
                        "received exec_result for a different request",
                    ));
                }
                Ok(message)
            }
            other => Err(RpcError::new(
                "repl_protocol_error",
                format!("unexpected Python REPL message type: {other}"),
            )),
        }
    }

    async fn write_control(&self, value: Value) -> std::result::Result<(), RpcError> {
        let mut stdin = self.stdin.lock().await;
        let mut line = serde_json::to_vec(&value).map_err(anyhow::Error::from)?;
        line.push(b'\n');
        stdin.write_all(&line).await.map_err(anyhow::Error::from)?;
        stdin.flush().await.map_err(anyhow::Error::from)?;
        Ok(())
    }

    async fn read_control(&self) -> std::result::Result<Value, RpcError> {
        let mut lines = self.lines.lock().await;
        let line = lines
            .next_line()
            .await
            .map_err(anyhow::Error::from)?
            .ok_or_else(|| RpcError::new("repl_exited", "Python REPL exited"))?;
        serde_json::from_str(&line).map_err(|error| {
            RpcError::new(
                "repl_protocol_error",
                format!("Python REPL emitted invalid JSON control line: {error}: {line}"),
            )
        })
    }

    async fn kill(&self) {
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

#[derive(Debug, Deserialize)]
struct ReplExecParams {
    session_id: String,
    code: String,
    timeout_ms: Option<u64>,
}

pub(crate) async fn repl_exec(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: ReplExecParams = from_params(params)?;
    let session_id = params.session_id.trim().to_string();
    if session_id.is_empty() {
        return Err(RpcError::new(
            "invalid_params",
            "session_id cannot be empty",
        ));
    }
    if params.code.trim().is_empty() {
        return Err(RpcError::new("invalid_params", "code cannot be empty"));
    }
    if !state
        .repo
        .session_exists(&session_id)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Err(RpcError::new("session_not_found", "session not found"));
    }
    state
        .repls
        .execute(&session_id, params.code, params.timeout_ms)
        .await
}

const PYTHON_REPL_BOOTSTRAP: &str = r#"
import ast
import contextlib
import io
import json
import sys
import traceback

_CONTROL_IN = sys.stdin
_CONTROL_OUT = sys.stdout


def _write_control(message):
    _CONTROL_OUT.write(json.dumps(message, separators=(",", ":")) + "\n")
    _CONTROL_OUT.flush()


def _read_control():
    line = _CONTROL_IN.readline()
    if not line:
        raise RuntimeError("host closed REPL stdin")
    return json.loads(line)


def _jsonish(value):
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, list):
        return [_jsonish(item) for item in value]
    if isinstance(value, tuple):
        return [_jsonish(item) for item in value]
    if isinstance(value, dict):
        return {str(key): _jsonish(item) for key, item in value.items()}
    if hasattr(value, "to_dict"):
        return value.to_dict()
    return None


def _exec_cell(code, globals_dict):
    tree = ast.parse(code, mode="exec")
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        prefix = ast.Module(body=tree.body[:-1], type_ignores=[])
        suffix = ast.Expression(tree.body[-1].value)
        ast.fix_missing_locations(prefix)
        ast.fix_missing_locations(suffix)
        if prefix.body:
            exec(compile(prefix, "<pi-relay-repl>", "exec"), globals_dict)
        result = eval(compile(suffix, "<pi-relay-repl>", "eval"), globals_dict)
        globals_dict["_"] = result
        return result
    compiled = compile(tree, "<pi-relay-repl>", "exec")
    exec(compiled, globals_dict)
    return None


_GLOBALS = {
    "__name__": "__pi_relay_repl__",
}


def _handle_exec(message):
    stdout = io.StringIO()
    stderr = io.StringIO()
    result = None
    try:
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            result = _exec_cell(message.get("code") or "", _GLOBALS)
        try:
            result_repr = repr(result)
        except Exception:
            result_repr = "<repr failed>"
        _write_control({
            "type": "exec_result",
            "id": message.get("id"),
            "ok": True,
            "stdout": stdout.getvalue(),
            "stderr": stderr.getvalue(),
            "result_repr": result_repr,
            "result_json": _jsonish(result),
            "error": None,
        })
    except Exception as exc:
        _write_control({
            "type": "exec_result",
            "id": message.get("id"),
            "ok": False,
            "stdout": stdout.getvalue(),
            "stderr": stderr.getvalue(),
            "result_repr": None,
            "result_json": None,
            "error": {
                "message": str(exc),
                "traceback": traceback.format_exc(),
            },
        })


while True:
    message = _read_control()
    if message.get("type") == "exec":
        _handle_exec(message)
    else:
        raise RuntimeError(f"unexpected control message: {message!r}")
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn python_repl_preserves_state_and_captures_last_expression() {
        if Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_err()
        {
            eprintln!("skipping python REPL test; python3 is not available");
            return;
        }

        let repl = PythonRepl::start().await.expect("start python repl");
        repl.write_control(json!({
            "type": "exec",
            "id": 1,
            "code": "x = 41\nprint('ready')\nx + 1",
        }))
        .await
        .expect("write first exec");
        let first = repl.read_control().await.expect("read first result");
        assert_eq!(first["type"], "exec_result");
        assert_eq!(first["id"], 1);
        assert_eq!(first["ok"], true);
        assert_eq!(first["stdout"], "ready\n");
        assert_eq!(first["result_repr"], "42");
        assert_eq!(first["result_json"], 42);

        repl.write_control(json!({
            "type": "exec",
            "id": 2,
            "code": "x += 1\nx",
        }))
        .await
        .expect("write second exec");
        let second = repl.read_control().await.expect("read second result");
        assert_eq!(second["type"], "exec_result");
        assert_eq!(second["id"], 2);
        assert_eq!(second["ok"], true);
        assert_eq!(second["result_json"], 42);

        repl.kill().await;
    }
}
