/**
 * @pi-relay/extensions — first-party extension pack for pi-relay.
 *
 * An "extension pack" is a single package that bundles multiple extensions
 * (tool providers today; TUI extensions, commands, hooks in later
 * milestones). It default-exports a single `ExtensionAPI` factory that the
 * pi-relay loader invokes once, and internally fans out to each bundled
 * sub-extension.
 *
 * Usage:
 *
 *   // from `settings.json` or `piConfig.extensions`:
 *   { "extensions": ["@pi-relay/extensions"] }
 *
 *   // from code:
 *   import registerExtensions from "@pi-relay/extensions";
 *   registerExtensions(pi);
 *
 * This package is structured for growth: each extension type lives under
 * `src/<type>/` and is wired in here. Today only `src/tools/` exists.
 */

import type { ExtensionAPI } from "@pi-relay/coding-agent";
import { registerAllTools } from "./tools/index.js";

/**
 * Pack's default `tools` opinion: if the user doesn't configure one, use
 * Perplexity as the backing provider for `web_search`. Both tool
 * providers are still registered so a user can override with:
 *
 *   pi.configureTools({ web_search: { provider: "com.openai.codex.web-search" } });
 *
 * or via a settings file once settings-backed configuration lands. This
 * avoids the "multiple providers implement web_search, no config" error
 * the resolver would otherwise throw when both are loaded.
 */
const PACK_DEFAULT_TOOLS = {
	web_search: { provider: "com.perplexity.sonar" },
} as const;

/**
 * Register every extension contributed by this pack on the given
 * `ExtensionAPI`. Safe to call more than once (the registry warns + drops
 * duplicate provider registrations; `configureTools` entries merge with
 * last-call-wins per tool name).
 */
export default async function registerExtensions(pi: ExtensionAPI): Promise<void> {
	await registerAllTools(pi);
	pi.configureTools(PACK_DEFAULT_TOOLS);
}

export { PACK_DEFAULT_TOOLS, registerAllTools };
export { registerCodexWebSearch, registerPerplexitySonar } from "./tools/index.js";
