/**
 * Bundled tool providers for @pi-relay/extensions.
 *
 * Each file under this directory is a self-contained extension: it
 * default-exports an `ExtensionAPI` factory that registers one
 * `ToolProvider` via `pi.registerToolProvider(...)`. The same module shape
 * means a file here works either:
 *
 *   - imported by `registerAllTools(pi)` below (the normal path), or
 *   - copied verbatim to `~/.pi/agent/extensions/` (the single-file path,
 *     as in the `examples/` directory at the repo root).
 */

import type { ExtensionAPI } from "@pi-relay/coding-agent";
import registerCodexWebSearch from "./codex-web-search.js";
import registerPerplexitySonar from "./perplexity-sonar.js";

/**
 * Invoke every bundled tool-provider factory against `pi`. Order doesn't
 * matter: each provider registers one `ToolProvider` whose binding is
 * derived independently by the resolver.
 */
export async function registerAllTools(pi: ExtensionAPI): Promise<void> {
	await registerCodexWebSearch(pi);
	await registerPerplexitySonar(pi);
}

export { default as registerCodexWebSearch } from "./codex-web-search.js";
export { default as registerPerplexitySonar } from "./perplexity-sonar.js";
