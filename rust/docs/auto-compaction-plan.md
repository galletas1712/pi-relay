# Rust/Web Auto-Compaction Plan

This document scopes compaction work to the **Rust daemon/provider/session/store
stack** and the **React web UI**. It intentionally does not cover the older
TypeScript coding-agent or `packages/ai` implementation.

The core rule: **provider-native compaction is replacement history, not just a
better summary.** OpenAI remote compaction must preserve opaque provider state;
Anthropic should continue to use local text-summary compaction until/unless it
gets a true provider-native compaction endpoint.

The plan is intentionally phased. Phase 1 makes manual compaction correct for
provider-native replay and storage; Phase 3 adds automatic triggering after the
manual path is stable.

## Goals

- Add robust manual and automatic compaction for Rust sessions.
- Use OpenAI/Codex `/responses/compact` for OpenAI sessions by default.
- Keep Anthropic compaction functional via local summary generation.
- Persist OpenAI remote compaction state so later OpenAI calls can replay it.
- Preserve cross-provider fallback behavior with text summaries.
- Avoid stale in-memory sessions, duplicate history, queued-input loss, and
  auto-compaction loops.
- Keep provider-native opaque payloads out of UI/export surfaces; they are
  daemon/provider replay state, not display data.
- Keep the web UI simple: request compaction, show status, display compacted
  history, never inspect provider-native opaque payloads.

## Non-goals

- No changes to `packages/ai`, `packages/coding-agent`, or the old TS agent path.
- No mid-turn provider-native compaction in the first implementation.
- No web UI rendering or exporting of OpenAI encrypted compaction state.
- No provider switching after transcript creation beyond the existing locked
  behavior.
- No assumption that OpenAI remote compaction can rely on server-retained state
  unless that is explicitly tested with the current `store: false` request mode.

## Relevant current architecture

- `agent-provider`
  - `ModelProvider::complete` is the only provider operation today.
  - OpenAI provider uses ChatGPT/Codex transport at:
    `https://chatgpt.com/backend-api/codex/responses`.
  - Anthropic provider uses `/v1/messages`.
- `agent-session`
  - Owns deterministic session history shape through `TranscriptStore`.
  - `TranscriptItem::CompactionSummary` already exists and is treated as a turn
    boundary/root-capable item.
- `agent-store`
  - Persists transcript entries with a `provider_replay jsonb` sidecar.
  - Manual compaction already writes a new root entry with
    `TranscriptItem::CompactionSummary`.
- `agent-daemon`
  - Orchestrates model/tool/compaction actions.
  - Current compaction is local-summary only.
  - Startup already marks unfinished actions stale; this remains the first
    implementation's compaction recovery policy.
- `packages/web`
  - Already has `/compact`, `compaction.request`, compaction activity events, and
    `compaction_summary` display.

## Design summary

Use a shared daemon compaction orchestration path that returns a structured
result:

```rust
pub(crate) enum CompactionSummaryKind {
    ProviderText,
    Generic,
}

pub(crate) struct CompactionOutput {
    pub summary: String,
    pub summary_kind: CompactionSummaryKind,
    pub provider_replay: Vec<ProviderReplayItem>,
    pub remote: bool,
    pub provider: ProviderKind,
    pub usage: Option<Value>,
}
```

Notes:

- `CompactionOutput` is daemon-local orchestration data.
- `agent-store` should accept a small store-facing completion type, not depend on
  a daemon-private type.
- `usage` is stored as JSON metadata if present; `agent-store` should not depend
  on `agent-provider`'s concrete `ProviderUsage` type.

Provider behavior:

- **OpenAI**
  - Prefer remote `/responses/compact`.
  - Store the returned replacement history in `provider_replay` on the compacted
    root.
  - Use `summary` as UI/export/fallback text only.
  - If remote compaction returns encrypted state but no usable text summary,
    create a generic summary string and mark `summary_kind = Generic`.
- **Anthropic**
  - Use local summary generation through `/v1/messages`.
  - Store the text summary in `TranscriptItem::CompactionSummary`.
  - Store `provider_replay: Vec::new()` for the compaction root.

The durable compacted root should be:

```rust
StoredTranscriptEntry {
    id: new_root_id,
    parent_id: None,
    timestamp_ms: now_ms(),
    item: TranscriptItem::CompactionSummary(...),
    provider_replay,
}
```

This uses the existing storage shape and avoids a new transcript item type.

Important visibility split:

- Provider replay on a compaction root is loaded by the Rust provider renderer.
- RPC/web history views should redact compaction-root `provider_replay` before
  sending entries to the browser.
