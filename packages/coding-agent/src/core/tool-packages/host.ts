/**
 * Build a `ToolHost` from an `ExtensionContext`.
 *
 * `ToolHost` is the small surface @pi-relay/tool-kit exposes to providers.
 * This adapter maps it onto the existing `ExtensionContext` services so
 * providers never import coding-agent internals directly.
 */

import type { Api, Model } from "@pi-relay/ai";
import type { ToolHost, ToolHostAuth, ToolHostModelRef } from "@pi-relay/tool-kit";
import type { ExtensionContext } from "../extensions/types.js";

/**
 * Build a `ToolHost` around an already-resolved `ExtensionContext`.
 *
 * Callers that need a host per tool call should construct it lazily via
 * `createToolHostFactory(() => runner.createContext())` instead so each call
 * sees the current model / signal.
 */
export function createToolHost(ctx: ExtensionContext): ToolHost {
	return {
		getModel(): ToolHostModelRef | undefined {
			const model = ctx.model;
			if (!model) return undefined;
			return { id: model.id, provider: model.provider, native: model };
		},
		async getApiKey(provider: string): Promise<ToolHostAuth> {
			const model = ctx.model;
			if (!model) {
				return { ok: false, error: "No active model in the current pi session." };
			}
			if (model.provider !== provider) {
				return {
					ok: false,
					error: `Current model is ${model.provider}/${model.id}, not ${provider}. Switch models or use a tool-owned credential.`,
				};
			}
			const auth = await ctx.modelRegistry.getApiKeyAndHeaders(model as Model<Api>);
			if (!auth.ok) {
				return { ok: false, error: auth.error };
			}
			return { ok: true, apiKey: auth.apiKey, headers: auth.headers };
		},
		// Node 20+ has a global fetch; providers may monkey-patch this in
		// tests by passing a different host. Use the global directly (no
		// `.bind`) so tests can stub `globalThis.fetch` after host creation.
		http: globalThis.fetch,
		// Escape hatch for in-tree providers that still need direct
		// sessionManager / ExtensionContext access during the migration.
		// Kept intentionally untyped on the author side
		// (Record<string, unknown>) so third-party providers don't build on it.
		native: {
			sessionManager: ctx.sessionManager,
			extensionContext: ctx,
		},
	};
}

/**
 * Lazy host factory. The factory is invoked at tool-call time so each call
 * sees the current model / abort signal.
 */
export function createToolHostFactory(ctxFactory: () => ExtensionContext): () => ToolHost {
	return () => createToolHost(ctxFactory());
}
