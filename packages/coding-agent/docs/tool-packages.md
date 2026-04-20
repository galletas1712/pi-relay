# Tool interfaces, providers, and tools (internal design)

`@pi-relay/tool-kit` introduces a modular authoring surface for pi-relay
tools. This document describes the **internal** plumbing that turns a
`ToolProvider` into the `ToolDefinition`s the rest of pi-relay already
understands.

Milestone 1 (this PR) is intentionally narrow: the tool-kit author
surface, the interface registry + tool resolver, a small `ExtensionAPI`
delta, and two in-tree providers (`codex-web-search`, `perplexity-sonar`)
shipped inside `@pi-relay/extensions`. Later milestones add persisted
config/secrets, first-use UX, login flows, and lifecycle hooks. See
"Upgrade path" below.

## Two layers

- **`ToolInterface`** — contract. Name, description, TypeBox parameters,
  optional `resultShape`. Owned by pi-relay core for built-in tools;
  third-party code can declare its own via `defineToolInterface`.
  Interfaces live in
  `packages/coding-agent/src/core/tool-packages/interfaces.ts`:
  - `web_search` — fully wired in this PR; implemented by the
    codex-web-search and perplexity-sonar providers.
  - `bash` — stub only. The built-in bash tool in `agent-core` is not
    yet provider-backed; the interface is declared here so later
    milestones can migrate it without reshaping the public surface.

- **`ToolProvider`** — implementation. `id`, `implements` (interface
  name), `defaultConfig`, `secrets`, optional `parameters` override,
  `execute(params, ctx)`. One provider per package. Provider ids are
  **never** surfaced to the LLM — they only appear in
  `pi.configureTools({...})` and diagnostics.

- **`ToolsConfig`** (user-authored) — `Record<toolName,
  ToolConfigEntry>`. The key is what the LLM sees; the value picks
  which provider backs it. One tool entry = one LLM-visible tool name =
  one provider.

## Architecture

```
extensions/my-provider.ts           <-- third-party author code
  └─ defineToolProvider({ id, implements: "web_search", ... })
  └─ export default (pi) => {
        pi.registerToolProvider(provider);
        // optional, to pin provider selection:
        // pi.configureTools({ web_search: { provider: provider.id } });
     }

packages/coding-agent/src/core/extensions/loader.ts
  └─ createExtensionAPI(...).registerToolProvider(provider)
       └─ registerToolProviderInExtension(extension, runtime.toolRegistry, provider, warn)
       └─ runtime.refreshTools()

  └─ createExtensionAPI(...).configureTools(config)
       └─ configureToolsInRegistry(runtime.toolRegistry, config)
       └─ runtime.refreshTools()

packages/coding-agent/src/core/tool-packages/tools.ts
  └─ ToolRegistry.resolve()   (shared across all extensions)
       └─ returns ToolDefinition[] — one per resolved tool entry

packages/coding-agent/src/core/extensions/runner.ts
  └─ getAllRegisteredTools()  → union of extension.tools + ToolRegistry.resolve()

packages/coding-agent/src/core/agent-session.ts
  └─ _refreshToolRegistry() wires those ToolDefinition[]s into the agent loop
     (same path as classic registerTool tools — no agent-core changes).
```

Key points:

- **The `ToolRegistry` is shared**, not per-extension. Providers
  registered in different extensions all participate in the same
  resolution pass. A pack extension can register providers and a later
  user extension can pin the provider selection via `configureTools`,
  and the two see the same underlying registry.
- **Classic `pi.registerTool(...)` is unchanged** and takes priority
  over provider-derived tools with the same name. Third-party code
  using the classic path keeps working.
- **Context resolution is lazy.** Each produced
  `ToolDefinition.execute` is handed the `ExtensionContext` at call
  time, from which it builds a fresh `ToolHost`. This mirrors the
  existing `wrapToolDefinition(ctxFactory)` pattern and guarantees each
  call sees the current model / signal.
- **`extension.toolProviders`** is a new optional `Map<string,
  RegisteredToolProvider>` on `Extension` for diagnostics. The actual
  `ToolDefinition`s the agent runs live on the shared registry; the
  per-extension map is for ownership / `/tools` reporting.

## Resolution rules

Implemented by `ToolRegistry.resolve()` in
`packages/coding-agent/src/core/tool-packages/tools.ts`.

**Pass 1 — user-configured tools.** For every `toolName -> { provider,
config? }` in the merged `ToolsConfig`:

- Provider id unknown → **throw** with the list of registered provider
  ids.