- Assistant-message replay can still be sent to the browser for existing hosted
  tool/citation display, but compaction replay is opaque provider state.

## Provider API changes

Add provider-level remote-compaction support in
`rust/crates/agent-provider/src/lib.rs`.

Do **not** reuse `ModelRequest` for provider-native compaction. `ModelRequest`
contains completion-specific concerns such as max output tokens, prompt-cache
routing, dynamic context, and streaming assumptions. Remote compaction has a
separate request contract.

```rust
#[derive(Debug, Clone)]
pub struct ProviderCompactionRequest {
    pub model: String,
    pub instructions: Option<String>,
    pub transcript: Vec<ModelTranscriptEntry>,
    pub tool_profile: ProviderToolProfile,
    pub tools: Vec<ToolDefinition>,
    pub reasoning_effort: ReasoningEffort,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderCompactionResponse {
    /// Provider-returned text, if the provider exposes one. OpenAI remote
    /// compaction may return only opaque encrypted state.
    pub summary: Option<String>,
    pub provider_replay: Vec<ProviderReplayItem>,
    pub usage: Option<ProviderUsage>,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse>;

    fn supports_remote_compaction(&self) -> bool {
        false
    }

    async fn compact(
        &self,
        _request: ProviderCompactionRequest,
    ) -> ProviderResult<ProviderCompactionResponse> {
        Err(ProviderError::Provider(
            "provider does not support remote compaction".to_string(),
        ))
    }
}
```

OpenAI overrides both remote-compaction methods. Anthropic keeps the defaults.
Local summary compaction, including OpenAI fallback, continues to use
`complete(ModelRequest)` through the daemon's local-summary path.

### Provider rendering boundary

Keep the compaction rendering paths provider-specific:

- `ProviderCompactionRequest` is for true provider-native replacement-history
  endpoints. In the first implementation that means OpenAI only.
- Local summary compaction uses ordinary `ModelRequest` rendering for the
  selected provider.
  - OpenAI local fallback renders to Responses items through the OpenAI provider.
  - Anthropic local compaction renders to Claude `/v1/messages` through the
    Anthropic provider.
- Do not pass OpenAI Responses `ResponseItem` filtering rules, compact output,
  or `ProviderCompactionRequest` through the Anthropic path.
- Do not store local-summary call provider replay on the compaction root for any
  provider. Local summaries are future conversation text, not provider-native
  replacement state.

This boundary is important for the Claude backend: Anthropic's Messages API has
strict message/tool structure requirements, so local trimming and replay must be
validated in Anthropic message space, not in OpenAI Responses-item space.

## OpenAI remote compaction

### Request

Implement in `rust/crates/agent-provider/src/openai.rs`.

Endpoint:

```text
POST {base_url}/responses/compact
```

For current Codex base URL:

```text
https://chatgpt.com/backend-api/codex/responses/compact
```

Body shape should match Codex, but the `input` semantics must be explicit.
Illustrative shape:

```json
{
  "model": "...",
  "input": ["<rendered transcript item>", "..."],
  "instructions": "...",
  "tools": [],
  "parallel_tool_calls": true,
  "reasoning": {
    "effort": "xhigh"
  }
}
```

Rules:

- Use the same Codex identity/auth envelope as normal OpenAI model calls.
- Use the **same session id** as the parent session for remote compaction.
- Do **not** suffix the session id with `:compaction` for remote OpenAI
  compaction.
- Do **not** include `stream: true`.
- Do **not** include dynamic working-directory context in compact input.
  - The next normal model request will inject dynamic context again.
  - Including it in compact input risks preserving stale wrapper messages.
- Do not assume server-side history retention for compaction while normal
  OpenAI calls use `store: false`.
  - First implementation should render the current transcript into `input`,
    excluding dynamic context.
  - `input: []` is allowed only behind an explicit, tested implementation path
    proving that `/responses/compact` compacts server-side session state with
    the current transport/settings.
- It is acceptable to include tools and reasoning controls.
- Avoid prompt-cache-only fields unless deliberately tested for this endpoint.
- Do not include dynamic text/output schema controls unless deliberately
  supported; Codex can include optional `text`, but first implementation may
  omit it.
- `instructions` should come from the request's stable instructions. Do not
  inject the cwd/dynamic context into `instructions`.
- Codex's canonical compact payload contains `model`, `input`, `instructions`,
  `tools`, `parallel_tool_calls`, optional `reasoning`, and optional `text`.
  It does not include `stream`, `store`, `prompt_cache_key`, `service_tier`, or
  `include`.

For first implementation, prefer a compact body helper separate from the normal
streaming response body:

