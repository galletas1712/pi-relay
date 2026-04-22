# @pi-relay/tool-kit

Author-facing toolkit for building **pi-relay tool providers**.

pi-relay exposes tools through a two-layer model:

- **Tool** (first layer, LLM-facing): named entry in user config
  (`tools.<name>`). The LLM sees only this name. Exactly one provider
  backs it.
- **Provider** (second layer, never surfaced to the model): the
  implementation that satisfies a tool. Providers declare which
  `ToolInterface` they implement and ship their own config schema +
  secrets.

If a user wants two bash-style tools (local + remote), they declare two
tools with **different names**, each pointing at a different provider that
implements the `bash` interface:

```jsonc
// settings.json / piConfig / pi.configureTools({...})
{
  "tools": {
    "bash":      { "provider": "local" },
    "bash_prod": { "provider": "ssh", "config": { "host": "prod.example.com" } }
  }
}
```

The LLM sees `bash` and `bash_prod`. Provider ids (`local`, `ssh`) stay
internal.

The main entry of this package is **TUI-free** and has **no dependency on
`@pi-relay/*` runtime code**. You can author a provider with nothing but
`@sinclair/typebox` and `@pi-relay/tool-kit` on your import graph.

## Install

```bash
npm install @pi-relay/tool-kit @pi-relay/extension-api @sinclair/typebox
```

`@pi-relay/tui` is declared as an **optional peer dependency** used only by
the `@pi-relay/tool-kit/render` subpath. If your provider doesn't need a
custom renderer, you can skip it.

If your provider is packaged as a pi-relay extension, import `ExtensionAPI`
from `@pi-relay/extension-api`.

## Minimal example

```ts
// my-provider.ts
import { defineToolProvider, type ToolCallContext } from "@pi-relay/tool-kit";
import { Type } from "@sinclair/typebox";
import type { ExtensionAPI } from "@pi-relay/extension-api";

interface Config {
  endpoint: string;
}

interface Secrets {
  apiKey: string;
}

const provider = defineToolProvider<Config, Secrets>({
  id: "com.example.search",
  implements: "web_search", // built-in ToolInterface
  displayName: "Example Search",
  version: "0.1.0",
  defaultConfig: { endpoint: "https://api.example.com/v1/search" },
  secrets: [
    { key: "apiKey", displayName: "Example API Key", kind: "api_key", envVar: "EXAMPLE_API_KEY" },
  ],
  parameters: Type.Object({ query: Type.String({ minLength: 1 }) }),
  async execute(params, ctx: ToolCallContext<{ query: string }, Config, Secrets>) {
    const res = await ctx.host.http(ctx.config.endpoint, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${ctx.secrets.apiKey}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ q: params.query }),
      signal: ctx.signal,
    });
    const data = (await res.json()) as { answer: string };
    return {
      content: [{ type: "text", text: data.answer }],
      details: { query: params.query },
    };
  },
});

export default function (pi: ExtensionAPI) {
  pi.registerToolProvider(provider);
  // Optional: pin which provider backs `web_search` when multiple are
  // loaded. Omit to let the pi-relay resolver auto-bind (only valid when
  // exactly one provider implements the interface).
  // pi.configureTools({ web_search: { provider: provider.id } });
}
```

Drop this file into any of the extension directories pi scans:

- `./.pi/extensions/my-provider.ts` (repo-scoped)
- `~/.pi/agent/extensions/my-provider.ts` (user-scoped)

pi picks it up on startup. With just this one provider loaded and no
explicit `tools` entry, the LLM sees `web_search`. If another extension
also implements `web_search`, the pi-relay resolver throws at session
start with a message telling the user to add
`pi.configureTools({ web_search: { provider: "..." } })` (or a settings
entry) to disambiguate.

## Why tools + providers (and not just tools)?

A tool like `web_search` has many equally-valid implementations (Codex's
native web search, Perplexity's Sonar API, Exa, Anthropic's built-in
search, …). Earlier drafts of pi-relay's tool model asked each
integration to pick a distinct tool name (`perplexity_search`,
`exa_search`, …), but then the LLM had to know which is installed, prompt
snippets fragmented, and swapping implementations meant rewriting prompts.

Declaring `web_search` as an **interface** and shipping each integration
as a **provider** that `implements: "web_search"` fixes that. Users name
the tools they want — one or many — and pick which provider backs each
from a `tools.<name>` entry. The LLM sees a predictable, stable surface;
the user controls the routing.

## Core types

| Type | Purpose |
|---|---|
| `ToolInterface<TParams, TResult>` | Contract: name, description, TypeBox params, optional result shape. |
| `ToolProvider<TConfig, TSecrets, TParams, TDetails>` | Implementation: id + displayName + version + `implements` + config/secrets + `execute(params, ctx)`. |
| `ToolConfigEntry` | `{ provider, config? }` — one entry in the user's `ToolsConfig`. |
| `ToolsConfig` | `Record<string, ToolConfigEntry>` — map of LLM-visible tool name to chosen provider. |
| `ToolCallContext<TParams, TConfig, TSecrets>` | Runtime context passed to `execute`: `config`, `secrets`, `host`, `signal`, `onUpdate`, `cwd`, `toolCallId`, `params`, `toolName`. |
| `ToolHost` | Minimal host surface: `getModel()`, `getApiKey(provider)`, `http: typeof fetch`. |
| `SecretSpec` | Declared secret (key + displayName + kind + optional envVar). |
| `ToolResult<TDetails>` | `{ content: (TextContent \| ImageContent)[]; details: TDetails }`. |

