import { describe, expect, it } from "vitest";
import {
	FOREGROUND_REFRESH_AFTER_MS,
	shouldRefreshOnForeground,
} from "./refresh.mjs";

describe("shouldRefreshOnForeground", () => {
	const now = 1_000_000;

	it("does not refresh without a background timestamp", () => {
		expect(shouldRefreshOnForeground(null, now)).toBe(false);
	});

	it("refreshes at the threshold and after it", () => {
		expect(
			shouldRefreshOnForeground(now - FOREGROUND_REFRESH_AFTER_MS, now),
		).toBe(true);
		expect(
			shouldRefreshOnForeground(now - FOREGROUND_REFRESH_AFTER_MS - 1, now),
		).toBe(true);
	});

	it("does not refresh before the threshold", () => {
		expect(
			shouldRefreshOnForeground(now - FOREGROUND_REFRESH_AFTER_MS + 1, now),
		).toBe(false);
	});

	it("uses the supplied clock values without reading the current time", () => {
		expect(shouldRefreshOnForeground(500, 499)).toBe(false);
		expect(shouldRefreshOnForeground(500, 500)).toBe(false);
	});
});
