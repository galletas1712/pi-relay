import { afterEach, describe, expect, it } from "vitest";
import {
	CACHE_STATS_ENV_VAR,
	formatCacheLogLine,
	formatCompactTokens,
	formatSelfTreeCost,
	formatTurnCacheStats,
	isCacheStatsEnabled,
} from "../src/core/cache-telemetry.js";

describe("formatTurnCacheStats", () => {
	it("returns undefined when both cacheRead and cacheWrite are zero", () => {
		expect(formatTurnCacheStats({ cacheRead: 0, cacheWrite: 0 })).toBeUndefined();
	});

	it("renders raw token counts when cacheRead is non-zero", () => {
		expect(formatTurnCacheStats({ cacheRead: 4321, cacheWrite: 0 })).toBe("Δ R:4321 W:0");
	});

	it("renders raw token counts when only cacheWrite is non-zero", () => {
		expect(formatTurnCacheStats({ cacheRead: 0, cacheWrite: 89 })).toBe("Δ R:0 W:89");
	});

	it("renders both when both are non-zero", () => {
		expect(formatTurnCacheStats({ cacheRead: 4321, cacheWrite: 89 })).toBe("Δ R:4321 W:89");
	});

	it("does not abbreviate large counts (per-turn precision matters)", () => {
		expect(formatTurnCacheStats({ cacheRead: 12345, cacheWrite: 6789 })).toBe("Δ R:12345 W:6789");
	});
});

describe("formatCacheLogLine", () => {
	it("produces a stable single-line format", () => {
		expect(
			formatCacheLogLine({
				turn: 42,
				usage: { input: 1234, output: 567, cacheRead: 4321, cacheWrite: 89 },
			}),
		).toBe("[pi:cache] turn=42 cacheRead=4321 cacheWrite=89 input=1234 output=567");
	});

	it("emits zeros explicitly (so diff-on-zero can be measured)", () => {
		expect(
			formatCacheLogLine({
				turn: 1,
				usage: { input: 100, output: 50, cacheRead: 0, cacheWrite: 0 },
			}),
		).toBe("[pi:cache] turn=1 cacheRead=0 cacheWrite=0 input=100 output=50");
	});

	it("includes scope token after turn= when scope is provided", () => {
		expect(
			formatCacheLogLine({
				turn: 3,
				scope: "self",
				usage: { input: 100, output: 50, cacheRead: 0, cacheWrite: 0 },
			}),
		).toBe("[pi:cache] turn=3 self cacheRead=0 cacheWrite=0 input=100 output=50");

		expect(
			formatCacheLogLine({
				turn: 3,
				scope: "tree",
				usage: { input: 500, output: 200, cacheRead: 100, cacheWrite: 50 },
			}),
		).toBe("[pi:cache] turn=3 tree cacheRead=100 cacheWrite=50 input=500 output=200");
	});

	it("formats background-scope lines (worklog / compaction / branch / turn-prefix)", () => {
		expect(
			formatCacheLogLine({
				turn: 5,
				scope: "worklog",
				usage: { input: 200, output: 80, cacheRead: 300, cacheWrite: 20 },
			}),
		).toBe("[pi:cache] turn=5 worklog cacheRead=300 cacheWrite=20 input=200 output=80");

		expect(
			formatCacheLogLine({
				turn: 7,
				scope: "compaction",
				usage: { input: 4000, output: 300, cacheRead: 0, cacheWrite: 4000 },
			}),
		).toBe("[pi:cache] turn=7 compaction cacheRead=0 cacheWrite=4000 input=4000 output=300");

		expect(
			formatCacheLogLine({
				turn: 7,
				scope: "turn-prefix",
				usage: { input: 1500, output: 150, cacheRead: 0, cacheWrite: 1500 },
			}),
		).toBe("[pi:cache] turn=7 turn-prefix cacheRead=0 cacheWrite=1500 input=1500 output=150");

		expect(
			formatCacheLogLine({
				turn: 12,
				scope: "branch",
				usage: { input: 900, output: 120, cacheRead: 50, cacheWrite: 900 },
			}),
		).toBe("[pi:cache] turn=12 branch cacheRead=50 cacheWrite=900 input=900 output=120");
	});

	it("places scope token immediately after turn= for background scopes (grep invariant)", () => {
		// The tail format (cacheRead= ... output=...) must match the single-agent
		// default format so grep-based aggregation scripts continue to work.
		const defaultLine = formatCacheLogLine({
			turn: 1,
			usage: { input: 10, output: 20, cacheRead: 30, cacheWrite: 40 },
		});
		const worklogLine = formatCacheLogLine({
			turn: 1,
			scope: "worklog",
			usage: { input: 10, output: 20, cacheRead: 30, cacheWrite: 40 },
		});
		const defaultTail = defaultLine.replace(/^\[pi:cache\] turn=\d+/, "");
		const worklogTail = worklogLine.replace(/^\[pi:cache\] turn=\d+ worklog/, "");
		expect(worklogTail).toBe(defaultTail);
	});
});

