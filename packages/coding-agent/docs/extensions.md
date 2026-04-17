# Extensions

Extensions are TypeScript modules that plug into the coding-agent core. They work
across every transport (interactive TUI, RPC, print mode). They can:

- Subscribe to lifecycle events
- Register LLM-callable tools
- Register custom model providers
- Register prompt sources / context fragments (via `resources_discover`)
- Persist state across sessions

Extensions are deliberately **transport-agnostic**. They have no access to TUI
widgets, dialogs, custom footers/headers, slash commands, shortcuts, or any other
terminal-only UX. Relay ships with its own TUI and we expect TUI UX to be built
directly into the host, not plugged in via extensions.

> **Placement for `/reload`:** Put extensions in `~/.pi/agent/extensions/` (global)
> or `.pi/extensions/` (project-local) for auto-discovery. Use `pi -e ./path.ts`
> only for quick tests.

## Quick Start

Create `~/.pi/agent/extensions/my-extension.ts`:

```typescript
import type { ExtensionAPI } from "@pi-relay/coding-agent";
import { Type } from "@sinclair/typebox";

export default function (pi: ExtensionAPI) {
  // React to events
  pi.on("session_start", async (_event, ctx) => {
    console.log(`Session started in ${ctx.cwd}`);
  });

  // Block dangerous tool calls
  pi.on("tool_call", async (event) => {
    if (event.toolName === "bash" && event.input.command?.includes("rm -rf")) {
      return { block: true, reason: "Refusing destructive command" };
    }
  });

  // Register a custom tool
  pi.registerTool({
    name: "greet",
    label: "Greet",
    description: "Greet someone by name",
    parameters: Type.Object({
      name: Type.String({ description: "Name to greet" }),
    }),
    async execute(toolCallId, params, signal, onUpdate, ctx) {
      return {
        content: [{ type: "text", text: `Hello, ${params.name}!` }],
        details: {},
      };
    },
  });
}
```

## ExtensionContext

Event handlers and tool `execute` callbacks receive an `ExtensionContext`:

```typescript
interface ExtensionContext {
  cwd: string;
  sessionManager: ReadonlySessionManager;
  modelRegistry: ModelRegistry;
  model: Model<any> | undefined;
  isIdle(): boolean;
  signal: AbortSignal | undefined;
  abort(): void;
  hasPendingMessages(): boolean;
  shutdown(): void;
  getContextUsage(): ContextUsage | undefined;
  compact(options?: CompactOptions): void;
  getSystemPrompt(): string;
}
```

## Events

Subscribe with `pi.on(eventName, handler)`.

### Resource Events

- `resources_discover` — after session start, return additional skill/prompt/theme paths:
  ```typescript
  pi.on("resources_discover", async (_event, ctx) => ({
    skillPaths: [`${ctx.cwd}/.my-ext/skills`],
    promptPaths: [],
    themePaths: [],
  }));
  ```

### Session Events

| Event | When | Can cancel |
|-------|------|------------|
| `session_start` | A session is started/loaded/reloaded. `reason`: `"startup" \| "reload" \| "new" \| "resume" \| "fork"` | no |
| `session_before_switch` | Before switching to another session file | yes |
| `session_before_fork` | Before forking from an entry | yes |
| `session_before_compact` | Before context compaction | yes / can replace compaction |
| `session_compact` | After compaction completes | no |
| `session_before_tree` | Before session-tree navigation | yes / can supply summary |
| `session_tree` | After session-tree navigation | no |
| `session_shutdown` | Graceful process shutdown (Ctrl+C/D, SIGHUP, SIGTERM) | no |

### Agent / Turn Events

| Event | When |
|-------|------|
| `before_agent_start` | After the user submits a prompt, before the agent loop begins. Can inject custom messages or replace the system prompt. |
| `agent_start` | Agent loop starts |
| `agent_end` | Agent loop ends |
| `turn_start` / `turn_end` | Per-LLM-turn |
| `message_start` / `message_update` / `message_end` | Streaming message lifecycle |
| `tool_execution_start` / `tool_execution_update` / `tool_execution_end` | Tool execution lifecycle |
| `model_select` | A new model is selected |
| `context` | Before each LLM call. Can mutate / replace the outgoing message list. |
| `before_provider_request` | Before a raw provider request is sent. Can replace the payload. |
| `input` | User input arrives from any source (interactive / rpc / extension). Return `{ action: "transform", text, images }` to rewrite, or `{ action: "handled" }` to short-circuit. |

### Tool Events

