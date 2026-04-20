/**
 * Optional render helpers for tool providers.
 *
 * Re-exports TUI primitives used by custom `renderCall` / `renderResult`
 * implementations. This subpath is the only place `@pi-relay/tool-kit` touches
 * `@pi-relay/tui`; the main entry remains TUI-free so providers without custom
 * rendering can depend only on typebox + tool-kit.
 *
 * For the `Theme` type (currently owned by `@pi-relay/coding-agent`), import
 * it directly from `@pi-relay/coding-agent` in your provider module. Pulling
 * it in here would create a dependency cycle (coding-agent consumes tool-kit
 * to adapt `ToolProvider` -> `ToolDefinition`).
 */

export { type Component, Text } from "@pi-relay/tui";

/**
 * Options passed to `renderResult` by the host. Mirrors
 * `ToolRenderResultOptions` from `@pi-relay/coding-agent` so authors don't
 * need to import coding-agent internals for the common case.
 */
export interface ToolRenderResultOptions {
	expanded: boolean;
	isPartial: boolean;
	showImages: boolean;
	isError: boolean;
}

/**
 * Minimal render context surface. Providers that need the full
 * `ToolRenderContext<TState, TParams>` should import it from
 * `@pi-relay/coding-agent` directly; this is a convenience for the common
 * case of renderers that only read `expanded` / `isPartial` state.
 */
export interface ToolRenderContext<TState = unknown, TParams = unknown> {
	readonly state: TState;
	readonly params: TParams;
	readonly expanded: boolean;
	readonly isPartial: boolean;
}