Helpers:

- `defineToolProvider(provider)` — identity helper that preserves generics.
- `defineToolInterface(iface)` — identity helper for declaring a new interface.
- `isToolProvider(x)` — duck-type guard for discovery / routing code.
- `ToolConfigMissingError` — thrown by the adapter when a required secret is absent.

## Config vs secrets

- **Config** is structured, non-sensitive configuration (model names,
  timeouts, endpoints). Today it comes from `provider.defaultConfig`,
  with `pi.configureTools({ <name>: { config: {...} } })` supplying
  per-tool overrides. A later milestone adds a `SettingsManager`
  `tools.<toolName>` namespace and a `/configure <toolName>` slash command.
- **Secrets** are sensitive values (API keys, OAuth tokens). Today they
  are resolved from `process.env[spec.envVar]`. A later milestone adds an
  `AuthStorage` `tools.<toolName>` namespace (file-locked, `0600` on
  disk) and a login/prompt UI. Declaring a `SecretSpec` now is
  forward-compatible with that milestone.

If a required secret is not resolvable, the adapter throws
`ToolConfigMissingError` when the tool is invoked, with a message that
names the missing key and the expected env var.

## Resolution rules

For every registered interface:

- **0 providers** → nothing exposed.
- **1 provider**, no explicit `tools` entry → auto-bound under the bare
  interface name.
- **≥ 2 providers**, no explicit `tools` entry → resolver **throws** at
  session start listing the candidate provider ids. Add a
  `tools.<name> = { provider: "..." }` entry (or a settings file entry)
  to disambiguate.
- Any number of providers with explicit `tools` entries → each entry is
  exposed under the user-chosen name.

Multiple calls to `pi.configureTools(...)` merge per tool name. Second
call with a different provider for the same name wins with a warning.

## Optional custom rendering

If you want custom visual rendering for your tool, import from the render
subpath:

```ts
import { Text } from "@pi-relay/tool-kit/render";
```

`@pi-relay/tool-kit/render` re-exports `Text` / `Component` from
`@pi-relay/tui` and a minimal `ToolRenderResultOptions` shape. The main
entry stays TUI-free so providers without custom UI don't need to install
`tui`.

The `Theme` type currently lives in `@pi-relay/coding-agent`; import it
from there directly if you want full typing for `theme.fg(...)` etc.

## Loading

Providers are loaded by the same extension loader as classic extensions.
A provider file is any `.ts`/`.js` module whose default export is a
function taking `ExtensionAPI`. Inside that function, call
`pi.registerToolProvider(provider)` (you can still call
`pi.registerTool(...)` in the same file if you want; that path is
unchanged).

pi-relay resolves extension entries from three places:

1. `settings.json` `"extensions": [...]` (project-local
   `<cwd>/.pi/settings.json` + global `~/.pi/agent/settings.json`).
2. The `-e` / `--extensions` CLI flag.
3. Auto-discovery under `<cwd>/.pi/extensions/` and
   `~/.pi/agent/extensions/` (escape-hatch).

Each settings / CLI entry can be one of:

- `"./rel/path.ts"` / `"/abs/path.ts"` / `"~/path.ts"` — a filesystem
  path.
- `"@scope/pkg"` / `"pkg"` — a bare **npm package name**, resolved via
  Node module resolution from your `cwd`. This is the recommended way to
  ship tools at scale.
- `{ path: "..." }` / `{ package: "..." }` — explicit object forms for
  settings files that prefer unambiguous types.

### Recommended shipping shape

For anything beyond a quick script, ship your tools as an **extension
pack** — a normal npm package whose `main` default-exports an
`ExtensionAPI` factory that registers every tool the package contributes.
Users then install the package and add one line to `settings.json`:

```jsonc
// ~/.pi/agent/settings.json  (or <cwd>/.pi/settings.json)
{ "extensions": ["@your-org/pi-tools"] }
```

The pi-relay repo ships `@pi-relay/extensions` as the reference pack (see
`packages/extensions/`). It bundles today's built-in providers
(`codex-web-search`, `perplexity-sonar`) and reserves layout for future
extension types (TUI, commands, hooks).

### Auto-discovery escape hatch

Still works for quick experimentation — drop a file into
`<cwd>/.pi/extensions/my-tool.ts` or `~/.pi/agent/extensions/my-tool.ts`
and pi-relay picks it up without touching settings. The rules haven't
changed: direct `*.ts` / `*.js` files, `<subdir>/index.{ts,js}`, and
`<subdir>/package.json` with a `pi.extensions` manifest.

## Not in this milestone

- Persistent `tools.<name>` entries under `settings.json` / `auth.json`
  (`tools.<toolName>.provider` / `tools.<toolName>.config` / secret
  namespaces).
- `/configure <toolName>` slash command / first-use UX.
- OAuth secrets + first-use login flow (`LoginCallbacks` is declared;
  not wired).
- Auto-discovery of bare default-exported `ToolProvider`s (today you wrap
  in a one-line `ExtensionAPI` factory, as shown above).
- `init` / `dispose` lifecycle (types reserved; adapter doesn't call
  them yet).
- Tool-name collision rewriting for classic `pi.registerTool(...)`
  registrations that happen to clash with a resolver output.
- Migration of built-in `bash`/`read`/`edit`/`write`/`grep`/`find`/`ls`
  to the provider model. The `bash` interface is declared as a stub so
  the migration can happen without reshaping the public surface; the
  built-in implementation is unchanged.

See `packages/coding-agent/docs/tool-packages.md` for the internal
resolver design.
