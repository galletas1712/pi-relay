# pi-ai Sidecar Provider — Implementation Scope (Updated with Clean Seam)

## Design Principle

The daemon does NOT know which provider is being used. It passes an opaque
`ProviderConfig { provider: String, model: String }` to the sidecar. pi-ai resolves
the provider via models.json and handles all provider-specific logic internally.

This eliminates `ProviderKind` from the daemon entirely (14 files cleaned).

## Work Packages

### WP1: Extend `agent-vocab` types

**Files:**
- `rust/crates/agent-vocab/src/message.rs` — add `AssistantItem::Thinking`
- `rust/crates/agent-vocab/src/provider.rs` — replace `ProviderKind` enum with string
- `rust/crates/agent-provider/src/lib.rs` — add `cache_retention` to `ModelRequest`, remove `ProviderKind` from `ProviderToolProfile`

**Changes:**
1. `AssistantItem::Thinking { thinking: String, signature: Option<String> }`
   - Serde tagged: `{ "type": "thinking", "thinking": "...", "signature": "..." }`
2. `ProviderConfig`: replace `kind: ProviderKind` with `provider: String` (opaque)
3. `CacheRetention` enum: `None | Short | Long` (serde: none/short/long)
4. `ModelRequest`: add `cache_retention: Option<CacheRetention>`
5. `ProviderToolProfile`: remove `OpenAiCoding` / `AnthropicCoding` variants; replace with
   a single generic profile (or remove entirely — pi-ai handles tool profile internally)
6. Remove `ProviderKind` enum entirely; replace all `ProviderKind` references with `String`
   (or a `ProviderId(String)` newtype for type safety)

**Acceptance:**
- `cargo build` clean (all ProviderKind match arms updated)
- `AssistantItem::Thinking` serializes/deserializes correctly
- `ProviderConfig` with arbitrary provider string serializes correctly

### WP2: Build the pi-ai sidecar bridge (Node.js)

**Files (new):**
- `rust/sidecar/bridge.mjs` — HTTP server wrapping pi-ai
- `rust/sidecar/package.json` — depends on `@earendil-works/pi-ai`

**Methods:**
- `complete`: ModelRequest JSON → pi-ai `Context` (systemPrompt, messages with images + thinking,
  tools as canonical defs, sessionId, cacheRetention, reasoning) → `models.complete()` → AssistantMessage JSON
- `compact`: ProviderCompactionRequest JSON → summarization prompt → `models.completeSimple()` → `{ summary, usage }`
- `models.available`: returns available models with metadata (contextWindow, recommendedAutoCompactTokens)
- `web_search`: provider-native web search (Anthropic WebSearch tool, OpenAI web search tool) or neutral HTTP fallback (based on models.json `webSearch` config)
- `web_fetch`: provider-native web fetch or neutral HTTP fallback (based on models.json config)

**Key: the sidecar handles provider-specific features that pi-ai doesn't expose:**
- Provider-native compaction: `POST /responses/compact` (Codex) or `POST /messages` with compaction beta (Anthropic) — decided by models.json `compaction: "native" | "prompt"` config
- Provider-native web search: Anthropic WebSearch tool, OpenAI web search tool — decided by models.json `webSearch: "native" | "neutral"` config
- These are pi-relay differentiators implemented as sidecar extensions over pi-ai

**Key: pi-ai builds ALL provider-specific wire format:**
- Tool declarations (apply_patch for Codex, str_replace for Anthropic, etc.)
- Cache markers (cache_control for Anthropic, prompt_cache_key for Codex)
- Session affinity headers (x-codex-*, x-client-request-id, x-session-id)
- System prompt formatting (Claude Code identity for OAuth, etc.)
- Auth (OAuth refresh, API key from auth.json / env)

**Acceptance:**
- Works with: OpenAI Codex (OAuth), Anthropic (OAuth), NVIDIA inference (API key via models.json)
- `compact` returns non-empty summary
- No secrets in stdout/logs

### WP3: Implement `PiAiSidecarProvider` (Rust)

**Files:**
- `rust/crates/agent-provider/src/pi_ai.rs` — new
- `rust/crates/agent-provider/src/lib.rs` — re-export, remove old providers
- `rust/crates/agent-daemon/src/provider_runtime/connections.rs` — replace with `PiAiConnection`

