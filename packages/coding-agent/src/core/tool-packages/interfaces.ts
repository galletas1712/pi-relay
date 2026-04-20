/**
 * Built-in `ToolInterface` registry.
 *
 * Interfaces are contracts the LLM sees (name, description, parameters,
 * optional result shape). Providers implement an interface by referencing it
 * via `ToolProvider.implements`.
 *
 * This registry declares:
 * - `web_search` — fully wired in this PR; used by codex-web-search and
 *   perplexity-sonar providers.
 * - `bash` — declared only as a stub. The built-in bash tool in `agent-core`
 *   is NOT yet routed through a provider; it's declared here so future
 *   milestones can migrate it without reshaping the interface surface.
 *
 * Third-party authors can declare their own interfaces in the same shape via
 * `defineToolInterface` from @pi-relay/tool-kit, and register a provider that
 * implements the new name. The resolver only requires that `implements`
 * matches *some* registered interface (built-in or otherwise).
 */

import type { ToolInterface } from "@pi-relay/tool-kit";
import { Type } from "@sinclair/typebox";

/**
 * web_search — return a cited answer or list of search results for a query.
 *
 * Result shape is intentionally loose so providers can return richer details
 * in the `details` channel without the interface having to evolve.
 */
export const webSearchInterface: ToolInterface = {
	name: "web_search",
	description: "Search the web and return a concise, cited answer.",
	parameters: Type.Object({
		query: Type.String({ minLength: 1, description: "The web search query." }),
	}),
	resultShape: Type.Object({
		answer: Type.Optional(Type.String()),
		results: Type.Optional(
			Type.Array(
				Type.Object({
					title: Type.Optional(Type.String()),
					url: Type.String(),
					snippet: Type.Optional(Type.String()),
				}),
			),
		),
		citations: Type.Optional(Type.Array(Type.String())),
	}),
	promptSnippet: "- web_search: search the web for up-to-date information",
	promptGuidelines: [
		"Use web_search when the user needs current information, docs, release notes, or anything outside the repo.",
		"Prefer web_search over ad-hoc bash / browser-style lookups for general web research.",
	],
};

/**
 * bash — run a shell command.
 *
 * Stub declaration only. The built-in bash tool in `agent-core` is not yet
 * provider-backed; this interface exists so later milestones can migrate
 * bash to a provider model (e.g. `bash.local`, `bash.ssh`, `bash.docker`).
 *
 * The actual schema used by the built-in today lives in
 * `packages/agent-core/src/tools/bash.ts`. Keep in sync when wiring the
 * migration.
 */
export const bashInterface: ToolInterface = {
	name: "bash",
	description: "Run a shell command and return its output.",
	parameters: Type.Object({
		command: Type.String({ minLength: 1, description: "The shell command to run." }),
	}),
};

/**
 * All built-in interface declarations, keyed by interface name.
 */
export const builtinToolInterfaces: ReadonlyMap<string, ToolInterface> = new Map<string, ToolInterface>([
	[webSearchInterface.name, webSearchInterface],
	[bashInterface.name, bashInterface],
]);

/**
 * Mutable registry: built-ins + any interfaces third-party code declares at
 * runtime. Third-party registration is not wired through `ExtensionAPI` in
 * this milestone; the registry is exposed so later code paths can add
 * `pi.registerToolInterface(iface)` without a type-surface change here.
 */
export class ToolInterfaceRegistry {
	private readonly interfaces = new Map<string, ToolInterface>();

	constructor(seed: ReadonlyMap<string, ToolInterface> = builtinToolInterfaces) {
		for (const [name, iface] of seed) {
			this.interfaces.set(name, iface);
		}
	}

	get(name: string): ToolInterface | undefined {
		return this.interfaces.get(name);
	}

	has(name: string): boolean {
		return this.interfaces.has(name);
	}

	register(iface: ToolInterface): void {
		this.interfaces.set(iface.name, iface);
	}

	names(): string[] {
		return Array.from(this.interfaces.keys());
	}
}

/** Default shared registry seeded with the built-in interfaces. */
export const defaultToolInterfaceRegistry = new ToolInterfaceRegistry();
