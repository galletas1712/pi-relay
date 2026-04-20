/**
 * Author-facing helpers for declaring interfaces and providers.
 */

import type { TSchema } from "@sinclair/typebox";
import type { ToolInterface, ToolProvider } from "./types.js";

/**
 * Identity helper that preserves generic inference for a `ToolInterface`.
 *
 * Interfaces are usually declared by pi-relay core. Third parties may declare
 * their own (e.g. a new `vector_search` interface) and ship it alongside a
 * default provider.
 */
export function defineToolInterface<TParams extends TSchema, TResult extends TSchema = TSchema>(
	iface: ToolInterface<TParams, TResult>,
): ToolInterface<TParams, TResult> {
	return iface;
}

/**
 * Identity helper that preserves generic inference for a `ToolProvider`.
 *
 * ```ts
 * export default defineToolProvider<Config, Secrets>({
 *   id: "com.example.search",
 *   implements: "web_search",
 *   displayName: "Example Search",
 *   version: "0.1.0",
 *   defaultConfig: { ... },
 *   secrets: [ ... ],
 *   parameters: Type.Object({ query: Type.String() }),
 *   async execute(params, ctx) { ... },
 * });
 * ```
 */
export function defineToolProvider<
	TConfig = Record<string, never>,
	TSecrets = Record<string, never>,
	TParams extends TSchema = TSchema,
	TDetails = unknown,
>(
	provider: ToolProvider<TConfig, TSecrets, TParams, TDetails>,
): ToolProvider<TConfig, TSecrets, TParams, TDetails> {
	return provider;
}

/**
 * Duck-type guard for a `ToolProvider`. Useful for discovery paths that accept
 * a raw module default export and need to decide whether to route it through
 * `registerToolProvider` vs. treat it as an `ExtensionFactory`.
 */
export function isToolProvider(value: unknown): value is ToolProvider<unknown, unknown> {
	if (value === null || typeof value !== "object") return false;
	const v = value as Record<string, unknown>;
	return (
		typeof v.id === "string" &&
		v.id.length > 0 &&
		typeof v.displayName === "string" &&
		typeof v.version === "string" &&
		typeof v.implements === "string" &&
		v.implements.length > 0 &&
		typeof v.execute === "function"
	);
}
