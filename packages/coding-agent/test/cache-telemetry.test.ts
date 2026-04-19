import { afterEach, describe, expect, it } from "vitest";
import {
	CACHE_STATS_ENV_VAR,
	formatCacheLogLine,
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

	it("returns true only when env var is exactly '1'", () => {
		process.env[CACHE_STATS_ENV_VAR] = "1";
		expect(isCacheStatsEnabled()).toBe(true);
	});

	it("treats arbitrary truthy strings as disabled (strict opt-in)", () => {
		process.env[CACHE_STATS_ENV_VAR] = "true";
		expect(isCacheStatsEnabled()).toBe(false);
		process.env[CACHE_STATS_ENV_VAR] = "yes";
		expect(isCacheStatsEnabled()).toBe(false);
	});
});
