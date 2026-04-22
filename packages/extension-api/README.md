# @pi-relay/extension-api

Lightweight **extension authoring** surface for pi-relay.

This package gives extension authors a smaller public import path for the core
extension contracts — `ExtensionAPI`, `ExtensionFactory`, event/context types,
and `defineTool(...)` helpers — without asking them to reach into the broader
`@pi-relay/coding-agent` runtime package for every type import.

`@pi-relay/coding-agent` still re-exports the same public symbols for backward
compatibility. This package is an opt-in authoring facade, not a runtime
behavior change.

## Install

```bash
npm install @pi-relay/extension-api
```

`@pi-relay/coding-agent` remains a required peer dependency because extensions
still execute inside the coding-agent runtime.

## Example

```ts
import { defineTool, type ExtensionAPI } from "@pi-relay/extension-api";
import { Type } from "@sinclair/typebox";

const helloTool = defineTool({
  name: "hello",
  description: "Say hello",
  parameters: Type.Object({ name: Type.String() }),
  async execute(_toolCallId, params) {
    return {
      content: [{ type: "text", text: `Hello, ${params.name}!` }],
    };
  },
});

export default function registerHello(pi: ExtensionAPI) {
  pi.registerTool(helloTool);
}
```

## Scope

Use `@pi-relay/extension-api` when you only need extension-facing contracts.
Keep importing from `@pi-relay/coding-agent` for broader runtime helpers such
as UI components, session helpers, theme utilities, or built-in tool factories.
