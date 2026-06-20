use agent_store::SessionActivity;
use agent_vocab::{TranscriptItem, TurnOutcome, UserMessage};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout, Duration};

use crate::codec::{from_params, required_string};
use crate::rpc_views;
use crate::runtime::{session_has_live_tasks, SessionDriver};
use crate::state::AppState;
use crate::subagents::{require_known_subagent, subagent_list, subagent_spawn_from_active_parent};
use crate::types::RpcError;
use crate::{enqueue_session_input, interrupt_session, SessionInputRequest};

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
        let run = repl.execute(state, code);
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
        "subagents.spawn" => subagents_spawn_host(state, parent_session_id, params).await,
        "subagents.wait" => subagents_wait_host(state, parent_session_id, params).await,
        "subagents.call" => subagents_call(state, parent_session_id, params).await,
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
    #[serde(default)]
    sources: Vec<Value>,
}

#[derive(Debug, Clone)]
struct SubagentCallSpec {
    role: String,
    message: String,
    fork_context: bool,
    role_workspace: Option<String>,
    sources: Vec<Value>,
}

#[derive(Debug, Clone)]
struct SpawnedCall {
    child_session_id: String,
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubagentWaitTarget {
    session_id: String,
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubagentWaitParams {
    targets: Vec<SubagentWaitTarget>,
}

async fn subagents_spawn_host(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: SubagentCallParams = from_params(params)?;
    let spawned = spawn_call(
        state,
        parent_session_id,
        SubagentCallSpec {
            role: params.role,
            message: params.message,
            fork_context: params.fork_context,
            role_workspace: params.role_workspace,
            sources: params.sources,
        },
    )
    .await?;
    spawned_handle_value(state, spawned).await
}

async fn subagents_wait_host(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: SubagentWaitParams = from_params(params)?;
    if params.targets.is_empty() {
        return Err(RpcError::new("invalid_params", "targets cannot be empty"));
    }
    let mut targets = Vec::with_capacity(params.targets.len());
    for target in params.targets {
        let child_session_id = target.session_id.trim().to_string();
        if child_session_id.is_empty() {
            return Err(RpcError::new(
                "invalid_params",
                "session_id cannot be empty",
            ));
        }
        require_known_subagent(state, parent_session_id, &child_session_id).await?;
        targets.push(SpawnedCall {
            child_session_id,
            role: target
                .role
                .map(|role| role.trim().to_string())
                .filter(|role| !role.is_empty()),
        });
    }
    let child_session_ids = targets
        .iter()
        .map(|target| target.child_session_id.clone())
        .collect::<Vec<_>>();
    wait_for_children_idle(state, &child_session_ids).await?;
    let mut results = Vec::with_capacity(targets.len());
    for target in targets {
        results.push(call_result(state, target).await?);
    }
    Ok(json!(results))
}

async fn subagents_call(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: SubagentCallParams = from_params(params)?;
    let spawned = spawn_call(
        state,
        parent_session_id,
        SubagentCallSpec {
            role: params.role,
            message: params.message,
            fork_context: params.fork_context,
            role_workspace: params.role_workspace,
            sources: params.sources,
        },
    )
    .await?;
    wait_for_children_idle(state, &[spawned.child_session_id.clone()]).await?;
    call_result(state, spawned).await
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
    if !spec.sources.is_empty() {
        params["sources"] = json!(spec.sources);
    }
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
    let spawned = subagent_spawn_from_active_parent(state, params).await?;
    let child_session_id = spawned
        .get("child_session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("internal_error", "subagent.spawn omitted child_session_id"))?
        .to_string();
    Ok(SpawnedCall {
        child_session_id,
        role: Some(role),
    })
}

async fn spawned_handle_value(
    state: &AppState,
    spawned: SpawnedCall,
) -> std::result::Result<Value, RpcError> {
    let activity = state
        .repo
        .activity(&spawned.child_session_id)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(json!({
        "session_id": spawned.child_session_id,
        "role": spawned.role,
        "activity": activity,
    }))
}

async fn wait_for_children_idle(
    state: &AppState,
    child_session_ids: &[String],
) -> std::result::Result<(), RpcError> {
    loop {
        let mut all_idle = true;
        for child_session_id in child_session_ids {
            let driver = SessionDriver::acquire(state, child_session_id).await;
            driver.recover_if_needed().await?;
            let mut activity = state
                .repo
                .activity(child_session_id)
                .await
                .map_err(anyhow::Error::from)?;
            if activity != SessionActivity::Idle && !session_has_live_tasks(state, child_session_id)
            {
                driver.drive_until_blocked().await?;
                activity = state
                    .repo
                    .activity(child_session_id)
                    .await
                    .map_err(anyhow::Error::from)?;
            }
            if activity == SessionActivity::Idle {
                driver.notify_subagent_parent_idle_if_needed().await;
            }
            if activity != SessionActivity::Idle {
                all_idle = false;
                break;
            }
        }
        if all_idle {
            return Ok(());
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
    if let Some(card) = turns.cards.iter().rev().find(|card| card.outcome.is_some()) {
        if matches!(
            card.outcome,
            Some(TurnOutcome::Crashed | TurnOutcome::Interrupted)
        ) {
            let code = match card.outcome {
                Some(TurnOutcome::Interrupted) => "subagent_interrupted",
                _ => "subagent_crashed",
            };
            let label = match card.outcome {
                Some(TurnOutcome::Interrupted) => "was interrupted",
                _ => "crashed",
            };
            return Err(RpcError::new(
                code,
                format!(
                    "subagent {} {} in latest terminal turn {}",
                    spawned.child_session_id,
                    label,
                    card.turn_id
                        .map(|turn_id| turn_id.0.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                ),
            ));
        }
    }
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

/// A read-only subagent is fire-and-forget: it runs in a disposable snapshot
/// and cannot be steered or interrupted individually (only a whole stage can be
/// cancelled). Reject either operation for a read-only child.
async fn reject_read_only_control(
    state: &AppState,
    child_session_id: &str,
    operation: &str,
) -> std::result::Result<(), RpcError> {
    if state
        .repo
        .session_subagent_type(child_session_id)
        .await
        .map_err(anyhow::Error::from)?
        == Some(agent_store::SubagentType::ReadOnly)
    {
        return Err(RpcError::new(
            "read_only_subagent",
            format!("a read-only subagent cannot be {operation}; cancel the stage instead"),
        ));
    }
    Ok(())
}

async fn subagents_steer_host(
    state: &AppState,
    parent_session_id: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let child_session_id = required_string(&params, "session_id")?;
    let message = required_string(&params, "message")?;
    require_known_subagent(state, parent_session_id, &child_session_id).await?;
    reject_read_only_control(state, &child_session_id, "steered").await?;
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
    reject_read_only_control(state, &child_session_id, "interrupted").await?;
    interrupt_session(state, &child_session_id).await
}

const PYTHON_REPL_BOOTSTRAP: &str = r#"
import ast
import contextlib
import io
import json
import sys
import traceback
import types

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
    def __init__(self, session_id, role=None, activity=None):
        self.session_id = session_id
        self.role = role
        self.activity = activity

    def to_dict(self):
        return {
            "session_id": self.session_id,
            "role": self.role,
            "activity": self.activity,
        }

    def wait(self):
        return subagents.wait(self)

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
        return f"SubagentHandle(session_id={self.session_id!r}, role={self.role!r}, activity={self.activity!r})"


def _source_id(source):
    if isinstance(source, SubagentResult) or isinstance(source, SubagentHandle):
        return source.session_id
    if isinstance(source, dict):
        return source.get("session_id") or source.get("child_session_id")
    return source


def _wait_target(target):
    if isinstance(target, SubagentResult) or isinstance(target, SubagentHandle):
        return {"session_id": target.session_id, "role": getattr(target, "role", None)}
    if isinstance(target, dict):
        session_id = target.get("session_id") or target.get("child_session_id")
        return {"session_id": session_id, "role": target.get("role")}
    return {"session_id": target, "role": None}


def _spawn_params(role, message, fork_context=False, role_workspace=None, sources=None):
    params = {
        "role": role,
        "message": message,
        "fork_context": bool(fork_context),
    }
    if role_workspace is not None:
        params["role_workspace"] = role_workspace
    if sources is not None:
        params["sources"] = [_source_id(source) for source in sources]
    return params


class Subagents:
    def spawn(self, role, message, fork_context=False, role_workspace=None, sources=None):
        data = _host_call(
            "subagents.spawn",
            _spawn_params(role, message, fork_context, role_workspace, sources),
        )
        return SubagentHandle(
            data.get("session_id"),
            role=data.get("role"),
            activity=data.get("activity"),
        )

    def wait(self, targets):
        single = not isinstance(targets, (list, tuple))
        target_list = [targets] if single else list(targets)
        results = [SubagentResult(item) for item in _host_call(
            "subagents.wait",
            {"targets": [_wait_target(target) for target in target_list]},
        )]
        return results[0] if single else results

    def call(self, role, message, fork_context=False, role_workspace=None, sources=None):
        return self.spawn(role, message, fork_context, role_workspace, sources).wait()

    def list(self, parent_session_id=None):
        response = _host_call("subagents.list", {"parent_session_id": parent_session_id})
        items = response.get("subagents") if isinstance(response, dict) else None
        handles = []
        for item in items or []:
            handles.append(SubagentHandle(
                item.get("child_session_id") or item.get("session_id"),
                role=item.get("role") or item.get("role_name"),
                activity=item.get("activity"),
            ))
        return handles

    def steer(self, session_id, message):
        return _host_call("subagents.steer", {
            "session_id": _source_id(session_id),
            "message": message,
        })

    def interrupt(self, session_id):
        return _host_call("subagents.interrupt", {"session_id": _source_id(session_id)})

    def __getitem__(self, session_id):
        return SubagentHandle(session_id)


subagents = Subagents()


class _SubagentsModule(types.ModuleType):
    def __getitem__(self, session_id):
        return subagents[session_id]


_subagents_module = _SubagentsModule("subagents")
_subagents_module.spawn = subagents.spawn
_subagents_module.wait = subagents.wait
_subagents_module.call = subagents.call
_subagents_module.list = subagents.list
_subagents_module.steer = subagents.steer
_subagents_module.interrupt = subagents.interrupt
_subagents_module.SubagentResult = SubagentResult
_subagents_module.SubagentHandle = SubagentHandle
_subagents_module.subagents = subagents
sys.modules["subagents"] = _subagents_module


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

        repl.write_control(json!({
            "type": "exec",
            "id": 3,
            "code": "import subagents\nprint(hasattr(subagents, 'spawn'), hasattr(subagents, 'spawn_bulk'), hasattr(subagents, 'wait'), hasattr(subagents, 'call'), hasattr(subagents, 'call_bulk'), hasattr(subagents, 'steer'), hasattr(subagents, 'interrupt'), hasattr(subagents, 'list'))\nsubagents['child']",
        }))
        .await
        .expect("write import exec");
        let imported = repl.read_control().await.expect("read import result");
        assert_eq!(imported["type"], "exec_result");
        assert_eq!(imported["id"], 3);
        assert_eq!(imported["ok"], true);
        assert_eq!(
            imported["stdout"],
            "True False True True False True True True\n"
        );
        assert!(imported["result_repr"]
            .as_str()
            .unwrap_or_default()
            .contains("SubagentHandle"));
        assert_eq!(imported["result_json"]["session_id"], "child");
        assert!(imported["result_json"].get("role").is_some());
        assert!(imported["result_json"].get("activity").is_some());

        repl.kill().await;
    }
}