describe("formatCompactTokens", () => {
	it("shows raw numbers under 1000", () => {
		expect(formatCompactTokens(0)).toBe("0");
		expect(formatCompactTokens(999)).toBe("999");
	});

	it("uses 1 decimal kilo abbreviation from 1k to 10k", () => {
		expect(formatCompactTokens(1234)).toBe("1.2k");
		expect(formatCompactTokens(4321)).toBe("4.3k");
	});

	it("uses rounded kilo from 10k to 1M", () => {
		expect(formatCompactTokens(12_345)).toBe("12k");
		expect(formatCompactTokens(999_499)).toBe("999k");
	});

	it("uses 1 decimal mega from 1M to 10M", () => {
		expect(formatCompactTokens(1_200_000)).toBe("1.2M");
	});

	it("uses rounded mega above 10M", () => {
		expect(formatCompactTokens(12_345_678)).toBe("12M");
	});
});

describe("formatSelfTreeCost", () => {
	it("returns undefined for zero cost, zero tree, no suffix", () => {
		expect(formatSelfTreeCost({ selfCost: 0, treeCost: 0 })).toBeUndefined();
	});

	it("renders single cost when tree == self", () => {
		expect(formatSelfTreeCost({ selfCost: 0.023, treeCost: 0.023 })).toBe("$0.023");
	});

	it("appends subscription suffix when tree == self", () => {
		expect(formatSelfTreeCost({ selfCost: 0, treeCost: 0, suffix: " (sub)" })).toBe("$0.000 (sub)");
	});

	it("expands to self/tree when tree > self", () => {
		expect(formatSelfTreeCost({ selfCost: 0.02, treeCost: 0.14 })).toBe("self $0.020 · tree $0.140");
	});

	it("applies suffix only to the self cost when expanded", () => {
		expect(formatSelfTreeCost({ selfCost: 0.02, treeCost: 0.14, suffix: " (sub)" })).toBe(
			"self $0.020 (sub) · tree $0.140",
		);
	});

	it("honors alwaysShowTree even when tree == self", () => {
		expect(formatSelfTreeCost({ selfCost: 0.02, treeCost: 0.02, alwaysShowTree: true })).toBe(
			"self $0.020 · tree $0.020",
		);
	});
});

describe("isCacheStatsEnabled", () => {
	const original = process.env[CACHE_STATS_ENV_VAR];

	afterEach(() => {
		if (original === undefined) {
			delete process.env[CACHE_STATS_ENV_VAR];
		} else {
			process.env[CACHE_STATS_ENV_VAR] = original;
		}
	});

	it("returns false when the env var is unset", () => {
		delete process.env[CACHE_STATS_ENV_VAR];
		expect(isCacheStatsEnabled()).toBe(false);
	});

	it("returns true when env var is '1'", () => {
		process.env[CACHE_STATS_ENV_VAR] = "1";
		expect(isCacheStatsEnabled()).toBe(true);
	});

	it("accepts conventional truthy strings (case-insensitive)", () => {
		process.env[CACHE_STATS_ENV_VAR] = "true";
		expect(isCacheStatsEnabled()).toBe(true);
		process.env[CACHE_STATS_ENV_VAR] = "yes";
		expect(isCacheStatsEnabled()).toBe(true);
		process.env[CACHE_STATS_ENV_VAR] = "TRUE";
		expect(isCacheStatsEnabled()).toBe(true);
	});

	it("treats non-conventional strings as disabled", () => {
		process.env[CACHE_STATS_ENV_VAR] = "0";
		expect(isCacheStatsEnabled()).toBe(false);
		process.env[CACHE_STATS_ENV_VAR] = "maybe";
		expect(isCacheStatsEnabled()).toBe(false);
		process.env[CACHE_STATS_ENV_VAR] = "";
		expect(isCacheStatsEnabled()).toBe(false);
	});
});