- `tool_call` — before a tool runs. Return `{ block: true, reason }` to refuse. Mutate `event.input` in place to rewrite arguments.
- `tool_result` — after a tool finishes. Return `{ content, details, isError }` to replace the result.

Built-in tool names narrow automatically. For custom tools, use the
`isToolCallEventType<"my_tool", MyToolInput>(...)` type guard.

## Tool Registration

```typescript
pi.registerTool({
  name: "my_tool",
  label: "My Tool",
  description: "Description for the LLM",
  promptSnippet: "Call my_tool to ...",  // optional
  promptGuidelines: ["Do X before calling my_tool"],  // optional
  parameters: Type.Object({ ... }),
  async execute(toolCallId, params, signal, onUpdate, ctx) {
    return { content: [{ type: "text", text: "Done" }], details: {} };
  },
});
```

`defineTool(...)` is a type-preserving wrapper when assigning to a variable.

## Provider Registration

```typescript
pi.registerProvider("my-proxy", {
  baseUrl: "https://proxy.example.com",
  apiKey: "PROXY_API_KEY",
  api: "anthropic-messages",
  models: [
    {
      id: "claude-sonnet-4-20250514",
      name: "Claude 4 Sonnet (proxy)",
      reasoning: false,
      input: ["text", "image"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      contextWindow: 200000,
      maxTokens: 16384,
    },
  ],
});

pi.unregisterProvider("my-proxy");
```

Provider registrations issued during extension load are queued and flushed when
the runner binds; calls after load take effect immediately.

## Actions

The following helpers delegate to the running session:

- `pi.sendMessage(message, options?)` — append a `CustomMessage` to the session (optionally triggering a turn)
- `pi.sendUserMessage(content, options?)` — send a user message (always triggers a turn)
- `pi.appendEntry(customType, data?)` — append a custom session entry for state persistence
- `pi.setSessionName(name)` / `pi.getSessionName()`
- `pi.setLabel(entryId, label)` — attach a label to a session entry
- `pi.exec(command, args, options?)` — run a shell command
- `pi.getActiveTools()` / `pi.getAllTools()` / `pi.setActiveTools(names)`
- `pi.setModel(model)` / `pi.getThinkingLevel()` / `pi.setThinkingLevel(level)`
- `pi.events` — shared `EventBus` for inter-extension communication

## Available Imports

Extensions are loaded via jiti, so top-level imports from these packages are
already bundled:

- `@pi-relay/coding-agent` — `ExtensionAPI`, `ToolDefinition`, `defineTool`, session/tool types
- `@pi-relay/ai` — model types, provider primitives
- `@pi-relay/agent-core` — `AgentMessage`, `AgentToolResult`, `ThinkingLevel`
- `@pi-relay/tui` — `Component` (used only as a type in tool-renderer signatures)
- `@sinclair/typebox` — `Type.Object(...)`, etc.

Extensions that need anything outside this set should live under a subdirectory
with a `package.json` and `index.ts`, and list their dependencies explicitly
(see `examples/extensions/with-deps/`).

## State Persistence

For state that needs to survive restarts:

```typescript
// Store state via a tool result's `details`
return {
  content: [{ type: "text", text: "Done" }],
  details: { todos, nextId },
};

// Rehydrate on session_start
pi.on("session_start", async (_event, ctx) => {
  for (const entry of ctx.sessionManager.getBranch()) {
    if (entry.type === "message" && entry.message.toolName === "my_tool") {
      const saved = entry.message.details;
      // ...
    }
  }
});
```

Or use `pi.appendEntry(customType, data)` to write a typed custom entry.

## Error Handling

Errors thrown inside handlers are captured by the runner and surfaced via the
error listener the host registers. Extensions should prefer returning
`{ block: true, reason }` or similar structured refusals over throwing.

## Mode Behavior

Every event and every API method is available in every mode. Extensions do not
branch on transport. If a feature (e.g. per-user confirmation UI) needs the
terminal, build it into the TUI host instead.

## Examples

See [`../examples/extensions/`](../examples/extensions/):

- `hello.ts` — minimal custom tool
- `built-in-tool-renderer.ts` — custom rendering for built-in tools
- `minimal-mode.ts` — override rendering for a minimal display
- `antigravity-image-gen.ts` — image generation tool
- `bash-spawn-hook.ts` — hook bash child-process spawns
- `provider-payload.ts` — inspect/modify provider request payloads
- `truncated-tool.ts` — grep tool with bounded output
- `dynamic-resources/` — `resources_discover` returning skills/prompts/themes
- `custom-provider-*` — custom model providers (Anthropic, GitLab Duo, Qwen CLI)
- `with-deps/` — an extension bundling its own package.json
