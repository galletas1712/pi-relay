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
import { isTruthyEnvFlag } from "../utils/env-flag.js";

/** Env flag that opts a developer into per-turn cache telemetry. */
export const CACHE_STATS_ENV_VAR = "PI_SHOW_CACHE_STATS";

/**
 * Returns true when the developer has opted into cache telemetry.
 *
 * Precedence mirrors `SettingsManager.getShowCacheStats`:
 *   - Env var `PI_SHOW_CACHE_STATS` wins when set to a non-empty value. Uses
 *     the shared `isTruthyEnvFlag` convention (`1`, `true`, `yes`
 *     case-insensitively as "enabled"; anything else disabled).
 *   - Otherwise falls through to the caller-supplied `settingsShowStats`.
 *   - Otherwise `false`.
 *
 * The optional `settingsShowStats` argument lets footer/print-mode surface the
 * persistent setting without duplicating the precedence rules at every call
 * site. Callers that don't have access to settings (or that want env-only
 * gating) can omit it and get the pre-settings behavior.
 */
export function isCacheStatsEnabled(settingsShowStats?: boolean): boolean {
	if (typeof process === "undefined") return settingsShowStats ?? false;
	const envValue = process.env[CACHE_STATS_ENV_VAR];
	if (envValue !== undefined && envValue !== "") {
		return isTruthyEnvFlag(envValue);
	}
	return settingsShowStats ?? false;
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
 * Scope tag for structured cache-telemetry lines. Values:
 *
 * - `self` — attached agent only (multi-agent per-turn aggregate).
 * - `tree` — attached agent plus all live descendants (multi-agent per-turn).
 * - `worklog` | `compaction` | `branch` | `turn-prefix` — per-background-call
 *   attribution for out-of-band LLM calls whose usage would otherwise not
 *   appear in the per-turn aggregate. Emitted at the moment the background
 *   call completes; useful for attributing cost to the specific out-of-band
 *   path that produced it.
 *
 * The union mirrors {@link BackgroundUsageScope} (from `agent-session.ts`)
 * plus the two per-turn scopes. `cache-telemetry.ts` cannot import from
 * `agent-session.ts` (imports would become cyclic) so the union is duplicated
 * here as the string-literal authority for formatting.
 */
export type CacheLogScope = "self" | "tree" | "worklog" | "compaction" | "branch" | "turn-prefix";

/**
 * Formats a structured cache-telemetry line for stderr.
 *
 * Default shape (single-agent): `[pi:cache] turn=<N> cacheRead=<R> cacheWrite=<W> input=<I> output=<O>`
 *
 * With `scope`: `[pi:cache] turn=<N> <scope> cacheRead=<R> ...` where
 * `<scope>` is one of the values in {@link CacheLogScope}. The `scope` token
 * sits immediately after `turn=<N>` so the tail format stays grep-compatible
 * with single-agent logs.
 *
 * For background-call lines (`worklog`/`compaction`/`branch`/`turn-prefix`)
 * the `turn` value is the session's current turn count at emission time.
 * Background calls do not have their own turn number; reusing the session
 * turn lets devs correlate a background cost to the turn that triggered it
 * (e.g., a worklog fork fires after the main turn completes).
 *
 * Stable enough to grep; no color codes; single line per emission. Consumed
 * by `print-mode` when `PI_SHOW_CACHE_STATS=1`.
 */
export function formatCacheLogLine(opts: {
	turn: number;
	usage: Pick<Usage, "input" | "output" | "cacheRead" | "cacheWrite">;
	scope?: CacheLogScope;
}): string {
	const { turn, usage, scope } = opts;
	const scopePart = scope ? ` ${scope}` : "";
	return `[pi:cache] turn=${turn}${scopePart} cacheRead=${usage.cacheRead} cacheWrite=${usage.cacheWrite} input=${usage.input} output=${usage.output}`;
}

/**
 * Compact token-count formatter used by the TUI footer for cumulative cols
 * (matching the pre-PR format: `1234`, `4.3k`, `12k`, `1.2M`, `12M`).
 *
 * Exposed here so the subtree-aware footer renderer and any external tooling
 * can produce matching strings without duplicating the abbreviation rules.
 */
export function formatCompactTokens(count: number): string {
	if (count < 1000) return count.toString();
	if (count < 10000) return `${(count / 1000).toFixed(1)}k`;
	if (count < 1000000) return `${Math.round(count / 1000)}k`;
	if (count < 10000000) return `${(count / 1000000).toFixed(1)}M`;
	return `${Math.round(count / 1000000)}M`;
}

/**
 * Formats the stats portion of the footer for an agent that has descendants.
 *
 * Returns an array of tokens meant to be joined with spaces by the caller,
 * matching the existing footer token layout (`↑N ↓N RN WN self$X · tree$Y`).
 * When `tree == self` (no descendants or identical totals), returns only the
 * self-scoped tokens so single-agent output stays untouched.
 *
 * Cost strings use 3-decimal precision to match the existing cumulative-cost
 * format at the footer's `$X.XXX` rendering.
 */
export function formatSelfTreeStatsTokens(opts: {
	self: { input: number; output: number; cacheRead: number; cacheWrite: number; cost: number };
	tree: { input: number; output: number; cacheRead: number; cacheWrite: number; cost: number };
	/** When true, always include the `tree` column even if tree == self. Defaults to false. */
	alwaysShowTree?: boolean;
}): string[] {
	const { self, tree, alwaysShowTree = false } = opts;
	const parts: string[] = [];
	if (self.input) parts.push(`↑${formatCompactTokens(self.input)}`);
	if (self.output) parts.push(`↓${formatCompactTokens(self.output)}`);
	if (self.cacheRead) parts.push(`R${formatCompactTokens(self.cacheRead)}`);
	if (self.cacheWrite) parts.push(`W${formatCompactTokens(self.cacheWrite)}`);

	const showTree =
		alwaysShowTree ||
		tree.cost > self.cost ||
		tree.input > self.input ||
		tree.output > self.output ||
		tree.cacheRead > self.cacheRead ||
		tree.cacheWrite > self.cacheWrite;

	if (!showTree) {
		return parts;
	}

	// Append tree token counts inline when they differ from self.
	const treeTokenParts: string[] = [];
	if (tree.input !== self.input) treeTokenParts.push(`↑${formatCompactTokens(tree.input)}`);
	if (tree.output !== self.output) treeTokenParts.push(`↓${formatCompactTokens(tree.output)}`);
	if (tree.cacheRead !== self.cacheRead) treeTokenParts.push(`R${formatCompactTokens(tree.cacheRead)}`);
	if (tree.cacheWrite !== self.cacheWrite) treeTokenParts.push(`W${formatCompactTokens(tree.cacheWrite)}`);
	if (treeTokenParts.length > 0) {
		parts.push(`tree[${treeTokenParts.join(" ")}]`);
	}

	return parts;
}

/**
 * Formats the cost portion of the footer as either a single `$X.XXX` value
 * (no descendants) or `self $X.XXX · tree $Y.YYY` (descendants present).
 *
 * Returns `undefined` when neither self nor tree has any cost AND `suffix`
 * isn't provided. Otherwise always returns a string so the caller can append
 * subscription suffixes. Precision is fixed at 3 decimal places to match the
 * pre-PR single-cost format.
 */
export function formatSelfTreeCost(opts: {
	selfCost: number;
	treeCost: number;
	/** Suffix appended to the cost value, e.g. ` (sub)`. Applied to the self-cost only. */
	suffix?: string;
	/** When true, always render `self $X · tree $Y`. Defaults to false. */
	alwaysShowTree?: boolean;
}): string | undefined {
	const { selfCost, treeCost, suffix = "", alwaysShowTree = false } = opts;
	const showTree = alwaysShowTree || treeCost > selfCost;
	if (!showTree) {
		if (!selfCost && !suffix) return undefined;
		return `$${selfCost.toFixed(3)}${suffix}`;
	}
	return `self $${selfCost.toFixed(3)}${suffix} · tree $${treeCost.toFixed(3)}`;
}