**Changes:**
- Delete `openai.rs` and `anthropic.rs`
- `PiAiSidecarProvider`: HTTP client to sidecar bridge
  - `complete()`: serialize ModelRequest (with Thinking + Image + canonical tools + cache_retention) → POST → ModelResponse
  - `compact()`: serialize ProviderCompactionRequest → POST → ProviderCompactionResponse
  - `model_metadata()`: call sidecar's `models.available`
  - `count_tokens()`: default (Err)
- `PiAiConnection`: shared HTTP client to sidecar (one sidecar process per daemon)
- `ProviderConnectionRegistry`: remove all provider-specific connection types;
  always return `PiAiSidecarProvider`

**Acceptance:**
- `cargo build` clean
- Unit test: mock sidecar, verify request/response translation (Thinking, Image, tools)
- Unit test: `compact()` returns non-empty summary

### WP4: Clean ProviderKind from daemon (14 files)

**Files:**
- `provider_runtime/connections.rs` — remove provider-specific connections
- `provider_runtime/compaction.rs` — `run_native_compaction()` calls sidecar `compact()` (sidecar decides native vs prompt-based via models.json); remove `generic_native_compaction_summary()` and provider-specific match arms
- `provider_runtime/context_accounting.rs` — replace provider-specific token counting with `usage.totalTokens` from response
- `provider_runtime/prompt.rs` — remove `provider_tools_for_session(provider)`; pass canonical tool defs directly
- `provider_runtime/mcp.rs` — remove `HashMap<ProviderKind, Vec<ProviderTool>>`; single canonical list
- `provider_runtime/web_tools.rs` — replace provider-native web sidecar with sidecar `web_search`/`web_fetch` calls (provider-native for known providers, neutral fallback for others); remove ProviderKind
- `provider_runtime/skills.rs` — remove ProviderKind parsing; pass provider string through
- `runtime_hosts.rs` — remove `HashMap<ProviderKind, Vec<ProviderTool>>`; single list
- `runtime/tool.rs` — remove ProviderKind from ToolRegistry::execute (or use opaque string)
- `config.rs` — `ProviderConfig { provider: String, model: String }` (no ProviderKind)
- `main.rs` — remove `ProviderKind::from_str()`; accept arbitrary provider string
- `session_start.rs` — read provider from config, not hardcoded
- `subagents.rs` — inherit parent provider config, not hardcoded
- `agent-tools/src/registry.rs` — remove provider-keyed tool declarations; single canonical def per tool

**Acceptance:**
- `cargo build` clean with ZERO `ProviderKind` references in daemon
- All existing tests pass (update mocks)
- `grep -rn 'ProviderKind' rust/crates/agent-daemon/src/ | grep -v test` → empty

### WP5: Frontend + transcript rendering

**Files:**
- `packages/web/src/transcript.tsx` — render `Thinking` items (collapsible, hidden by default)
- `packages/web/src/types.ts` — add `Thinking` to transcript item types

**Acceptance:**
- Web build clean, vitest pass
- Thinking items render as collapsed sections (hidden by default per design rules)

### WP6: Integration + end-to-end

**Files:**
- `rust/crates/agent-daemon/src/main.rs` — start sidecar on boot, kill on shutdown
- `rust/crates/agent-daemon/tests/` — integration tests

**Acceptance:**
- Full session lifecycle with sidecar: create, prompt, model turn with thinking + tools, compaction, fork/switch
- Real model turn with Codex returns thinking + text + tool calls
- Compaction triggers and produces summary
- MCP tools work (execution pipeline unchanged)
- No regressions in existing daemon tests

## Dependency Order

```
WP1 (agent-vocab types) — foundation
 ├── WP2 (sidecar bridge) — independent
 ├── WP3 (PiAiSidecarProvider) — depends WP1
 │    ├── WP4 (clean ProviderKind from daemon) — depends WP1 + WP3
 │    └── WP5 (frontend) — depends WP1, parallel
 └── WP6 (integration) — depends WP3 + WP4
```

## What's NOT in Scope

- WebSocket transport replacement (Phase 4)
- PostgreSQL → JSONL migration (Phase 3)
- MCP → pi extension system (Phase 2)
- Rust daemon deletion (Phase 5)
- Old session migration (separate, migrator already built)
