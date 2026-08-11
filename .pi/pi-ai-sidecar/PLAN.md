# pi-ai Sidecar Migration — Updated Plan (Using @oh-my-pi/pi-ai)

## Decision: Use @oh-my-pi/pi-ai (the oh-my-pi fork) instead of @earendil-works/pi-ai

The oh-my-pi fork (`@oh-my-pi/pi-ai` v17.2.12, by can1357 / omp.sh) is a superset
of upstream pi-ai with provider-native compaction, provider-native web search,
and additional providers built in. Using it as our sidecar's pi-ai dependency
gives us everything we need without building custom extensions.

## What We Gain (vs current rust backend)

### From upstream pi-ai (same as before)
- ✅ Custom endpoints via models.json (`"api": "openai-completions"`) — any OpenAI-compatible endpoint
- ✅ Thinking content (ThinkingContent with signature for multi-turn continuity)
- ✅ Full image support (ImageContent in messages, provider-specific wire format handled)
- ✅ Provider-native cache hints (cacheRetention, cache_control markers, prompt_cache_key, session affinity headers)
- ✅ 87+ built-in providers (OpenAI, Anthropic, Google, Bedrock, Groq, Fireworks, etc.)
- ✅ Upstream-maintained HTTP/SSE parsing, OAuth refresh, API key management

### Additional from oh-my-pi fork (NOT in upstream)
- ✅ **Provider-native compaction** (OpenAI V1 `/responses/compact` + V2 streaming `compaction_trigger`)
  - `shouldUseOpenAiRemoteCompaction(model)` — detects if model supports remote compaction
  - `shouldUseProviderNativeCompaction(model, settings)` — detects provider-native compaction
  - Fallback chain: V2 streaming → V1 `/responses/compact` → local prompt-based summarization
  - `CodexCompactionContext` / `CodexCompactionMetadata` types in pi-ai
  - Compaction hooks: `session_before_compact` (cancel/customize), `session_compact` (post-notify)
- ✅ **Provider-native web search** (Anthropic `web_search_20250305` tool)
  - 23-provider search chain (Anthropic native, Brave, Kagi, etc.)
  - Configurable ordering and exclusion
  - Returns synthesized answers with citations and source metadata
- ✅ **Snapcompact** (bitmap archival — no LLM call, history archived as dense PNG images)
- ✅ **Shake** (mechanical content elision — inline local reduction without summarization model)
- ✅ **Additional providers** (Cursor, Devin, GitLab Duo, Ollama, Pi Native, etc.)
- ✅ **Claude Code fingerprint** (OAuth identity headers, stealth mode tool naming)
- ✅ **Marketplace system** (Claude Code-compatible plugin registry)
- ✅ **Compaction strategies**: context-full, snapcompact, shake, handoff, off

### From our existing rust backend (preserved)
- ✅ PostgreSQL session store (durable, atomic, queryable)
- ✅ MCP client (rmcp, stdio + streamable HTTP, OAuth)
- ✅ Tool execution pipeline (Bash, Edit, MCP, delegations — all daemon-owned)
- ✅ Subagents/delegations with CAS barriers
- ✅ Fork/switch history (transcript forest, branch navigation)
- ✅ WebSocket transport (53 RPC methods, event system)
- ✅ Compaction TRIGGER logic (threshold/overflow detection — provider-agnostic)
- ✅ WebSearch/WebFetch tools (daemon-executed, provider-neutral HTTP)

## What We Lose

### From the rust backend (replaced by pi-ai)
- ❌ `provider_replay` cache continuity (opaque per-provider sidecar data stored alongside
  transcript entries) — pi-ai manages cache internally via cache_control markers and
  prompt_cache_key. This is a simplification: no correctness impact, only a different
  cache management model. The `provider_replay` field becomes unused.
- ❌ Hand-rolled HTTP/SSE parsing for OpenAI and Anthropic — replaced by pi-ai's
  maintained implementations (gain: upstream maintenance, less code to maintain)
- ❌ `ProviderKind` enum (closed, 2 variants) — replaced by opaque provider string
  (gain: adding a new provider = adding to models.json, zero rust changes)
- ❌ Provider-specific tool declaration JSON (apply_patch vs str_replace) — pi-ai builds
  the wire format from canonical tool definitions (gain: simpler tool registry)
- ❌ Provider-specific token counting (Anthropic remote, OpenAI estimation) — use
  `usage.totalTokens` from pi-ai's response (gain: uniform, no provider-specific code)
- ❌ Provider-native web search sidecar (second provider API call) — replaced by
  oh-my-pi's built-in web search with Anthropic native support (gain: 23-provider chain)

### Nothing of substance is lost
- WebSearch/WebFetch: retained (oh-my-pi has provider-native + 23-provider chain)
- Provider-native compaction: retained (oh-my-pi has V1/V2 + snapcompact + shake)
- Cache hints: retained (pi-ai manages cache_control + prompt_cache_key + session affinity)
- Thinking content: retained (new AssistantItem::Thinking variant)
- Image support: retained (already in ContentBlock, pi-ai handles wire format)
- All session/store/MCP/tools/transport: unchanged

## The Plan

### Phase 1: pi-ai Sidecar Provider (this wiki)

Replace `agent-provider/` (both OpenAI + Anthropic providers) with a single
`PiAiSidecarProvider` that delegates to `@oh-my-pi/pi-ai` via a Node.js HTTP bridge.

**Daemon changes:**
- `agent-vocab`: add `AssistantItem::Thinking`, replace `ProviderKind` with opaque string
- `agent-provider`: delete `openai.rs` + `anthropic.rs`, add `pi_ai.rs` (HTTP client to sidecar)
- `agent-daemon`: clean `ProviderKind` from 14 files, adapt compaction/prompt/transcript
- `agent-tools`: simplify tool registry (canonical defs, no provider-specific JSON)
- Frontend: render thinking content (collapsible, hidden by default)

**Sidecar (new):**
- Node.js HTTP server wrapping `@oh-my-pi/pi-ai`
- Methods: `complete`, `compact` (with provider-native detection), `web_search`, `models.available`
- Loads models.json + auth.json from agentDir
- Provider-native compaction: V2 streaming → V1 `/responses/compact` → prompt-based fallback
- Provider-native web search: Anthropic `web_search` tool → 23-provider chain → HTTP fallback

**Result:** daemon is provider-agnostic; sidecar handles all provider-specific logic.
Adding a new provider = adding to models.json, zero rust changes.

### Future Phases (not in scope)
- Phase 2: Move MCP to pi's extension system
- Phase 3: Move session store from PG to pi's JSONL (or adapter)
- Phase 4: Replace WebSocket transport with bridge WS (adds message.delta streaming)
- Phase 5: Delete rust daemon entirely