```rust
fn compact_body(request: ProviderCompactionRequest) -> ProviderResult<Value> {
    Ok(json!({
        "model": request.model,
        "instructions": request.instructions.unwrap_or_default(),
        "input": compact_input_items(&request.transcript)?,
        "tools": response_tools(request.tool_profile, &request.tools)?,
        "parallel_tool_calls": true,
        "reasoning": { "effort": openai_reasoning_effort(request.reasoning_effort)? },
    }))
}
```

If `text_controls` is later added to `ProviderCompactionRequest`, include it as
Codex does. Until then, omit `text` entirely rather than sending `null`.

### Response

Expected shape:

```json
{
  "output": [
    {
      "type": "compaction",
      "encrypted_content": "..."
    }
  ]
}
```

Codex's compact client parses the unary response's `output` array as the new
replacement history. The official API response also includes `usage`; parse and
store it as action metadata when available, but do not require it for success.

Preserve each kept output item exactly as `ProviderReplayItem.raw_json`.

Filtering should be conservative and Codex-aligned for the first implementation.

Keep:

- `type: "compaction"`
- `type: "message"` with `role: "assistant"`
- `type: "message"` with `role: "user"` only if it parses as a real user
  message or persisted hook prompt

Drop:

- user messages that are synthetic/session wrapper content, unless/until a
  verified parser says they are real user content
- `role: "developer"` messages
- synthetic/wrapper messages
- function/tool calls
- function/tool outputs
- reasoning items
- web/search/image call artifacts
- unknown operational artifacts

These filters apply only to OpenAI remote compact output. They must not be reused
for Claude local-summary compaction; Claude trimming must operate on complete
Anthropic message/tool groups.

Remote compaction success criteria:

- At least one kept item must be `type: "compaction"`.
- Empty output, invalid JSON, no kept output, or no kept compaction item is a
  remote-compaction failure.
- In `Auto` mode, that failure falls back to local summary compaction.
- In `Always` mode, that failure is reported to the user/action.

Summary handling:

- If the response includes usable text, return it as `summary: Some(text)`.
- If the response only includes opaque compaction state, return `summary: None`.
- The daemon converts `None` into a generic UI/export fallback summary, for
  example:

```text
Conversation history before this point was compacted using OpenAI provider-native compaction.
```

### OpenAI transcript replay after compaction

Update OpenAI transcript rendering:

```rust
TranscriptItem::CompactionSummary(summary) => {
    let replay_items = openai_replay_items(&entry.provider_replay)?;
    if !replay_items.is_empty() {
        responses.extend(replay_items);
    } else {
        responses.push(json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": compaction_summary_text(summary)
            }],
        }));
    }
}
```

This is the crucial behavior: OpenAI should consume remote compacted provider
state when present, not the fallback summary.

Dynamic context placement after OpenAI compaction should follow Codex's pre-turn
behavior:

- Do not compact dynamic cwd/session wrapper context into the replacement
  history.
- On the next normal OpenAI request, replay the compaction-root provider items
  first, then inject fresh dynamic context before new post-compaction user
  content.
- In the full Responses `input` array, dynamic context may appear before the
  compacted branch because OpenAI request assembly currently prepends dynamic
  context to every request. The important invariant is that dynamic context is
  not persisted inside the compaction root replay itself.
- Anthropic is different: its dynamic context remains normal system context for
  the local-summary path, and the compaction root is replayed as a user text
  summary in Claude messages.

### Local fallback after OpenAI remote failure

When remote compaction falls back to local summary in `Auto` mode:

- Use the existing local summary flow through `complete(ModelRequest)`.
- Use the local-summary session id convention: `session_id:compaction`.
- Isolate prompt-cache keys as the current local compaction path does.
- Store `provider_replay: Vec::new()` on the compaction root.

Remote and local compaction intentionally have different session-id rules.

## Anthropic compaction path

Anthropic should remain local text-summary compaction.

### Generation

Use the local-summary flow:

1. Build a normal `ModelRequest` for the Anthropic provider.
2. Render transcript as Anthropic-compatible `/v1/messages` messages using the
   existing Anthropic provider renderer.
3. Append the compaction user prompt:
   `Summarize the transcript above into a compact continuation context.`
4. Use `ProviderToolProfile::None` and `tools: Vec::new()` for the summary call.
5. Put the compaction instructions in normal Anthropic system content via
   `PromptSections`, not in OpenAI-style Responses instructions.
6. Call Anthropic `/v1/messages` through `complete(ModelRequest)`.
7. Extract assistant text.
8. Store only that summary in `CompactionSummary`.
9. Store `provider_replay: Vec::new()` on the compaction root.

