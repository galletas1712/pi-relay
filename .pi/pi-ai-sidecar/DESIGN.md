# pi-ai Sidecar Provider — Design

## Goal

Replace both hand-rolled Rust providers (OpenAI Codex Responses + Anthropic Messages)
with a single `PiAiSidecarProvider` that delegates to upstream pi-ai (npm), while
keeping the Rust daemon's session management, PostgreSQL store, MCP/tools, WebSocket
transport, and frontend protocol completely unchanged.

## The Seam

The `ModelProvider` trait (`agent-provider/src/lib.rs`) is the clean boundary:

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse>;
    async fn model_available(&self, model: &str) -> ProviderResult<bool>;        // default
    async fn model_metadata(&self, _model: &str) -> ProviderResult<Option<ProviderModelMetadata>>; // default
    async fn compact(&self, request: ProviderCompactionRequest) -> ProviderResult<ProviderCompactionResponse>;
    async fn count_tokens(&self, _request: ProviderTokenCountRequest) -> ProviderResult<...>; // default: Err
}
```

Only `complete()` and `compact()` are mandatory. The daemon calls them in **batch**
(consumes the full SSE stream internally, returns one assembled `ModelResponse`).

Everything above this trait is provider-agnostic:
- `agent-core` (pure FSM, zero provider knowledge)
- `agent-session` (AgentSession, TranscriptStore, ModelContext)
- `agent-store` (PostgreSQL — `provider_config` is opaque JSONB)
- `agent-tools` + `agent-mcp` (tool registry, MCP client, execution — provider-agnostic)
- `agent-daemon/src/runtime/` (dispatch, compaction trigger, session driver)
- WebSocket transport, 53 RPC methods, event system, frontend

## Architecture

```
┌─ Frontend (React SPA) ← WS → Rust Daemon ─────────────────────┐
│                                                               │
│  agent-core (FSM) ─── agent-session (PG) ─── agent-tools/MCP  │
│       │                        │                  │             │
│       └────────────────────────┴──────────────────┘             │
│                    ModelProvider trait (EXTENDED)               │
│                    + Thinking in AssistantItem                  │
│                    + cacheRetention in ModelRequest             │
│                         │                                       │
│               ┌─────────┴──────────┐                           │
│               │  PiAiSidecarProvider │  (NEW — replaces both)   │
│               │  (Node.js subprocess) │                         │
│               └─────────┬──────────┘                           │
│                         │                                       │
└─────────────────────────┼───────────────────────────────────────┘
                          │ JSON over stdin/stdout (or HTTP)
                          │
              ┌───────────┴───────────┐
              │   pi-ai (npm package)  │
              │   ├ anthropic-messages  │
              │   ├ openai-responses   │
              │   ├ openai-completions │  ← custom endpoints via models.json
              │   └ models.json store │
              └───────────────────────┘
