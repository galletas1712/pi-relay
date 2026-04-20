/**
 * Helpers invoked by `createExtensionAPI.registerToolProvider` and
 * `createExtensionAPI.configureTools` (see `../extensions/loader.ts`).
 *
 * Routes all registrations into the shared `ToolRegistry` on the runtime so
 * providers declared across different extensions participate in the same
 * resolution pass.
 */

import type { ToolProvider, ToolsConfig } from "@pi-relay/tool-kit";
import type { Extension } from "../extensions/types.js";
import type { ToolRegistry } from "./tools.js";

/**
 * Register a provider into the shared `ToolRegistry` and record it on the
 * extension. `provider.id` / `provider.implements` validation lives in
 * `ToolRegistry.registerProvider` — don't duplicate it here.
 */
export function registerToolProviderInExtension(
	extension: Extension,
	registry: ToolRegistry,
	provider: ToolProvider,
	warn: (message: string) => void,
): void {
	if (extension.toolProviders?.has(provider.id)) {
		warn(
			`registerToolProvider: duplicate provider id "${provider.id}" in ${extension.path}; ignoring second registration.`,
		);
		return;
	}

	registry.registerProvider(provider, extension.path);

	extension.toolProviders?.set(provider.id, {
		provider,
		sourceInfo: extension.sourceInfo,
	});
}

/** Merge a `ToolsConfig` object into the shared registry. */
export function configureToolsInRegistry(registry: ToolRegistry, config: ToolsConfig): void {
	registry.configureTools(config);
}
