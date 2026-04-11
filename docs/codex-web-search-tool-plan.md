# Codex-backed `web_search` Tool Plan

## Goal

Add a normal pi extension tool that lets a pi agent search the web using the same underlying Codex/OpenAI native `web_search` capability that the Codex harness uses.

This should give pi agents access to the same backend web information source as Codex without changing pi core.

## Non-Goals

This plan does **not** include:

- adding first-class native `web_search` support to pi core
- adding a separate `web_fetch` tool in v1
- preserving Codex `search` / `openPage` / `findInPage` trace fidelity in pi
- matching Codex's final wording or exact search strategy byte-for-byte

What we want is:

- the same underlying native web-search backend
- reused pi `openai-codex` OAuth credentials
- a clean local pi tool the outer agent can call directly

## Decision

Implement `web_search` as a **custom extension tool** that makes a **nested `openai-codex` request**.

The nested request will:

- use an `openai-codex` model
- reuse the user's existing pi OAuth login for `openai-codex`
- inject the native Responses/Codex `web_search` tool into the request payload
- return a concise answer plus sources

We explicitly avoid patching pi's main provider loop or tool system.

## Why this approach

### Why not a skill?

A real tool is more reliable than a skill for automatic agent use. Skills are progressive disclosure and models do not always load them automatically.

### Why not pi-core native `web_search`?

pi's current tool model is function-tool-oriented. Adding first-class native provider tools would be a larger architectural change than needed.

### Why not provider payload hacks on the main agent turn?

That is brittle and couples normal turns to Codex-specific payload surgery. A nested tool call keeps the Codex-specific logic isolated.

## High-level architecture

```text
pi outer agent
  -> calls local extension tool: web_search(query, allowed_domains?)
    -> extension resolves openai-codex model + OAuth token from pi
    -> extension makes nested Codex Responses request
       with native tool: { type: "web_search", ... }
    -> extension returns answer + sources to outer agent
```

## Reusing existing OAuth

We should reuse pi's existing login state for provider `openai-codex`.

That means the extension should use:

- `ctx.modelRegistry.find("openai-codex", modelId)`
- `ctx.modelRegistry.getApiKeyAndHeaders(model)`

This gives us:

- the current access token
- automatic token refresh if expired
- any provider/model headers pi already knows how to attach

This is the correct reuse path when the user has already logged into pi with ChatGPT Plus/Pro Codex OAuth.

## Important limitation

This reuses **pi's** `openai-codex` OAuth state.

It does **not** automatically import auth state from the separate `~/codex` harness if that harness stores credentials independently.

## Proposed tool surface

### Tool name

`web_search`

### Tool parameters

The implemented tool exposes:

```ts
{
  query: string,
  allowed_domains?: string[],
  reasoning_effort?: "low" | "medium" | "high" | "xhigh",
  search_context_size?: "low" | "medium" | "high"
}
```

Defaults:

- `reasoning_effort`: `"medium"`
- `search_context_size`: `"high"`

## Tool result shape

### Content sent back to the outer LLM

Return plain text content containing:

- concise synthesized answer
- a mandatory `Sources:` section
- markdown links for sources

Example:

```text
React 19 added ...

Sources:
- [React docs](https://react.dev/...)
- [Release notes](https://...)
```

### Structured details for UI/debugging

Store lightweight metadata in `details`, for example:

```ts
{
  provider: "openai-codex",
  model: string,
  query: string,
  allowedDomains?: string[],
  reasoningEffort: "low" | "medium" | "high" | "xhigh",
  searchContextSize: "low" | "medium" | "high",
  serviceTier: "priority",
  sourceUrls: string[]
}
```

Do **not** optimize for rich trace fidelity in v1.

## Nested Codex request strategy

### Model selection

The implemented behavior is stricter:

1. require the current pi session model to be `openai-codex/...`
2. use that exact active model for the nested search call

Rationale:

- the nested request should match the current Codex session model exactly
- this keeps search behavior aligned with the user's active Codex session
- no fallback model is used

## Request flow

Use `completeSimple()` from `@mariozechner/pi-ai` with the resolved `openai-codex` model.

The extension should build a nested prompt like:

```text
You are a web research helper.
Use the native web_search tool to answer the user's query.
Return a concise answer followed by a Sources section.
Always include source URLs as markdown links.
```

User message:

```text
Search the web for: <query>
```

## Injecting the native `web_search` tool

pi's public tool abstraction is function-tool-based, so the extension should use `onPayload` to replace the outgoing request body's `tools` field with the native Codex/OpenAI tool.

Target payload shape:

```json
{
  "tools": [
    {
      "type": "web_search",
      "external_web_access": true,
      "filters": {
        "allowed_domains": ["example.com"]
      },
      "search_context_size": "high"
    }
  ]
}
```

Notes:

- always set `external_web_access: true`
- omit `filters` when `allowed_domains` is not provided
- default `search_context_size` to `"high"`
- keep `tool_choice: "auto"`
- always set `service_tier: "priority"`
- no custom function tools are needed in the nested call

## Why `onPayload`

This is the smallest change that lets us use the same native tool Codex uses while staying inside pi's existing provider stack.

It lets pi continue to handle:

- Codex endpoint selection
- Codex OAuth token usage
- Codex-specific headers
- retries / streaming / transport behavior already implemented by pi-ai

## Expected behavior from pi-ai parsing

The nested Codex request may emit internal `web_search_call` items, but pi's current Responses/Codex parsing does not expose those as first-class content blocks.

That is acceptable for v1 because we only need:

- final answer text
- source URLs

We do **not** need to surface `search` / `openPage` / `findInPage` traces.

## Error handling

The tool should fail clearly in these cases:

### No Codex auth available

If `ctx.modelRegistry.getApiKeyAndHeaders(model)` fails or returns no API key:

- throw a user-facing error instructing the user to log into `openai-codex`
- do not silently fall back to a different provider

### No `openai-codex` model available

Throw a clear error if the model registry cannot find any usable `openai-codex` model.

### Nested request aborted

Pass `signal` through so Esc/abort cancels the nested request cleanly.

### No sources returned

Do not fail the tool purely because source extraction is imperfect.

Instead:

- return the raw answer if needed
- include any URLs we can recover from the answer text
- record the missing-source condition in `details`

## Suggested prompt contract for the nested call

The nested prompt should be strict and simple:

```text
You are a web research helper.
Use the native web_search tool when needed.
Answer the query concisely.
After the answer, include a section exactly named "Sources:".
Under it, list the relevant source URLs as markdown bullets.
Do not omit the Sources section.
```

This is enough for v1. We should not over-engineer source extraction before seeing real failures.

## Suggested implementation breakdown

### Phase 1: minimal working tool

- create a standalone extension tool `web_search`
- resolve `openai-codex` model + auth from `ctx.modelRegistry`
- make a nested `completeSimple()` call
- inject native `web_search` via `onPayload`
- return final answer text with `Sources:`

### Phase 2: output cleanup

- extract source URLs into `details`
- improve nested prompt wording
- tighten error messages
- add custom `renderCall` / `renderResult` only if useful

### Phase 3: optional configurability

- optional domain filtering UI / defaults
- optional user-location and content-type controls if later needed
- optional transport/session prewarm work if latency still needs improvement

## Suggested file layout

Keep the first implementation isolated.

Recommended shape:

```text
<extension file>
  registerTool("web_search")
  resolveCodexModel(ctx)
  runCodexWebSearch(model, auth, query, allowedDomains, signal)
  normalizeWebSearchResult(text)
```

The exact location can be decided later, but the implementation should stay in one dedicated file first.

## Manual verification checklist

### Auth reuse

- log into pi with `openai-codex`
- call the tool while the outer agent is using another provider
- verify the nested call still succeeds
- verify expired OAuth credentials refresh automatically

### Search correctness

Compare pi tool output against Codex for prompts like:

- "latest React 19 docs changes"
- "Rust 1.90 release notes"
- "What changed in Bun's package manager recently?"

We are looking for:

- the same general factual coverage
- plausible overlap in sources
- not identical wording

### Domain filtering

- run unrestricted search
- run `allowed_domains: ["react.dev"]`
- verify sources stay within the allowed set

### Abort behavior

- trigger a search
- abort during nested request
- verify tool exits cleanly and does not hang the outer session

## Test cases to add later

When we implement this, add tests for:

- model resolution order
- no-auth failure path
- `onPayload` tool injection
- `allowed_domains` mapping to `filters.allowed_domains`
- result normalization for `Sources:` extraction
- abort propagation

## Open questions

### Do we need a separate `web_fetch` tool?

Not for v1.

If the goal is "let pi learn the same web information Codex could get," native Codex `web_search` is enough for the first version.

### Do we need `search` / `openPage` / `findInPage` traces?

No.

They are mostly useful for provenance/debugging, not for getting better web content into the outer pi agent.

### Do we need to patch pi core later?

Probably not unless we want:

- first-class native web-search rendering
- exact Codex-style trace display
- provider-native tools across all normal agent turns

## Final recommendation

Build a **local pi extension tool** named `web_search` that performs a **nested `openai-codex` call** using the user's existing pi OAuth login and injects the native Codex/OpenAI `web_search` tool via `onPayload`.

That gives us:

- the same underlying web backend as Codex
- clean integration with pi's tool model
- zero pi-core architecture churn
- a simple path to iterate later if we want richer traces or a separate fetch tool

## References

Relevant files examined while making this plan:

- `pi-mono/packages/coding-agent/docs/extensions.md`
- `pi-mono/packages/coding-agent/docs/custom-provider.md`
- `pi-mono/packages/coding-agent/docs/providers.md`
- `pi-mono/packages/coding-agent/examples/extensions/custom-provider-anthropic/index.ts`
- `pi-mono/packages/ai/src/providers/openai-codex-responses.ts`
- `pi-mono/packages/ai/src/models.generated.ts`
- `pi-mono/packages/coding-agent/src/core/model-registry.ts`
- `pi-mono/packages/coding-agent/src/core/auth-storage.ts`
- `~/codex/codex-rs/core/src/tools/spec.rs`
- `~/codex/codex-rs/core/tests/suite/web_search.rs`
- `~/codex/codex-rs/app-server-protocol/schema/typescript/v2/WebSearchAction.ts`
