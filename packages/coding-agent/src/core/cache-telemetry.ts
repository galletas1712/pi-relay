/**
 * Dev-visible cache telemetry.
 *
 * Anthropic's `cache_read_input_tokens` / `cache_creation_input_tokens` fields
 * flow through `Usage.cacheRead` / `Usage.cacheWrite` (see
 * `packages/ai/src/providers/anthropic.ts`). The TUI footer has long shown
 * the *cumulative* totals for a session. When iterating on caching changes
 * (prompt assembly, breakpoint placement, retention tiers), developers need
 * *per-turn* numbers to attribute hits/writes to the turn that produced them.
 *
 * This module provides:
 *   - `isCacheStatsEnabled()` — gated on `PI_SHOW_CACHE_STATS=1` so we
 *     don't add dev telemetry to the default UX.
 *   - `formatTurnCacheStats(usage)` — compact string for the TUI footer.
 *   - `formatCacheLogLine(opts)` — stable structured line for print-mode
 *     stderr output, matching the format documented in the README caching
 *     section so it can be grepped or piped into measurement scripts.
 */

import type { Usage } from "@pi-relay/ai";

/** Env flag that opts a developer into per-turn cache telemetry. */
export const CACHE_STATS_ENV_VAR = "PI_SHOW_CACHE_STATS";

/**
 * Returns true when the developer has opted into cache telemetry.
 * Checked via env var (not settings) so it can be toggled per-invocation.
 */
export function isCacheStatsEnabled(): boolean {
	return typeof process !== "undefined" && process.env[CACHE_STATS_ENV_VAR] === "1";
}

/**
 * Formats per-turn cache R/W numbers for the TUI footer.
 *
 * Returns `undefined` when the turn had no cache activity at all — the caller
 * suppresses the column entirely rather than showing `Δ R:0 W:0`.
 *
 * Uses raw token counts (no k/M abbreviation) because per-turn numbers are
 * typically small (<10k) and abbreviation would lose precision.
 */
export function formatTurnCacheStats(usage: Pick<Usage, "cacheRead" | "cacheWrite">): string | undefined {
	if (!usage.cacheRead && !usage.cacheWrite) return undefined;
	return `Δ R:${usage.cacheRead} W:${usage.cacheWrite}`;
}

/**
 * Formats a structured cache-telemetry line for stderr.
 *
 * Shape: `[pi:cache] turn=<N> cacheRead=<R> cacheWrite=<W> input=<I> output=<O>`
 *
 * Stable enough to grep; no color codes; single line per turn. Consumed by
 * `print-mode` when `PI_SHOW_CACHE_STATS=1`.
 */
export function formatCacheLogLine(opts: {
	turn: number;
	usage: Pick<Usage, "input" | "output" | "cacheRead" | "cacheWrite">;
}): string {
	const { turn, usage } = opts;
	return `[pi:cache] turn=${turn} cacheRead=${usage.cacheRead} cacheWrite=${usage.cacheWrite} input=${usage.input} output=${usage.output}`;
}
