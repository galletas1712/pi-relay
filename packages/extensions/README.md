# @pi-relay/extensions

First-party **extension pack** for pi-relay.

An extension pack is a single npm package that bundles multiple extensions and
default-exports one `ExtensionAPI` factory that registers them all. Pi-relay
loads it with a single entry in `settings.json` (or `piConfig.extensions`, or
the `-e` CLI flag) and fans the registration out internally.

Today this package ships **tool providers**. The layout reserves space for
future extension types (TUI extensions, commands, hooks) — they'll slot into
`src/<type>/` and be wired from `src/index.ts` without breaking callers.

## Install

```bash
npm install @pi-relay/extensions
```

Then add it to your pi-relay settings (`~/.pi/agent/settings.json`, project-
local `.pi/settings.json`, or `piConfig.extensions` in any `package.json`):

```json
{
  "extensions": ["@pi-relay/extensions"]
}
```

The entry is a bare package name — the pi-relay extension loader resolves it
via Node module resolution from your `cwd`. File paths (absolute, relative,
or `~/`-prefixed) still work exactly as before.

## What ships today

| Provider id                    | Implements    | Secret env var        |
|--------------------------------|---------------|-----------------------|
| `com.openai.codex.web-search`  | `web_search`  | (piggybacks on Codex) |
| `com.perplexity.sonar`         | `web_search`  | `PERPLEXITY_API_KEY`  |

Both providers implement the `web_search` interface from
`@pi-relay/tool-kit`. With this pack installed, the LLM sees a single
`web_search` tool backed by whichever provider the user has chosen.

### Default provider selection

The pack's default-export factory registers both providers AND calls
`pi.configureTools(...)` with:

```ts
{ web_search: { provider: "com.perplexity.sonar" } }
```

So the out-of-the-box behavior with no user config is:

- `web_search` tool name, backed by Perplexity. Requires
  `PERPLEXITY_API_KEY`.

To override this selection in your own project, load an extension after
`@pi-relay/extensions` that calls:

```ts
pi.configureTools({
  web_search: { provider: "com.openai.codex.web-search" },
});
```

Or, once settings-backed tool configuration lands (see the pi-relay
upgrade path), edit `~/.pi/agent/settings.json`'s `tools.web_search`.

Without this default, the resolver would throw at session start whenever
both providers are loaded, because two providers implement the same
interface and the user hasn't picked one.

## Layout

```
packages/extensions/
├── src/
│   ├── index.ts           default-export factory: registers every bundled sub-extension
│   └── tools/
│       ├── index.ts       registerAllTools(pi) + named exports
│       ├── codex-web-search.ts    one provider, one default-export ExtensionAPI factory
│       └── perplexity-sonar.ts    same
└── README.md
```

Future:

```
├── src/
│   ├── tui/               additional TUI widgets / renderers
│   ├── commands/          slash commands
│   └── hooks/             lifecycle event handlers
```

Each new extension type ships its own `register<Type>(pi)` helper that the
top-level `src/index.ts` invokes.

## Adding a new tool

1. Create `src/tools/<name>.ts`. Default-export an `(pi: ExtensionAPI) => void
   | Promise<void>` that calls `pi.registerToolProvider(provider)` exactly
   once. See `codex-web-search.ts` for the minimum shape (piggyback auth) or
   `perplexity-sonar.ts` for a provider that owns a config + secret.
2. Add it to `src/tools/index.ts`'s `registerAllTools(pi)` sequence and
   export the factory by name.
3. Build and test: `npm run build -w @pi-relay/extensions` +
   `npm test -w @pi-relay/extensions`.

Each file is also a complete standalone pi-relay extension — you can copy it
to `~/.pi/agent/extensions/` and it'll load without this package.

## Examples

See [`./examples/`](./examples/) for a categorized catalogue of ~40 example
extensions — lifecycle handlers, custom tools, commands, UI, custom
providers, etc. Single-file copies of this pack's tool providers
(`codex-web-search.ts`, `perplexity-sonar.ts`) live there too under
**Provider examples** at the top of
[`./examples/README.md`](./examples/README.md) as reference
implementations for authors who want to start from a working file.

## See also

- `@pi-relay/tool-kit` — the author-facing types (`ToolInterface`,
  `ToolProvider`, `ToolsConfig`, `ToolCallContext`, ...). Authors only depend
  on tool-kit (+ `@sinclair/typebox`); this pack depends on
  `@pi-relay/coding-agent` for the `ExtensionAPI` type.
- `packages/coding-agent/docs/tool-packages.md` — internal design of the
  interface/provider/tool model and the extension discovery algorithm.
