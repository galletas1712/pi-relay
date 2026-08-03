// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import {
	canSplitFilePane,
	CHAT_COLUMN_MAX_WIDTH,
	clampSidebarWidth,
	DEFAULT_SIDEBAR_WIDTH,
	defaultPanelState,
	FILE_SPLIT_MIN_CENTER_PX,
	MAX_FILE_PANE_WIDTH,
	MAX_SIDEBAR_WIDTH,
	MEDIUM_PANEL_QUERY,
	MIN_SIDEBAR_WIDTH,
	panelModeForViewport,
	saveSidebarWidth,
	SIDEBAR_WIDTH_STORAGE_KEY,
	WIDE_PANEL_QUERY,
	loadSidebarWidth,
} from "./panelLayout.ts";

afterEach(() => {
	vi.restoreAllMocks();
	window.localStorage.clear();
});

describe("panel layout policies", () => {
	it("clamps sidebar widths to rounded minimum and maximum bounds", () => {
		expect(clampSidebarWidth(239.4)).toBe(MIN_SIDEBAR_WIDTH);
		expect(clampSidebarWidth(240.4)).toBe(240);
		expect(clampSidebarWidth(321.5)).toBe(322);
		expect(clampSidebarWidth(999)).toBe(MAX_SIDEBAR_WIDTH);
	});

	it("uses the default for missing or invalid storage and persists clamped values", () => {
		expect(loadSidebarWidth()).toBe(DEFAULT_SIDEBAR_WIDTH);
		window.localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, "not-a-number");
		expect(loadSidebarWidth()).toBe(DEFAULT_SIDEBAR_WIDTH);
		window.localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, "-1");
		expect(loadSidebarWidth()).toBe(DEFAULT_SIDEBAR_WIDTH);

		saveSidebarWidth(999);
		expect(window.localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBe(String(MAX_SIDEBAR_WIDTH));
		expect(loadSidebarWidth()).toBe(MAX_SIDEBAR_WIDTH);
	});

	it("uses compact, medium, and wide panel defaults", () => {
		expect(defaultPanelState("compact")).toEqual({ sidebarOpen: false, rightOpen: false });
		expect(defaultPanelState("medium")).toEqual({ sidebarOpen: false, rightOpen: true });
		expect(defaultPanelState("wide")).toEqual({ sidebarOpen: true, rightOpen: true });
	});

	it("selects viewport mode from controlled media-query matches", () => {
		const matches = new Map<string, boolean>([
			[MEDIUM_PANEL_QUERY, false],
			[WIDE_PANEL_QUERY, false],
		]);
		vi.stubGlobal("matchMedia", (query: string) => ({
			matches: matches.get(query) ?? false,
			media: query,
			onchange: null,
			addEventListener: vi.fn(),
			removeEventListener: vi.fn(),
			addListener: vi.fn(),
			removeListener: vi.fn(),
			dispatchEvent: vi.fn(() => true),
		}));

		expect(panelModeForViewport()).toBe("compact");
		matches.set(MEDIUM_PANEL_QUERY, true);
		expect(panelModeForViewport()).toBe("medium");
		matches.set(WIDE_PANEL_QUERY, true);
		expect(panelModeForViewport()).toBe("wide");
	});

	it("splits only when the center can fit two chat-max columns", () => {
		expect(FILE_SPLIT_MIN_CENTER_PX).toBe(CHAT_COLUMN_MAX_WIDTH * 2);
		expect(MAX_FILE_PANE_WIDTH).toBe(CHAT_COLUMN_MAX_WIDTH);
		expect(canSplitFilePane(FILE_SPLIT_MIN_CENTER_PX - 1)).toBe(false);
		expect(canSplitFilePane(FILE_SPLIT_MIN_CENTER_PX)).toBe(true);
		expect(canSplitFilePane(2400)).toBe(true);
	});
});
