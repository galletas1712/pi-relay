import type { CacheRetention } from "@pi-relay/ai";
import { beforeAll, describe, expect, it } from "vitest";
import {
	type SettingsCallbacks,
	type SettingsConfig,
	SettingsSelectorComponent,
} from "../src/modes/interactive/components/settings-selector.js";
import { initTheme } from "../src/modes/interactive/theme/theme.js";

/**
 * Integration tests for the cache preferences exposed through the `/settings`
 * TUI picker. Verifies the selector renders the current values from
 * `SettingsConfig` and that cycling through values dispatches the right
 * callbacks.
 */
describe("SettingsSelectorComponent cache preferences", () => {
	beforeAll(() => {
		initTheme(undefined, false);
	});

	type PartialCallbacks = Partial<SettingsCallbacks>;

	function makeConfig(overrides: Partial<SettingsConfig> = {}): SettingsConfig {
		return {
			autoCompact: false,
			showImages: false,
			autoResizeImages: false,
			blockImages: false,
			enableSkillCommands: false,
			steeringMode: "one-at-a-time",
			followUpMode: "one-at-a-time",
			transport: "sse",
			thinkingLevel: "off",
			availableThinkingLevels: ["off"],
			currentTheme: "dark",
			availableThemes: ["dark", "light"],
			hideThinkingBlock: false,
			collapseChangelog: false,
			enableInstallTelemetry: false,
			doubleEscapeAction: "tree",
			treeFilterMode: "default",
			showHardwareCursor: false,
			editorPaddingX: 0,
			autocompleteMaxVisible: 5,
			quietStartup: false,
			clearOnShrink: false,
			cacheRetention: undefined,
			showCacheStats: false,
			...overrides,
		};
	}

	function makeCallbacks(overrides: PartialCallbacks = {}): SettingsCallbacks {
		const noop = () => {};
		return {
			onAutoCompactChange: noop,
			onShowImagesChange: noop,
			onAutoResizeImagesChange: noop,
			onBlockImagesChange: noop,
			onEnableSkillCommandsChange: noop,
			onSteeringModeChange: noop,
			onFollowUpModeChange: noop,
			onTransportChange: noop,
			onThinkingLevelChange: noop,
			onThemeChange: noop,
			onHideThinkingBlockChange: noop,
			onCollapseChangelogChange: noop,
			onEnableInstallTelemetryChange: noop,
			onDoubleEscapeActionChange: noop,
			onTreeFilterModeChange: noop,
			onShowHardwareCursorChange: noop,
			onEditorPaddingXChange: noop,
			onAutocompleteMaxVisibleChange: noop,
			onQuietStartupChange: noop,
			onClearOnShrinkChange: noop,
			onCacheRetentionChange: noop,
			onShowCacheStatsChange: noop,
			onCancel: noop,
			...overrides,
		};
	}

	// Strip ANSI escape sequences so text assertions can target raw content
	// without having to model the theme's color wrapping.
	// biome-ignore lint/suspicious/noControlCharactersInRegex: stripping ANSI.
	const ANSI_RE = /\u001b\[[0-9;]*m/g;
	function stripAnsi(s: string): string {
		return s.replace(ANSI_RE, "");
	}

	/**
	 * Render every page of items in the settings list and concatenate the
	 * stripped output. The SettingsList renders a scrolling window so we iterate
	 * the selection downward until we've seen all distinct pages.
	 */
	function renderAllItems(selector: SettingsSelectorComponent): string {
		const list = selector.getSettingsList();
		const collected: string[] = [];
		const seen = new Set<string>();
		for (let i = 0; i < 40; i++) {
			const rendered = stripAnsi(list.render(200).join("\n"));
			if (seen.has(rendered)) break;
			seen.add(rendered);
			collected.push(rendered);
			list.handleInput("\x1b[B");
		}
		return collected.join("\n---\n");
	}

	/**
	 * Drive the list to select the row whose label matches, then press space to
	 * cycle its value. Returns true when the row was found and space was sent.
	 */
	function cycleByLabel(selector: SettingsSelectorComponent, label: string): boolean {
		const list = selector.getSettingsList();
		for (let i = 0; i < 40; i++) {
			const rendered = stripAnsi(list.render(200).join("\n"));
			const selectedLineRe = new RegExp(`^\\s*[\u2192>]\\s+${label}\\b`, "m");
			if (selectedLineRe.test(rendered)) {
				list.handleInput(" ");
				return true;
			}
			list.handleInput("\x1b[B");
		}
		return false;
	}

	it("renders cache-retention with the current setting value", () => {
		const selector = new SettingsSelectorComponent(
			makeConfig({ cacheRetention: "long" }),
			makeCallbacks(),
		);
		const rendered = renderAllItems(selector);
		expect(rendered).toContain("Cache retention");
		expect(rendered).toMatch(/Cache retention\s+long/);
	});

	it("renders cache-retention defaulting to 'short' when setting is undefined", () => {
		const selector = new SettingsSelectorComponent(
			makeConfig({ cacheRetention: undefined }),
			makeCallbacks(),
		);
		const rendered = renderAllItems(selector);
		expect(rendered).toMatch(/Cache retention\s+short/);
	});

	it("renders show-cache-stats with the current setting value", () => {
		const selector = new SettingsSelectorComponent(
			makeConfig({ showCacheStats: true }),
			makeCallbacks(),
		);
		const rendered = renderAllItems(selector);
		expect(rendered).toContain("Show cache stats");
		expect(rendered).toMatch(/Show cache stats\s+true/);
	});

	it("dispatches onCacheRetentionChange when the value cycles from 'short' to 'long'", () => {
		const received: CacheRetention[] = [];
		const selector = new SettingsSelectorComponent(
			makeConfig({ cacheRetention: "short" }),
			makeCallbacks({ onCacheRetentionChange: (r) => received.push(r) }),
		);
		expect(cycleByLabel(selector, "Cache retention")).toBe(true);
		expect(received).toEqual(["long"]);
	});

	it("dispatches onShowCacheStatsChange when the boolean toggles", () => {
		const received: boolean[] = [];
		const selector = new SettingsSelectorComponent(
			makeConfig({ showCacheStats: false }),
			makeCallbacks({ onShowCacheStatsChange: (v) => received.push(v) }),
		);
		expect(cycleByLabel(selector, "Show cache stats")).toBe(true);
		expect(received).toEqual([true]);
	});
});