Do **not** store the Anthropic summary call's raw provider response as the
compaction root replay. That replay belongs to the summarization call, not the
future conversation state.

The Anthropic path must not construct or consume `ProviderCompactionRequest`.
`RemoteCompactionMode::Always` should fail clearly for Anthropic before any
provider call is attempted.

### Replay after compaction

Anthropic transcript rendering should continue to use text summary:

```rust
TranscriptItem::CompactionSummary(summary) => {
    messages.push(json!({
        "role": "user",
        "content": [{
            "type": "text",
            "text": compaction_summary_text(summary)
        }],
    }));
}
```

Anthropic should ignore OpenAI `provider_replay` on compaction roots. More
generally, Anthropic transcript rendering should use only Claude replay records
on assistant entries and plain summary text for compaction roots.

### Anthropic-specific edge cases

- Local compaction can itself exceed context limits.
- Trimming/retry must preserve valid Claude message/tool structure.
- Trim complete turn/tool groups, not arbitrary individual messages.
- Avoid creating `tool_result` blocks without corresponding prior `tool_use`.
- Avoid preserving assistant `tool_use` blocks whose result has been trimmed.
- Tool output limiting should run before local compaction.
- Prompt caching should naturally work after compaction because the summary is
  normal transcript text.
- Do not trim a Claude assistant message containing `tool_use` unless the
  corresponding `tool_result` user block is trimmed with it.
- Do not preserve a Claude `tool_result` block if the matching assistant
  `tool_use` block was trimmed.

## Compaction settings

Use a dedicated compaction configuration rather than adding policy knobs directly
to `ProviderConfig`.

`ProviderConfig` should describe model/provider call behavior. Compaction policy
is daemon/session orchestration behavior, and changing an auto-compaction
threshold should not look like changing the provider/model identity.

