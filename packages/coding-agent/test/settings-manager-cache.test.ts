import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "fs";
import { join } from "path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { SettingsManager } from "../src/core/settings-manager.js";

/**
 * Tests for `SettingsManager.{getCacheRetention,setCacheRetention}` and
 * `SettingsManager.{getShowCacheStats,setShowCacheStats}`.
 *
 * Precedence rules these getters enforce:
 *   - `PI_CACHE_RETENTION` env (when set to `none`/`short`/`long`) wins over
 *     `settings.cache.retention`; otherwise fall through to the persistent
 *     setting; otherwise `undefined` (provider default).
 *   - `PI_SHOW_CACHE_STATS` env (truthy/falsy per `isTruthyEnvFlag`) wins over
 *     `settings.cache.showStats` whenever it's a non-empty string; otherwise
 *     the persistent setting; otherwise `false`.
 */
describe("SettingsManager cache preferences", () => {
	const testDir = join(process.cwd(), "test-settings-cache-tmp");
	const agentDir = join(testDir, "agent");
	const projectDir = join(testDir, "project");

	const envSnapshot: Record<string, string | undefined> = {};

	function stashEnv(key: string) {
		if (!(key in envSnapshot)) {
			envSnapshot[key] = process.env[key];
		}
	}

	function clearEnv(key: string) {
		stashEnv(key);
		delete process.env[key];
	}

	function setEnv(key: string, value: string) {
		stashEnv(key);
		process.env[key] = value;
	}

	function restoreEnv() {
		for (const key of Object.keys(envSnapshot)) {
			const saved = envSnapshot[key];
			if (saved === undefined) {
				delete process.env[key];
			} else {
				process.env[key] = saved;
			}
			delete envSnapshot[key];
		}
	}

	beforeEach(() => {
		if (existsSync(testDir)) {
			rmSync(testDir, { recursive: true });
		}
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(join(projectDir, ".pi"), { recursive: true });
	});

	afterEach(() => {
		if (existsSync(testDir)) {
			rmSync(testDir, { recursive: true });
		}
		restoreEnv();
	});

	describe("getCacheRetention", () => {
		it("returns undefined when no env and no setting", () => {
			clearEnv("PI_CACHE_RETENTION");
			writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ theme: "dark" }));
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getCacheRetention()).toBeUndefined();
		});

		it("returns the setting when env is unset", () => {
			clearEnv("PI_CACHE_RETENTION");
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { retention: "long" } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getCacheRetention()).toBe("long");
		});

		it("env=none wins over setting=long", () => {
			setEnv("PI_CACHE_RETENTION", "none");
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { retention: "long" } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getCacheRetention()).toBe("none");
		});

		it("env=long wins over setting=none", () => {
			setEnv("PI_CACHE_RETENTION", "long");
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { retention: "none" } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getCacheRetention()).toBe("long");
		});

		it("env=short wins over setting=long", () => {
			setEnv("PI_CACHE_RETENTION", "short");
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { retention: "long" } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getCacheRetention()).toBe("short");
		});

		it("ignores unrecognized env values (falls through to setting)", () => {
			setEnv("PI_CACHE_RETENTION", "bogus");
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { retention: "long" } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getCacheRetention()).toBe("long");
		});
	});

	describe("setCacheRetention", () => {
		it("persists the setting to settings.json", async () => {
			writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ theme: "dark" }));
			const manager = SettingsManager.create(projectDir, agentDir);
			manager.setCacheRetention("long");
			await manager.flush();

			const saved = JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf-8"));
			expect(saved.cache).toEqual({ retention: "long" });
			expect(saved.theme).toBe("dark");
		});

		it("clearing with undefined removes the field", async () => {
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { retention: "long", showStats: true } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			manager.setCacheRetention(undefined);
			await manager.flush();

			const saved = JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf-8"));
			expect(saved.cache).toEqual({ showStats: true });
		});
	});

	describe("getShowCacheStats", () => {
		it("returns false when no env and no setting", () => {
			clearEnv("PI_SHOW_CACHE_STATS");
			writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ theme: "dark" }));
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getShowCacheStats()).toBe(false);
		});

		it("returns the setting when env is unset", () => {
			clearEnv("PI_SHOW_CACHE_STATS");
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { showStats: true } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getShowCacheStats()).toBe(true);
		});

		it("env=1 wins over setting=false", () => {
			setEnv("PI_SHOW_CACHE_STATS", "1");
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { showStats: false } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getShowCacheStats()).toBe(true);
		});

		it("env=0 wins over setting=true", () => {
			setEnv("PI_SHOW_CACHE_STATS", "0");
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { showStats: true } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getShowCacheStats()).toBe(false);
		});

		it("env='' (empty string) is treated as unset and falls through", () => {
			setEnv("PI_SHOW_CACHE_STATS", "");
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { showStats: true } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getShowCacheStats()).toBe(true);
		});

		it("env=TRUE (case-insensitive) is enabled", () => {
			setEnv("PI_SHOW_CACHE_STATS", "TRUE");
			writeFileSync(join(agentDir, "settings.json"), JSON.stringify({}));
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getShowCacheStats()).toBe(true);
		});

		it("env=maybe is treated as disabled (env wins, not a fall-through)", () => {
			setEnv("PI_SHOW_CACHE_STATS", "maybe");
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { showStats: true } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.getShowCacheStats()).toBe(false);
		});
	});

	describe("setShowCacheStats", () => {
		it("persists the setting to settings.json", async () => {
			writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ theme: "dark" }));
			const manager = SettingsManager.create(projectDir, agentDir);
			manager.setShowCacheStats(true);
			await manager.flush();

			const saved = JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf-8"));
			expect(saved.cache).toEqual({ showStats: true });
			expect(saved.theme).toBe("dark");
		});

		it("coexists with retention in the same cache object", async () => {
			writeFileSync(
				join(agentDir, "settings.json"),
				JSON.stringify({ cache: { retention: "long" } }),
			);
			const manager = SettingsManager.create(projectDir, agentDir);
			manager.setShowCacheStats(true);
			await manager.flush();

			const saved = JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf-8"));
			expect(saved.cache).toEqual({ retention: "long", showStats: true });
		});
	});
});
