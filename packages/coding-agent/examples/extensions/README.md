# Extension Examples

Core-extension examples for pi-coding-agent. Core extensions work in any transport
(interactive TUI, RPC, print mode). They can subscribe to lifecycle events, register
LLM tools, register prompt sources, and register custom model providers.

## Usage

```bash
# Load an extension with --extension flag
pi --extension examples/extensions/hello.ts

# Or copy to extensions directory for auto-discovery
cp hello.ts ~/.pi/agent/extensions/
```

## Examples

### Custom Tools

| Extension | Description |
|-----------|-------------|
| `hello.ts` | Minimal custom tool example |
| `built-in-tool-renderer.ts` | Custom compact rendering for built-in tools (read, bash, edit, write) while keeping original behavior |
| `minimal-mode.ts` | Override built-in tool rendering for minimal display |
| `truncated-tool.ts` | Wraps ripgrep with proper output truncation |
| `antigravity-image-gen.ts` | Generate images via Google Antigravity with optional save-to-disk modes |
| `bash-spawn-hook.ts` | Demonstrates hooking bash child-process spawns via a registered hook |
| `provider-payload.ts` | Inspect/modify provider request payloads via `before_provider_request` |

### Resources

| Extension | Description |
|-----------|-------------|
| `dynamic-resources/` | Loads skills, prompts, and themes using `resources_discover` |

### Custom Providers

| Extension | Description |
|-----------|-------------|
| `custom-provider-anthropic/` | Custom Anthropic provider with OAuth support and custom streaming implementation |
| `custom-provider-gitlab-duo/` | GitLab Duo provider using pi-ai's built-in Anthropic/OpenAI streaming via proxy |
| `custom-provider-qwen-cli/` | Qwen CLI provider with OAuth device flow and OpenAI-compatible models |

### External Dependencies

| Extension | Description |
|-----------|-------------|
| `with-deps/` | Extension with its own package.json and dependencies (demonstrates jiti module resolution) |

## Writing Extensions

See [docs/extensions.md](../../docs/extensions.md) for full documentation.

```typescript
import type { ExtensionAPI } from "@pi-relay/coding-agent";
import { Type } from "@sinclair/typebox";

export default function (pi: ExtensionAPI) {
  // Subscribe to lifecycle events
  pi.on("tool_call", async (event, ctx) => {
    if (event.toolName === "bash" && event.input.command?.includes("rm -rf")) {
      return { block: true, reason: "Blocked by policy" };
    }
  });

  // Register custom tools
  pi.registerTool({
    name: "greet",
    label: "Greeting",
    description: "Generate a greeting",
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