Persist the config in `sessions.metadata["compaction"]` for the first
implementation. This uses existing JSONB storage and does not require a schema
migration.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCompactionMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionConfig {
    #[serde(default = "default_remote_compaction_mode")]
    pub remote_mode: RemoteCompactionMode,
    #[serde(default = "default_auto_compaction_enabled")]
    pub auto_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_limit_tokens: Option<usize>,
    #[serde(default = "default_auto_compaction_max_failures")]
    pub max_consecutive_failures: usize,
}
```

Defaults:

- `remote_mode = Auto`
- `auto_enabled = true` only when a context window can be known/configured;
  otherwise auto-compaction does not trigger
- `auto_limit_tokens = None`
- `max_consecutive_failures = 3`

Remote mode semantics:

- `Auto` — recommended default
  - OpenAI: try remote; fallback to local on remote failure.
  - Anthropic: local only.
- `Always`
  - OpenAI: remote only; fail if `/responses/compact` fails.
  - Anthropic: fail clearly because remote compaction is unsupported.
- `Never`
  - OpenAI: local summary only.
  - Anthropic: local summary only.

Circuit-breaker runtime state should live next to the config in session metadata:

```json
{
  "compaction": {
    "config": {
      "remote_mode": "auto",
      "auto_enabled": true,
      "context_window": 400000,
      "auto_limit_tokens": null,
      "max_consecutive_failures": 3
    },
    "auto_state": {
      "consecutive_failures": 0,
      "suppressed": false,
      "last_failure": null,
      "last_success_root_id": null
    }
  }
}
```

Manual compaction success resets the auto failure state.

## Daemon orchestration

Refactor `run_compaction` in
`rust/crates/agent-daemon/src/provider_runtime.rs` to return
`CompactionOutput`.

Pseudo-flow:

```rust
pub(crate) async fn run_compaction(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<CompactionOutput> {
    let compaction_config = compaction_config(config);
    let remote_mode = compaction_config.remote_mode;
    let credentials = Credentials::load();
    let provider = provider_for_config(config, &credentials)?;

    if remote_mode != RemoteCompactionMode::Never
        && provider.provider.supports_remote_compaction()
    {
        let request = remote_compaction_request(
            state,
            config,
            session_id,
            model_context.clone(),
        ).await?;

        match compact_with_auth_retry(provider, request).await {
            Ok(result) => {
                let (summary, summary_kind) = match result.summary {
                    Some(summary) => (summary, CompactionSummaryKind::ProviderText),
                    None => (
                        generic_remote_compaction_summary(config.provider.kind),
                        CompactionSummaryKind::Generic,
                    ),
                };
                return Ok(CompactionOutput {
                    summary,
                    summary_kind,
                    provider_replay: result.provider_replay,
                    remote: true,
                    provider: config.provider.kind,
                    usage: result.usage.and_then(|usage| serde_json::to_value(usage).ok()),
                });
            }
            Err(error) if remote_mode == RemoteCompactionMode::Auto => {
                // log and fall through to local summary
            }
            Err(error) => return Err(error.into()),
        }
    } else if remote_mode == RemoteCompactionMode::Always {
        return Err(anyhow!("remote compaction unsupported for this provider"));
    }

    run_local_summary_compaction(state, config, session_id, model_context).await
}
```

Authentication retry behavior:

- Preserve existing Codex-auth refresh on 401.
- Apply it to both `complete` and `compact`.
- Factor the retry behavior into helper functions instead of duplicating subtly
  different logic:
  - `complete_with_auth_retry(...)`
  - `compact_with_auth_retry(...)`

Local summary behavior:

- Local summary compaction uses `complete(ModelRequest)`.
- Local summary compaction uses `session_id:compaction` and an isolated
  prompt-cache key.
- Local summary compaction always returns empty compaction-root
  `provider_replay`.
- The local summary request is provider-specific:
  - OpenAI fallback goes through OpenAI Responses rendering.
  - Anthropic goes through Claude `/v1/messages` rendering and must preserve
    valid Claude message/tool structure.

## Store changes

Do not make `agent-store` depend on daemon-private `CompactionOutput`. Add a
small store-facing completion type in `agent-store` or pass equivalent fields.

```rust
pub struct CompactionCompletion {
    pub summary: String,
    pub summary_kind: String,
    pub provider_replay: Vec<ProviderReplayItem>,
    pub remote: bool,
    pub provider: ProviderKind,
    pub usage: Option<Value>,
}
```

Change:

```rust
complete_compaction_action(&job, summary)
```

to:

```rust
complete_compaction_action(&job, completion: CompactionCompletion)
```

Persist:

- `CompactionSummary.summary`
- `CompactionSummary.tokens_before`
- `CompactionSummary.last_turn_id`
- `provider_replay` sidecar

Also include useful action/event result metadata:

```json
{
  "new_root_id": "...",
  "source_session_id": "...",
  "source_leaf_id": "...",
  "trigger": "manual",
  "reason": null,
  "remote": true,
  "provider": "openai",
  "summary_kind": "generic",
  "usage": null,
  "provider_replay_items": 1
}
```

No schema migration is needed for provider replay because
`transcript_entries.provider_replay` already exists.

Compaction action creation should accept trigger metadata:

```rust
create_compaction_action(session_id, CompactionTrigger::Manual)
create_compaction_action(session_id, CompactionTrigger::Auto { reason })
```

Action payload should include:

```json
{
  "source_session_id": "...",
  "source_leaf_id": "...",
  "last_turn_id": 12,
  "context_tokens": 350000,
  "trigger": "auto",
  "reason": "threshold"
}
```

## Manual compaction

Manual `/compact` should keep existing constraints:

- session must be idle
- no unfinished actions
- no queued inputs, because compaction is a source-mutating history operation
- active leaf must be a turn boundary
- source must be non-empty

Additional behavior:

- If active leaf changed while compaction was running, mark the action stale and
  do not mutate history.
- If active leaf is already a `CompactionSummary`, return a clear no-op/error
  instead of compacting again.
  - If there are new turns after a compaction root, the active leaf will be a
    later `TurnFinished`; this rule only rejects the exact active-root case.
- Manual OpenAI compaction should use remote by default in `Auto` mode.
- Manual Anthropic compaction should use local summary.
- Manual compaction success resets auto-compaction failure suppression for the
  session.

## Auto-compaction

Auto-compaction belongs in Rust daemon/session driving, not the web UI.

Recommended location:

```rust
SessionDriver::drive_until_blocked
```

But keep the driver simple by factoring the policy into one helper:

```rust
async fn maybe_start_auto_compaction(
    &self,
    active: Arc<Mutex<RuntimeSession>>,
) -> Result<bool, RpcError>
```

Run the check:

- after persisting turn completion / while at a safe boundary
- before taking the next queued input

If auto-compaction starts:

1. Create an auto compaction action with trigger/reason metadata.
2. Publish requested events.
3. Remove the loaded runtime session from `state.active`.
4. Spawn the compaction task.
5. Break/return from `drive_until_blocked()` immediately.

### Critical in-memory session rule

If auto-compaction starts while `state.active` contains a loaded
`RuntimeSession`, remove it before spawning the compaction job:

```rust
state.active.lock().await.remove(&session_id);
```

Then stop using any previously cloned `Arc<Mutex<RuntimeSession>>`. The driver
must break/return immediately after starting auto-compaction. Otherwise the
daemon can continue with stale pre-compaction in-memory history after Postgres
has a compacted root.

### Queued input rule

Do not consume queued input before pre-request compaction.

Add store support for peeking:

```rust
peek_next_queued_input(session_id)
```

`peek_next_queued_input` must use the exact same ordering expression as
`take_next_queued_input`, including steer/follow-up priority and promotion time.
Otherwise auto-compaction may estimate one input and consume a different one.

Use it to estimate projected context. If compaction starts, leave the input
queued. After compaction completes, `drive_until_blocked()` reloads the
compacted root and consumes the queued input.

### Trigger policy

Follow Codex-style default:

```rust
auto_limit = context_window * 9 / 10
```

If a configured limit exists:

```rust
auto_limit = min(configured_limit, context_window * 9 / 10)
```

Trigger on input/context tokens, not output or total tokens.

Sources for context tokens, in priority order:

1. live `AgentSession.context_tokens`
2. latest completed model action usage for the active path/session
3. rough transcript estimate

If context window is unknown and no explicit limit exists, do not auto-compact.

### Projected next input

Before consuming queued input:

```rust
projected = current_context_tokens + estimate_user_message_tokens(next_input)
```

If `projected >= auto_limit`, compact first.

This prevents a huge next prompt from causing an avoidable context overflow.

### Failure circuit breaker

Track consecutive auto-compaction failures. Default max:

```rust
max_consecutive_failures = 3
```

After the limit, suppress further auto-compaction for the session until:

- manual compaction succeeds
- model/provider config changes
- or a new successful model response updates usage

Store this state in session metadata so it survives daemon restart:

```json
{
  "compaction": {
    "auto_state": {
      "consecutive_failures": 3,
      "suppressed": true,
      "last_failure": "context too large during local compaction",
      "last_success_root_id": "entry_..."
    }
  }
}
```

Manual compaction may still be requested after auto suppression.

### Avoid immediate re-compaction loops

After successful compaction:

- clear/reset context-token assumptions for the new compacted root
- record the new root id in auto state
- do not trigger another auto-compaction until at least one new model response
  or enough new estimated content exists

### Mid-turn policy

First implementation should **not** auto-compact mid-turn.

Do not insert a compaction root while the active transcript is not a turn
boundary. Rely on:

- tool output limiting
- between-turn compaction
- explicit errors/TODO for future mid-turn support

## Usage persistence

Current `context_tokens` is volatile. Auto-compaction needs a durable source
after idle/restart.

Options:

1. Query latest completed model action result:
   - action result already stores provider usage.
   - Need ensure it can be correlated to active leaf/context.
2. Persist `context_tokens` in session metadata keyed by active leaf id.
3. Add a small usage table or extend action payload/result.

Recommended first step:

- Add a store method with one responsibility:

```rust
latest_context_usage_for_active_path(session_id) -> Option<ContextUsageSnapshot>
```

It should validate:

- action kind is model
- status is completed
- result has provider usage input tokens
- action payload has `context_leaf_id`
- `context_leaf_id` belongs to the current active path
- newest matching action wins

If that query is too awkward or fragile, persist `context_tokens` in session
metadata keyed by active leaf id instead. Fall back to rough transcript estimate
only when no reliable usage is available.

## Transcript grouping abstraction

Local trimming/retry, token estimation, and safe provider message structure all
need the same primitive. Add one grouping helper instead of scattering trimming
rules through provider-specific code.

Example shape:

```rust
enum TranscriptGroup {
    CompactionRoot(ModelContextEntry),
    Turn {
        entries: Vec<ModelContextEntry>,
        complete: bool,
    },
}
```

Use this for:

- local summary trimming/retry
- rough token estimates
- dropping oldest complete groups first
- preserving tool-call/tool-result validity
- detecting no-new-turns-after-compaction cases
- avoiding mid-turn auto-compaction

The grouping abstraction is provider-neutral over `ModelContextEntry` /
`TranscriptItem`. Provider-specific code then renders each kept group into the
correct API shape. This prevents OpenAI Responses-item filtering from corrupting
Claude `/v1/messages` structure.

## Local summary trimming/retry

Needed especially for Anthropic and for OpenAI fallback local summary.

On context-length/provider-too-large failures:

1. Identify complete historical turn groups using the shared grouping helper.
2. Preserve:
   - latest compaction summary, if any
   - most recent turns
   - current unresolved user constraints where possible
3. Drop oldest complete turn groups first.
4. Preserve tool-call/tool-result validity.
5. Retry with bounded attempts.
6. If still too large, fail with a clear compaction error.

Never trim arbitrary individual tool outputs in a way that creates invalid
provider message structure. Tool output limiting can happen before grouping.

## Web UI and RPC changes

Keep the web UI provider-agnostic.

Existing behavior is mostly sufficient:

- `/compact` sends `compaction.request`.
- Activity events mark the session running/idle.
- `compaction_summary` displays as a system message.
- History picker treats compaction root as a switch target.

Required server/RPC behavior:

- Redact `provider_replay` on compaction-root transcript entries before sending
  history/session entries to the browser.
- Preserve assistant-message `provider_replay` in web responses for existing
  hosted tool/citation rendering.
- Transcript export includes compaction summary text, never raw
  `provider_replay`.
- Raw OpenAI encrypted compaction content is never rendered, parsed, or exported
  by the web UI.

Optional UI improvements:

- Show more specific text if summary/action metadata adds provider/remote:
  - `OpenAI compacted history`
  - `Claude summarized history`
- On `compaction.completed`, force-refresh selected session entries.
- On `compaction.error`, show the event error text.

No web code should know the `/responses/compact` endpoint or parse opaque
provider replay.

## Edge-case checklist

### History correctness

- [ ] Compaction writes a root (`parent_id: None`).
- [ ] Active leaf becomes the compacted root only if source leaf is unchanged.
- [ ] Pre-compaction transcript ancestors are not on the active model path.
- [ ] Post-compaction turns append after the compacted root.
- [ ] Rewind to compaction root preserves stored `provider_replay` in daemon/store state.
- [ ] Fork from compaction root preserves stored `provider_replay` in daemon/store state.
- [ ] Manual compaction rejects exact active-leaf `CompactionSummary` no-ops.

### Provider replay

- [ ] OpenAI remote `type: "compaction"` is preserved exactly.
- [ ] OpenAI follow-up uses replay items, not fallback summary text, when replay
      exists.
- [ ] OpenAI remote success requires at least one kept `compaction` item.
- [ ] Anthropic ignores OpenAI replay and uses summary text.
- [ ] Anthropic local compaction does not store summary-call provider replay.
- [ ] RPC/web redacts compaction-root replay payloads.
- [ ] Web/export never renders or exports encrypted replay payloads.

### Staleness/races

- [ ] Source leaf changed before completion marks compaction stale.
- [ ] Auto-compaction removes stale in-memory active session before spawning.
- [ ] Driver breaks immediately after starting auto-compaction.
- [ ] Late model/tool completions cannot mutate compacted history.
- [ ] Daemon startup marks unfinished compaction actions stale.

### Queue behavior

- [ ] Pre-request auto-compaction peeks queued input without consuming it.
- [ ] Peek ordering exactly matches consume ordering.
- [ ] Queued input is consumed only after compaction completes and session reloads.
- [ ] Steer/follow-up priority ordering is preserved.

### Auto-compaction loops

- [ ] Successful compaction does not immediately trigger another compaction.
- [ ] Repeated failures trip a circuit breaker.
- [ ] Manual compaction can still be requested after auto suppression.
- [ ] Manual compaction success resets the auto failure state.

### Provider-specific failures

- [ ] OpenAI remote empty output falls back/fails according to remote mode.
- [ ] OpenAI remote no kept output falls back/fails according to remote mode.
- [ ] OpenAI remote no `compaction` item falls back/fails according to remote
      mode.
- [ ] OpenAI 401 refresh logic applies to compact calls.
- [ ] Anthropic context-length failure retries with safe trimming.
- [ ] `remote = always` fails clearly for Anthropic.

## Test plan

### `agent-provider`

- OpenAI compact request path is `/responses/compact`.
- OpenAI compact uses `ProviderCompactionRequest`, not `ModelRequest`.
- OpenAI compact body includes:
  - `model`
  - `input`
  - `instructions`
  - `tools`
  - `parallel_tool_calls`
  - `reasoning`
- OpenAI compact body excludes dynamic cwd context.
- OpenAI compact body does not set streaming.
- OpenAI compact body does not include stream/store/prompt-cache/service-tier/
  include fields unless explicitly enabled by tested code.
- OpenAI compact `input` contains rendered transcript items in the default path.
- `input: []` path is disabled unless an integration test verifies it works with
  current `store: false` transport settings.
- OpenAI parses `{ output: [...] }`.
- OpenAI preserves raw `type: "compaction"` output.
- OpenAI rejects empty/no-kept/no-compaction output as remote failure.
- OpenAI filters developer/synthetic-user/tool/reasoning artifacts from compact
  output, while preserving compaction, assistant messages, and verified real
  user/hook messages.
- OpenAI transcript rendering replays compaction-root provider replay.
- Anthropic transcript rendering uses summary text for compaction roots.
- Anthropic ignores OpenAI replay on compaction roots.
- Anthropic local compaction request uses `/v1/messages`,
  `ProviderToolProfile::None`, and no tools.
- Anthropic local trimming preserves valid `tool_use` / `tool_result` structure.

### `agent-daemon`

- `run_compaction` uses remote for OpenAI in `Auto`.
- `run_compaction` falls back to local summary for OpenAI remote failure in
  `Auto`.
- OpenAI fallback local summary uses `session_id:compaction`.
- `run_compaction` fails OpenAI remote failure in `Always`.
- `run_compaction` uses local summary for Anthropic in `Auto`.
- `run_compaction` fails Anthropic in `Always` before making a provider call.
- Codex auth refresh works for `compact`.
- Local compaction result has empty `provider_replay`.
- OpenAI remote compaction with no text creates a generic summary.

### `agent-store`

- `complete_compaction_action` stores provider replay on the new root.
- Completion is stale-safe when active leaf changed.
- Events/action result include trigger/reason/remote/provider/summary metadata.
- History tree and session snapshots preserve compaction root replay in
  daemon/store state.
- Web/RPC entry serialization redacts compaction-root replay.
- Manual compaction success resets auto failure metadata.

### `agent-session`

- Compaction root remains a turn boundary.
- Rehydrate session from compaction root works.
- Fork/rewind around compaction root preserves daemon/store replay state.
- Shared transcript grouping identifies complete turns and compaction roots.

### Auto-compaction

- Over-threshold OpenAI session starts remote compaction.
- Over-threshold Anthropic session starts local compaction.
- Queued input remains queued during auto-compaction.
- After compaction completion, queued input is consumed against compacted root.
- Projected huge queued input triggers compaction before consumption.
- `peek_next_queued_input` matches `take_next_queued_input` ordering.
- Failure circuit breaker suppresses repeated auto compactions.
- Manual compaction remains possible after auto suppression.
- No mid-turn auto-compaction occurs.
- `drive_until_blocked` breaks immediately after spawning auto-compaction.

### Web

- `/compact` still sends `compaction.request`.
- `compaction.requested` marks running.
- `compaction.completed` refreshes transcript.
- `compaction.error` displays useful notice.
- Compaction root displays as system message.
- Export includes compaction summary text when relevant.
- Export does not include raw provider replay.
- Web does not receive compaction-root encrypted replay in normal history/session
  responses.

## Implementation phases

### Phase 1: Manual provider/store plumbing

1. Add `ProviderCompactionRequest`, `ProviderCompactionResponse`, and optional
   trait methods.
2. Implement OpenAI `compact` with transcript-derived input and conservative
   output filtering.
3. Add OpenAI replay handling for `CompactionSummary`.
4. Add store-facing `CompactionCompletion` and persist `provider_replay`.
5. Generate generic summary text for opaque OpenAI remote compaction.
6. Redact compaction-root replay from web/RPC history views.
7. Keep manual compaction working.

### Phase 2: Anthropic/local robustness

1. Refactor local summary generation into an explicit path.
2. Ensure Anthropic compaction root has empty replay.
3. Add shared transcript grouping.
4. Add trimming/retry for local compaction context failures.
5. Add provider-specific tests.

### Phase 3: Minimal auto-compaction

1. Add `CompactionConfig` in session metadata.
2. Add threshold/window config and auto-state metadata.
3. Add durable usage lookup or metadata-backed context-token persistence.
4. Add queued-input peek and projected-token check.
5. Add `maybe_start_auto_compaction()` in `SessionDriver`.
6. Start auto-compaction before consuming queued input.
7. Remove active runtime session before spawning and break the driver loop
   immediately after spawning auto-compaction.
8. Add failure circuit breaker.

### Phase 4: Web polish

1. Refresh transcript on compaction completion.
2. Improve compaction notices if needed.
3. Verify export/history picker behavior.

## Open questions

- Should OpenAI remote compaction ever use `input: []`?
  - Only after an integration test proves `/responses/compact` compacts
    server-side state correctly with the current `store: false` normal request
    mode.
  - Until then, use transcript-derived compact input without dynamic context.
- Do we want a second local summary call after OpenAI remote compaction to
  create a high-quality fallback summary?
  - First implementation should avoid the extra cost and use a generic fallback
    summary.
- How strict should OpenAI compact output filtering become later if we obtain
  Codex's exact real-user-message parser?
  - Conservative first implementation drops user messages and keeps only obvious
    safe output types.