- Provider implements an interface not in the `ToolInterfaceRegistry` →
  warn + skip that tool entry (the provider was registered but the
  interface hasn't been declared, so the call contract is undefined).
- Otherwise → emit a `ToolDefinition` with `name = toolName` and
  `execute` bound to the chosen provider.

**Pass 2 — auto-bind the rest.** For every interface that has registered
providers not already claimed by pass 1, and no user-chosen tool entry
for the bare interface name:

- 0 providers → skip.
- 1 provider → emit a `ToolDefinition` under the bare interface name.
- ≥ 2 providers → **throw** at resolve time listing the candidate
  provider ids. No silent "first wins". Prevents the LLM from seeing an
  unpredictable default when multiple packs register the same interface.

Config merge order for a resolved tool:

```
providerDefault ⊕ entry.config    // (shallow merge, entry wins)
```

Required-secret validation is performed inside the generated
`ToolDefinition.execute` (not at registration time) so the adapter can
throw `ToolConfigMissingError` with a pointer at the actual
user-invoked tool call.

## `ToolCallContext.toolName`

Providers can read `ctx.toolName` to distinguish which user-named tool
invoked them. In the `bash` / `bash_prod` example, the same provider
(`bash.ssh`) could in principle back multiple tools with different
`config.host` values; `toolName` lets the provider log which tool it's
serving without needing to reverse-engineer from config.

## `ToolHost` escape hatches

`ToolHost.native` and `ToolHostModelRef.native` are typed as opaque
(`Record<string, unknown>` / `unknown`) so in-tree providers can reach
`ExtensionContext` / `Model<Api>` during the migration without
broadening the author-facing public surface.

Third-party providers should rely only on `host.http`,
`host.getApiKey(provider)`, config, and env-backed secrets. The
`native` field is intentionally unstable.

## Discovery

Extensions reach the loader from three independent sources, unioned
into a single list before `loadExtensions(...)` is called:

1. **Settings files** — `Settings.extensions: string[]`. Merged from
   `~/.pi/agent/settings.json` (user scope) and
   `<cwd>/.pi/settings.json` (project scope). Strings may be file paths
   or bare package names.
2. **CLI flag** — `-e <value>` (repeatable). Same resolution rules as
   settings.
3. **Auto-discovery** — `<cwd>/.pi/extensions/` and
   `<agentDir>/extensions/` (default `~/.pi/agent/extensions/`) are
   scanned for `*.ts` / `*.js` files, `<subdir>/index.{ts,js}`, and
   `<subdir>/package.json` with a `pi.extensions` manifest.

### Resolution algorithm (per settings / CLI entry)

In order:

1. If the entry is an object `{ path: "..." }` → always a file path.
2. If the entry is an object `{ package: "..." }` → always a package
   name.
3. If the entry is a string starting with `.`, `/`, or `~` → file path.
4. If the entry is a string with a `npm:` / `git:` / `http(s):` /
   `ssh:` / `github:` / `file:` scheme → file path (a different loader
   layer handles these).
5. Otherwise → bare package name, resolved via
   `import.meta.resolve(value, <cwd>/package.json)`, then
   `createRequire(...)`'s resolver, then a manual walk up
   `node_modules/<pkg>/package.json`. The first hit wins.

Package resolution failures emit a per-extension diagnostic and skip;
they never crash the session. The resolved `Extension`'s
`sourceInfo.source` is set to `"package"` for package-sourced
extensions so `/tools` / logs / diagnostics can distinguish them from
raw files.

### Extension packs

The recommended shipping shape for any tool beyond a one-off script is
an **extension pack** — an npm package whose default export is an
`ExtensionAPI` factory that registers every extension the package
contributes. `packages/extensions/` in this repo is the reference pack.
Users install the package and add one line to `settings.json`:

```jsonc
{ "extensions": ["@your-org/pi-tools"] }
```

Under the hood a pack's `src/index.ts` just calls each sub-extension's
factory against the same `pi`, and optionally calls `pi.configureTools`
to express a default opinion. Nothing in the registry knows or cares
about the pack boundary; the pack is purely a packaging convenience.

## `configureTools` merge semantics

`pi.configureTools(...)` can be called multiple times, both within a
single extension and across different extensions. The registry merges
per tool name:

- New name → added.
- Existing name with the same provider → replaced silently (only
  `config` may have changed).
- Existing name with a different provider → replaced **with a warning**
  that names the previous and new provider ids.

This lets a pack express a default (`{ web_search: { provider:
"com.perplexity.sonar" } }`) while letting a user extension loaded
later override it (`{ web_search: { provider:
"com.openai.codex.web-search" } }`) without ambiguity.

## Upgrade path (post-milestone-1)

In priority order:

1. **`SettingsManager.getTools` / `setTools`.** Persist `ToolsConfig`
   under `settings.json`'s `tools` root; wire into the resolver so
   `pi.configureTools(...)` isn't the only input.
2. **`AuthStorage` `tools.<toolName>` namespace.** Same file-lock /
   `0600` semantics as the provider-keyed store; wire into the secret
   resolver.
3. **`/configure <toolName>` slash command.** Walks `configSchema` +
   `secrets` via `ctx.ui.prompt`. First-use UX when a provider throws
   `ToolConfigMissingError`.
4. **OAuth-style login for providers.** `LoginCallbacks` is already
   defined in tool-kit; wire a `login(callbacks): Promise<Credentials>`
   hook onto `ToolProvider` and surface it in `/login`.
5. **Auto-discovery of duck-typed default exports.** When
   `isToolProvider` returns true on a module's default export, route it
   through `registerToolProvider` instead of treating it as an
   `ExtensionFactory`.
6. **`init` / `dispose` lifecycle.** Let providers set up pooled HTTP
   clients and clean up on shutdown. The types reserve
   `ToolProvider.init/dispose` today.
7. **Tool-name collision handling.** Today a resolver-produced name
   can technically collide with a classic `pi.registerTool(...)`
   registration; classic wins (first-wins). Add a `/tools` diagnostic
   and, if needed, a rewriting mode behind a setting once real authors
   hit it.
8. **Migrate built-in tools to providers.** Convert the `bash` /
   `read` / `edit` / `write` / `grep` / `find` / `ls` built-ins into
   first-party providers registered at session startup. This also
   unlocks user-bindable alternatives (`bash.ssh`, `read.remote`, etc.).

## Not in milestone 1

- Persisted config and secrets (see 1 and 2 above).
- Any UI for configure / login (see 3 and 4).
- Lifecycle hooks (see 6).
- Tool-name collision rewriting (see 7).
- Migration of built-in tools (see 8).
- Other extension types beyond tool providers (TUI extensions,
  commands, hooks). `@pi-relay/extensions`'s layout reserves space
  (`src/tui/`, `src/commands/`, `src/hooks/`); nothing in the runtime
  is wired for them yet.
