# Clean Seam Design — Updated with Provider-Native Features

## Model Naming — No Prefix

Models are NOT prefixed with `openai:` or `claude:`. The daemon passes an opaque
`ProviderConfig { provider: String, model: String }` to the sidecar. pi-ai resolves
the provider via models.json and handles all provider-specific logic internally.

Example: `provider: "openai-codex", model: "gpt-5.6-sol"` — not `"openai:gpt-5.6-sol"`.
Example: `provider: "anthropic", model: "claude-opus-5"` — not `"claude:claude-opus-5"`.
Example: `provider: "nvidia-inference", model: "nvidia/zai-org/glm-5.2"`.

## Provider-Native Web Search (Retained, Configurable)

The rust daemon currently makes a SECOND provider API call for provider-native web search
(Anthropic server-side `web_search`, OpenAI file_search). This is a valuable feature
that should be retained.

**How it works with the sidecar:**
- The sidecar exposes a `web_search` method that:
  - For Anthropic: calls `complete()` with the Anthropic `WebSearch` tool declaration
    (pi-ai knows this tool name — it's in the Claude Code tool list)
  - For OpenAI: calls `complete()` with the OpenAI web search tool declaration
  - For other providers: falls back to provider-neutral HTTP web search (daemon-side)
- Configuration: `models.json` provider entry can declare `webSearch: "native" | "neutral" | "disabled"`
  (default: `"native"` for known providers, `"neutral"` for others)
- The daemon's `web_tools.rs` calls the sidecar's `web_search` method instead of
  making a direct provider API call. This removes ProviderKind from web_tools.rs
  while retaining provider-native web search.

**Alternative:** keep web search as a daemon-side provider-neutral HTTP tool only
(simpler, but loses provider-native web search quality). The owner wants to retain
provider-native, so the sidecar approach is preferred.

## Provider-Native Compaction (Retained, with Fallback)

The rust daemon currently calls provider-native compaction APIs:
- OpenAI: `POST /responses/compact` (Codex-specific endpoint)
- Anthropic: `POST /messages` with compaction beta header

pi-ai and pi-coding-agent do NOT have these — they use prompt-based compaction
(`completeSimple()` with a summarization prompt). The owner wants to retain
provider-native compaction for OpenAI and Anthropic, with prompt-based fallback
for other providers.

**How it works with the sidecar:**
- The sidecar's `compact()` method:
  - For `openai-codex` provider: calls `POST {baseUrl}/responses/compact` directly
    (the sidecar has the HTTP client + auth from pi-ai's credential store)
  - For `anthropic` provider: calls `POST {baseUrl}/messages` with the
    `anthropic-beta: compaction` header directly
  - For other providers: falls back to `completeSimple()` with the summarization prompt
    (pi-coding-agent's `generateSummary()` pattern)
- The sidecar decides which path based on the provider string — the daemon doesn't know.
- This means the `ModelProvider::compact()` trait method is unchanged;
  the sidecar internally dispatches.

**Why the sidecar can do this:** pi-ai gives the sidecar access to the provider's
HTTP client, auth credentials, and base URL. The sidecar can make direct HTTP calls
to provider-specific endpoints (like `/responses/compact`) that pi-ai doesn't
expose as a first-class API. This is an extension point — the sidecar extends
pi-ai's provider with provider-specific features that upstream doesn't have.

## Are There Existing Pi Plugins for This?

**No.** Checked:
- pi-coding-agent's compaction (`core/compaction/compaction.js`): always prompt-based (`completeSimple`)
- pi-mono's compaction (`packages/agent/src/harness/compaction/compaction.ts`): also prompt-based
- pi-ai: no `/responses/compact` or Anthropic compaction beta support
- Our extensions (prime-rlm, prime-comms, prime-harness, prime-autonomy): no compaction or web search
- oh-my-pi community repo: no compaction or web search plugins

Provider-native compaction and provider-native web search are **pi-relay differentiators**
that would be implemented as **new sidecar features** (not pi plugins per se, but
sidecar extensions that use pi-ai's HTTP client + auth to make provider-specific
API calls that pi-ai doesn't expose).

## Updated Architecture

```
┌─ Rust Daemon (provider-agnostic) ─────────────────────────────┐
│  session/store/MCP/tools/transport — all unchanged             │
│  ModelProvider trait (2 methods: complete + compact)           │
│  ProviderConfig { provider: String, model: String } — opaque  │
└──────────────────────────┬────────────────────────────────────┘
                           │ HTTP JSON
                           │
              ┌────────────┴────────────┐
              │  Node.js Sidecar        │
              │  (wraps pi-ai + extras) │
              │                         │
              │  complete() →           │
              │    pi-ai models.complete()│
              │    (handles tool decls,  │
              │     cache hints, auth,   │
              │     thinking, images)    │
              │                         │
              │  compact() →             │
              │    openai-codex:         │
              │      POST /responses/    │
              │      compact (native)    │
              │    anthropic:            │
              │      POST /messages with │
              │      compaction beta      │
              │      (native)            │
              │    others:               │
              │      completeSimple()    │
              │      with summary prompt │
              │      (fallback)          │
              │                         │
              │  web_search() →          │
              │    anthropic: WebSearch  │
              │      tool (native)       │
              │    openai: web search    │
              │      tool (native)       │
              │    others: HTTP fetch    │
              │      (neutral fallback) │
              └─────────────────────────┘
```

## What the Daemon Passes (No Provider Knowledge)

```rust
ModelRequest {
    model: String,           // "gpt-5.6-sol"
    provider: String,        // "openai-codex" (opaque)
    prompt: PromptSections,
    transcript: Vec<ModelTranscriptEntry>,  // includes Thinking + Image
    tools: Vec<ProviderTool>,  // canonical defs only (name, description, schema)
    cache_retention: Option<CacheRetention>,
    session_id: Option<String>,
    reasoning_effort: ReasoningEffort,
    ...
}
```

The daemon does NOT know:
- Which API type (responses/messages/completions) is used
- Whether the provider has native compaction
- Whether the provider has native web search
- What tool declaration format the provider expects
- What cache headers the provider needs

The sidecar resolves ALL of this via models.json + provider string.

## Configuration (models.json)

```json
{
  "providers": {
    "openai-codex": {
      "api": "openai-responses",
      "baseUrl": "https://api.openai.com/v1",
      ...
      "compaction": "native",    // use POST /responses/compact
      "webSearch": "native"      // use provider's web search tool
    },
    "anthropic": {
      "api": "anthropic-messages",
      "baseUrl": "https://api.anthropic.com",
      ...
      "compaction": "native",    // POST /messages with compaction beta
      "webSearch": "native"      // Anthropic WebSearch tool
    },
    "nvidia-inference": {
      "api": "openai-completions",
      "baseUrl": "http://127.0.0.1:8571/v1",
      ...
      "compaction": "prompt",    // completeSimple() with summary prompt
      "webSearch": "neutral"     // HTTP web search (daemon-side)
    }
  }
}
```

The sidecar reads these config fields to decide which compaction/web-search path to use.
The daemon is unaware of these fields — they're sidecar/pi-ai configuration.


## Marketplace Plugin Investigation (2026-08-10)

### Does a pi plugin marketplace exist?
**Yes** — oh-my-pi (the `@oh-my-pi/pi-coding-agent` fork by can1357) has a full marketplace system:
- `/marketplace add <source>` — add marketplace from Git, local dir, or direct catalog URL
- `/marketplace install name@marketplace` — install plugins
- Compatible with Claude Code plugin registry format (`.claude-plugin/marketplace.json`)
- Plugins can contain: skills, commands, agents, hooks, tools, MCP servers, LSP servers, extension modules

### Are there marketplace plugins for provider-native compaction or web search?
**No.** These are **built-in features** of the oh-my-pi fork, not marketplace plugins:
1. **Provider-native compaction**: `@oh-my-pi/pi-agent-core/compaction` — `shouldUseOpenAiRemoteCompaction()`, `shouldUseProviderNativeCompaction()`, OpenAI V1 (`/responses/compact`), V2 (streaming compaction_trigger), plus snapcompact (bitmap archival) and shake (mechanical elision) strategies
2. **Provider-native web search**: `@oh-my-pi/pi-ai` — Anthropic `web_search_20250305` tool as a search provider (`packages/coding-agent/src/web/search/providers/anthropic.ts`), plus 23-provider chain (Brave, Kagi, etc.)
3. **Compaction hooks**: `session_before_compact` (can cancel/customize), `session_compact` (post-compact notification) — extension hooks in the pi-coding-agent extension system

### Upstream pi (pi-mono / @earendil-works/pi-ai) vs oh-my-pi fork (@oh-my-pi/pi-ai)

| Feature | Upstream (pi-mono) | oh-my-pi fork |
|---|---|---|
| Compaction | Prompt-based only (`completeSimple`) | Provider-native (OpenAI V1/V2) + prompt-based + snapcompact + shake |
| Web search | Not built-in | 23-provider chain incl. Anthropic native `web_search` |
| Marketplace | No | Yes (Claude Code-compatible) |
| Compaction hooks | `session_before_compact` | Same + snapcompact/shake strategies |
| npm package | `@earendil-works/pi-ai` | `@oh-my-pi/pi-ai` (v17.2.12) |

### Options for our sidecar

**Option A: Use `@oh-my-pi/pi-ai` (the fork)**
- Gets provider-native compaction + web search built-in
- Risk: tracking a fork instead of upstream; fork may diverge
- The fork is actively maintained (can1357, omp.sh)

**Option B: Use upstream `@earendil-works/pi-ai` + sidecar extensions**
- Provider-native compaction: sidecar makes direct HTTP calls to `/responses/compact` (Codex) and `/messages` with compaction beta (Anthropic) using pi-ai's HTTP client + auth
- Provider-native web search: sidecar calls Anthropic's `web_search` tool via pi-ai's `complete()` with the tool declaration
- Prompt-based fallback for other providers
- Risk: we maintain the provider-native code ourselves

**Option C: Publish our provider-native features as marketplace plugins**
- Package compaction + web search as pi plugins installable via `/marketplace install`
- Works with both upstream and fork
- Most work but cleanest separation

### Recommendation
**Option B** for Phase 1 (fastest, self-contained). The sidecar wraps upstream
pi-ai and adds provider-native compaction + web search as sidecar-specific
extensions. These use pi-ai's HTTP client + auth to make direct API calls that
pi-ai doesn't expose. **Option C** for Phase 2 (publish as marketplace plugins
once the sidecar is proven). This gives us the cleanest separation and benefits
the broader ecosystem.