```

## Translation Map

### Rust → pi-ai (request)

| Rust (pi-relay) | pi-ai (TypeScript) |
|---|---|
| `ModelRequest.model` | `Model.id` (provider/modelId resolution via models.json) |
| `PromptSections` (stable_prefix + dynamic_context) | `Context.systemPrompt` (joined via `render_joined()`) |
| `Vec<ModelTranscriptEntry>` | `Context.messages: Message[]` (filter TurnStarted/TurnFinished/ToolCallStarted/DaemonToolObservation; map UserMessage→user, AssistantMessage→assistant with Thinking, ToolResult→toolResult, CompactionSummary→user msg) |
| `Vec<ProviderTool>` | `Context.tools: Tool[]` (name, description, parameters) |
| `ReasoningEffort` | `SimpleStreamOptions.reasoning: ThinkingLevel` (None→off, Minimal→minimal, Low→low, Medium→medium, High→high, XHigh→xhigh, Max→max) |
| `max_tokens` | `StreamOptions.maxTokens` |
| `session_id` / `prompt_cache_key` | `StreamOptions.sessionId` |
| `cache_retention` (NEW) | `StreamOptions.cacheRetention` (none/short/long) |
| `ContentBlock::Image` | `ImageContent { type:"image", mimeType, data }` |

### pi-ai → Rust (response)

| pi-ai (TypeScript) | Rust (pi-relay) |
|---|---|
| `AssistantMessage.content[]` | `Vec<AssistantItem>` (see below) |
| `TextContent` | `AssistantItem::Text(text)` |
| `ThinkingContent` (thinking + signature) | `AssistantItem::Thinking { thinking, signature }` (NEW variant) |
| `ToolCall` (id, name, arguments) | `AssistantItem::ToolCall(ToolCall)` |
| `AssistantMessage.usage` | `ProviderUsage` (input→input_tokens, output→output_tokens, cacheRead→cache_read, cacheWrite→cache_write, cacheWrite1h→cache_creation_1h, totalTokens→total_tokens) |
| `AssistantMessage.stopReason` | `ModelStopReason` (stop→Complete, length→MaxOutputTokens, toolUse→Complete, error→Refusal, aborted→Refusal) |
| *(no equivalent)* | `provider_replay: Vec::new()` (pi-ai manages cache internally) |

### Compaction

pi-ai has no native compaction endpoint. Use `completeSimple()` with a summarization
prompt (pi-coding-agent's `generateSummary()` pattern). This is a **generic, model-agnostic**
compaction — works with any provider, unlike the rust backend's provider-native-only
compaction. Return the summary text as `ProviderCompactionResponse.summary`.

## What Stays Unchanged

- `agent-core` (pure FSM)
- `agent-store` (PostgreSQL schema — `provider_config` is already opaque JSONB)
- `agent-tools` + `agent-mcp` (tool registry, MCP client, execution pipeline)
- `agent-daemon/src/runtime/` (dispatch, compaction trigger, session driver)
- WebSocket transport, 53 RPC methods, event system, frontend protocol
- Subagents, delegations, fork/switch, history

## What Changes

### `agent-vocab` (extend types)
- `AssistantItem`: add `Thinking { thinking: String, signature: Option<String> }` variant
- `ModelRequest`: add `cache_retention: Option<CacheRetention>` (none/short/long)
- `ProviderKind`: replace closed enum with string-based `provider: String` (or add `PiAi` variant)

### `agent-provider` (replace implementations)
- Delete `openai.rs` and `anthropic.rs`
- Add `pi_ai.rs` — `PiAiSidecarProvider: ModelProvider`
- Delete `ProviderConnectionRegistry`'s provider-specific connection types
- Add `PiAiConnection` (one subprocess per session, or shared HTTP bridge)

### `agent-daemon` (adapt upper modules)
- `provider_runtime/connections.rs`: replace with `PiAiConnection`
- `provider_runtime/compaction.rs`: `run_native_compaction()` calls sidecar's `compact()` (prompt-based)
- `provider_runtime/prompt.rs`: simpler (pi-ai handles provider-specific system prompt formatting)
- `provider_runtime/transcript.rs`: transcript serialization for next request — pi-ai handles conversion
- `runtime/model.rs`: `apply_model_response()` handles `AssistantItem::Thinking`

### `agent-session` (minor)
- `TranscriptStore`: store `Thinking` items
- `ModelContext`: include thinking content in transcript for multi-turn continuity

### Frontend (additive)
- Transcript rendering: show/hide thinking content (collapsible, hidden by default)
- Image rendering in user messages (already in ContentBlock, needs UI)

## What You Gain

- ✅ Custom endpoints via models.json (`"api": "openai-completions"`) — any OpenAI-compatible endpoint
- ✅ Generic compaction (prompt-based, works with any provider)
- ✅ Thinking content (with signature for multi-turn continuity)
- ✅ Full image support (already in ContentBlock, pi-ai handles wire format)
- ✅ Provider-native cache hints (pi-ai manages cache_control breakpoints, prompt_cache_key, session affinity headers)
- ✅ All existing pi-relay functionality (PG sessions, MCP, subagents, fork/switch, frontend) stays
- ✅ Upstream-maintained provider implementations (no more hand-rolled HTTP/SSE parsing)

## What You Lose

Nothing. `provider_replay` is replaced by pi-ai's internal cache management. The hand-rolled
HTTP clients are replaced by pi-ai's maintained ones. WebSearch/WebFetch stay in the rust tool
registry (they're tools, not provider features).
