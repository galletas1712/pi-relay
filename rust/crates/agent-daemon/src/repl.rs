use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use agent_store::SessionActivity;
use agent_vocab::{TranscriptItem, UserMessage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout, Duration};

use crate::codec::{from_params, required_string};
use crate::rpc_views;
use crate::runtime::SessionDriver;
use crate::state::AppState;
use crate::subagents::{require_known_subagent, subagent_list, subagent_spawn};
use crate::types::RpcError;
use crate::{enqueue_session_input, interrupt_session, SessionInputRequest};

const DEFAULT_REPL_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const DEFAULT_SUBAGENT_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const SUBAGENT_POLL_INTERVAL_MS: u64 = 250;
const PARENT_CONTEXT_MAX_CHARS: usize = 60 * 1024;

#[derive(Clone, Default)]
pub(crate) struct ReplRegistry {
    repls: Arc<Mutex<HashMap<String, Arc<PythonRepl>>>>,
}

impl ReplRegistry {
    pub(crate) async fn execute(
        &self,
        state: &AppState,
        session_id: &str,
        code: String,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<Value, RpcError> {
        let repl = self.get_or_start(session_id).await?;
        let timeout_ms = timeout_ms.unwrap_or(DEFAULT_REPL_TIMEOUT_MS);
        let run = repl.execute(state, code);
        match timeout(Duration::from_millis(timeout_ms), run).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => {
                if matches!(error.code.as_str(), "repl_exited" | "repl_protocol_error") {
                    self.remove_and_kill(session_id).await;
                }
                Err(error)
            }
            Err(_) => {
                self.remove_and_kill(session_id).await;
                Err(RpcError::new(
                    "repl_timeout",
                    format!("Python REPL execution exceeded {timeout_ms}ms"),
                ))
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
        let repl = Arc::new(PythonRepl::start(session_id.to_string()).await?);
        repls.insert(session_id.to_string(), repl.clone());
        Ok(repl)
    }

    async fn remove_and_kill(&self, session_id: &str) {
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
    parent_session_id: String,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    lines: Mutex<Lines<BufReader<ChildStdout>>>,
    exec_lock: Mutex<()>,
    next_exec_id: AtomicU64,
}

impl PythonRepl {
    async fn start(parent_session_id: String) -> std::result::Result<Self, RpcError> {
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
            parent_session_id,
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            lines: Mutex::new(BufReader::new(stdout).lines()),
            exec_lock: Mutex::new(()),
            next_exec_id: AtomicU64::new(1),
        })
    }

    async fn execute(
        &self,
        state: &AppState,
        code: String,
    ) -> std::result::Result<Value, RpcError> {
        let _guard = self.exec_lock.lock().await;
        let exec_id = self.next_exec_id.fetch_add(1, Ordering::Relaxed);
        self.write_control(json!({
            "type": "exec",
            "id": exec_id,
            "code": code,
        }))
        .await?;

        loop {
            let message = self.read_control().await?;
            let message_type = message.get("type").and_then(Value::as_str).unwrap_or("");
            match message_type {
                "host_call" => {
                    let call_id = message.get("id").and_then(Value::as_u64).unwrap_or(0);
                    let method = message
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
                    let result =
                        handle_host_call(state, &self.parent_session_id, &method, params).await;
                    match result {
                        Ok(result) => {
                            self.write_control(json!({
                                "type": "host_result",
                                "id": call_id,
                                "ok": true,
                                "result": result,
                            }))
                            .await?;
                        }
                        Err(error) => {
                            self.write_control(json!({
                                "type": "host_result",
                                "id": call_id,
                                "ok": false,
                                "error": {
                                    "code": error.code,
                                    "message": error.message,
                                    "data": error.data,
                                },
                            }))
                            .await?;
                        }
                    }
                }
                "exec_result" => {
                    if message.get("id").and_then(Value::as_u64) != Some(exec_id) {
                        return Err(RpcError::new(
                            "repl_protocol_error",
                            "received exec_result for a different request",
                        ));
                    }
                    return Ok(message);
                }
                other => {
                    return Err(RpcError::new(
                        "repl_protocol_error",
                        format!("unexpected Python REPL message type: {other}"),
                    ));
                }
            }
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
        .execute(state, &session_id, params.code, params.timeout_ms)
        .await
}

async fn handle_host_call(
    state: &AppState,
    parent_session_id: &str,
    method: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    match method {
        "subagents.call" => subagents_call(state, parent_session_id, params).await,
        "subagents.call_bulk" => subagents_call_bulk(state, parent_session_id, params).await,
        "subagents.list" => subagents_list_host(state, parent_session_id, params).await,
        "subagents.read" => subagents_read_host(state, parent_session_id, params).await,
        "subagents.steer" => subagents_steer_host(state, parent_session_id, params).await,
        "subagents.interrupt" => subagents_interrupt_host(state, parent_session_id, params).await,
        _ => Err(RpcError::new(
            "unknown_host_call",
            format!("unknown REPL host call: {method}"),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct SubagentCallParams {
    role: String,
    message: String,
    #[serde(default)]
    fork_context: bool,
    role_workspace: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct SubagentCallSpec {
    role: String,
    message: String,
    #[serde(default)]
    fork_context: bool,
    role_workspace: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubagentCallBulkParams {
    calls: Vec<SubagentCallSpec>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct SpawnedCall {
    child_session_id: String,
    role: String,
}

async fn subagents_call(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: SubagentCallParams = from_params(params)?;
    let timeout_ms = params.timeout_ms.unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_MS);
    let spawned = spawn_call(
        state,
        parent_session_id,
        SubagentCallSpec {
            role: params.role,
            message: params.message,
            fork_context: params.fork_context,
            role_workspace: params.role_workspace,
        },
    )
    .await?;
    wait_for_children_idle(state, &[spawned.child_session_id.clone()], timeout_ms).await?;
    call_result(state, spawned).await
}

async fn subagents_call_bulk(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: SubagentCallBulkParams = from_params(params)?;
    if params.calls.is_empty() {
        return Err(RpcError::new("invalid_params", "calls cannot be empty"));
    }
    let timeout_ms = params.timeout_ms.unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_MS);
    let mut spawned = Vec::with_capacity(params.calls.len());
    for call in params.calls {
        spawned.push(spawn_call(state, parent_session_id, call).await?);
    }
    let child_session_ids = spawned
        .iter()
        .map(|call| call.child_session_id.clone())
        .collect::<Vec<_>>();
    wait_for_children_idle(state, &child_session_ids, timeout_ms).await?;
    let mut results = Vec::with_capacity(spawned.len());
    for call in spawned {
        results.push(call_result(state, call).await?);
    }
    Ok(json!(results))
}

async fn spawn_call(
    state: &AppState,
    parent_session_id: &str,
    spec: SubagentCallSpec,
) -> std::result::Result<SpawnedCall, RpcError> {
    let role = spec.role.trim().to_string();
    let message = spec.message.trim().to_string();
    if role.is_empty() {
        return Err(RpcError::new("invalid_params", "role cannot be empty"));
    }
    if message.is_empty() {
        return Err(RpcError::new("invalid_params", "message cannot be empty"));
    }
    let initial_context = if spec.fork_context {
        Some(parent_context_block(state, parent_session_id).await?)
    } else {
        None
    };
    let mut params = json!({
        "parent_session_id": parent_session_id,
        "role": role,
        "task": message,
    });
    if let Some(role_workspace) = spec
        .role_workspace
        .map(|workspace| workspace.trim().to_string())
        .filter(|workspace| !workspace.is_empty())
    {
        params["role_workspace"] = json!(role_workspace);
    }
    if let Some(initial_context) = initial_context {
        params["initial_context"] = json!(initial_context);
    }
    let spawned = subagent_spawn(state, params).await?;
    let child_session_id = spawned
        .get("child_session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("internal_error", "subagent.spawn omitted child_session_id"))?
        .to_string();
    Ok(SpawnedCall {
        child_session_id,
        role,
    })
}

async fn wait_for_children_idle(
    state: &AppState,
    child_session_ids: &[String],
    timeout_ms: u64,
) -> std::result::Result<(), RpcError> {
    let started = Instant::now();
    let timeout_duration = Duration::from_millis(timeout_ms);
    loop {
        let mut all_idle = true;
        for child_session_id in child_session_ids {
            let activity = state
                .repo
                .activity(child_session_id)
                .await
                .map_err(anyhow::Error::from)?;
            if activity != SessionActivity::Idle {
                all_idle = false;
                break;
            }
        }
        if all_idle {
            return Ok(());
        }
        if started.elapsed() >= timeout_duration {
            return Err(RpcError::new(
                "subagent_timeout",
                format!("subagent call exceeded {timeout_ms}ms"),
            ));
        }
        sleep(Duration::from_millis(SUBAGENT_POLL_INTERVAL_MS)).await;
    }
}

async fn call_result(
    state: &AppState,
    spawned: SpawnedCall,
) -> std::result::Result<Value, RpcError> {
    let turns = state
        .repo
        .transcript_turns(&spawned.child_session_id, None, Some(20))
        .await
        .map_err(anyhow::Error::from)?;
    let text = latest_assistant_text(&turns.cards);
    let activity = state
        .repo
        .activity(&spawned.child_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(json!({
        "session_id": spawned.child_session_id,
        "role": spawned.role,
        "activity": activity,
        "text": text,
        "transcript": rpc_views::transcript_turns(turns),
    }))
}

fn latest_assistant_text(cards: &[agent_store::TurnCardRecord]) -> String {
    cards
        .iter()
        .rev()
        .filter_map(|card| card.assistant_message.as_ref())
        .find_map(|entry| match &entry.item {
            TranscriptItem::AssistantMessage(message) => Some(message.text()),
            _ => None,
        })
        .unwrap_or_default()
}

async fn parent_context_block(
    state: &AppState,
    parent_session_id: &str,
) -> std::result::Result<String, RpcError> {
    let driver = SessionDriver::acquire(state, parent_session_id).await;
    driver.recover_if_needed().await?;
    let history = state
        .repo
        .active_branch(parent_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let mut rendered = format!("Parent session `{parent_session_id}` active context:\n\n");
    for entry in history.entries {
        let line = transcript_item_context_line(&entry.item);
        if line.is_empty() {
            continue;
        }
        if rendered.len() + line.len() + 2 > PARENT_CONTEXT_MAX_CHARS {
            rendered.push_str("\n[Parent context truncated]\n");
            break;
        }
        rendered.push_str(&line);
        rendered.push_str("\n\n");
    }
    Ok(rendered)
}

fn transcript_item_context_line(item: &TranscriptItem) -> String {
    match item {
        TranscriptItem::UserMessage(message) => {
            let text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    agent_vocab::ContentBlock::Text { text } => Some(text.as_str()),
                    agent_vocab::ContentBlock::Image { .. } => Some("[image]"),
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = text.trim();
            if text.is_empty() {
                String::new()
            } else {
                format!("User:\n{text}")
            }
        }
        TranscriptItem::AssistantMessage(message) => {
            let text = message.text();
            if text.trim().is_empty() {
                String::new()
            } else {
                format!("Assistant:\n{}", text.trim())
            }
        }
        TranscriptItem::CompactionSummary(summary) => {
            format!("Compaction summary:\n{}", summary.summary.trim())
        }
        _ => String::new(),
    }
}

#[derive(Debug, Deserialize)]
struct SubagentsListParams {
    parent_session_id: Option<String>,
}

async fn subagents_list_host(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: SubagentsListParams = from_params(params)?;
    let parent = params
        .parent_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(parent_session_id);
    if parent != parent_session_id {
        return Err(RpcError::new(
            "subagent_scope",
            "REPL subagent listing is scoped to the current parent session",
        ));
    }
    subagent_list(state, json!({ "parent_session_id": parent })).await
}

async fn subagents_read_host(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let child_session_id = required_string(&params, "session_id")?;
    require_known_subagent(state, parent_session_id, &child_session_id).await?;
    let turns = state
        .repo
        .transcript_turns(&child_session_id, None, Some(20))
        .await
        .map_err(anyhow::Error::from)?;
    Ok(rpc_views::transcript_turns(turns))
}

async fn subagents_steer_host(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let child_session_id = required_string(&params, "session_id")?;
    let message = required_string(&params, "message")?;
    require_known_subagent(state, parent_session_id, &child_session_id).await?;
    enqueue_session_input(
        state,
        SessionInputRequest {
            session_id: child_session_id,
            priority: agent_store::InputPriority::Steer,
            content: UserMessage::text(message),
            client_input_id: None,
            base_leaf_id: None,
            expected_active_leaf_id: None,
        },
    )
    .await
}

async fn subagents_interrupt_host(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let child_session_id = required_string(&params, "session_id")?;
    require_known_subagent(state, parent_session_id, &child_session_id).await?;
    interrupt_session(state, &child_session_id).await
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
_HOST_CALL_ID = 0


def _write_control(message):
    _CONTROL_OUT.write(json.dumps(message, separators=(",", ":")) + "\n")
    _CONTROL_OUT.flush()


def _read_control():
    line = _CONTROL_IN.readline()
    if not line:
        raise RuntimeError("host closed REPL stdin")
    return json.loads(line)


def _host_call(method, params):
    global _HOST_CALL_ID
    _HOST_CALL_ID += 1
    call_id = _HOST_CALL_ID
    _write_control({
        "type": "host_call",
        "id": call_id,
        "method": method,
        "params": params,
    })
    while True:
        message = _read_control()
        if message.get("type") == "host_result" and message.get("id") == call_id:
            if message.get("ok"):
                return message.get("result")
            error = message.get("error") or {}
            raise RuntimeError(f"{error.get('code', 'host_error')}: {error.get('message', '')}")
        raise RuntimeError(f"unexpected host response: {message!r}")


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


class SubagentResult:
    def __init__(self, data):
        self._data = dict(data or {})
        self.session_id = self._data.get("session_id")
        self.role = self._data.get("role")
        self.activity = self._data.get("activity")
        self.text = self._data.get("text") or ""
        self.transcript = self._data.get("transcript")

    def to_dict(self):
        return dict(self._data)

    def __getitem__(self, key):
        return self._data[key]

    def __repr__(self):
        preview = self.text.replace("\n", " ")[:80]
        return f"SubagentResult(session_id={self.session_id!r}, role={self.role!r}, text={preview!r})"


class SubagentHandle:
    def __init__(self, session_id):
        self.session_id = session_id

    @property
    def transcript(self):
        return _host_call("subagents.read", {"session_id": self.session_id})

    def steer(self, message):
        return _host_call("subagents.steer", {
            "session_id": self.session_id,
            "message": message,
        })

    def interrupt(self):
        return _host_call("subagents.interrupt", {"session_id": self.session_id})

    def __repr__(self):
        return f"SubagentHandle({self.session_id!r})"


class Subagents:
    def call(self, role, message, fork_context=False, timeout=None, role_workspace=None):
        params = {
            "role": role,
            "message": message,
            "fork_context": bool(fork_context),
        }
        if timeout is not None:
            params["timeout_ms"] = int(float(timeout) * 1000)
        if role_workspace is not None:
            params["role_workspace"] = role_workspace
        return SubagentResult(_host_call("subagents.call", params))

    def call_bulk(self, calls, timeout=None):
        normalized = []
        for call in calls:
            item = dict(call)
            if "timeout" in item and "timeout_ms" not in item:
                item["timeout_ms"] = int(float(item.pop("timeout")) * 1000)
            normalized.append(item)
        params = {"calls": normalized}
        if timeout is not None:
            params["timeout_ms"] = int(float(timeout) * 1000)
        return [SubagentResult(item) for item in _host_call("subagents.call_bulk", params)]

    def list(self, parent_session_id=None):
        return _host_call("subagents.list", {"parent_session_id": parent_session_id})

    def __getitem__(self, session_id):
        return SubagentHandle(session_id)


subagents = Subagents()


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
    "subagents": subagents,
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

        let repl = PythonRepl::start("parent".to_string())
            .await
            .expect("start python repl");
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
