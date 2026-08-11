// M10b: cacheRetention plumbing unit tests — BRIDGE_PI_CACHE_RETENTION
// parsing (config.ts) + per-session threading through sessionEnv
// (supervisor.ts). Self-contained: no bridge, no PG (dummy env is set before
// imports; the pg Pool is lazy and never queried here).
//   node --test test/m10-cache-config.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

process.env.BRIDGE_AUTH_TOKEN ??= "m10-test-token";
process.env.BRIDGE_PG_URL ??= "postgres://m10:m10@127.0.0.1:1/m10";
delete process.env.BRIDGE_PI_CACHE_RETENTION;

const here = path.dirname(fileURLToPath(import.meta.url));
const bridgeDir = path.resolve(here, "..");

const { config, parseCacheRetention } = await import("../src/config.ts");
const { sessionEnv } = await import("../src/supervisor.ts");

const fakeHandle = { id: "s-m10", workspaces: [], mcpSelection: null };

test("parseCacheRetention accepts literals, rejects garbage", () => {
	assert.equal(parseCacheRetention(""), null);
	assert.equal(parseCacheRetention("none"), "none");
	assert.equal(parseCacheRetention("short"), "short");
	assert.equal(parseCacheRetention("long"), "long");
	assert.throws(() => parseCacheRetention("24h"), /invalid BRIDGE_PI_CACHE_RETENTION/);
	assert.throws(() => parseCacheRetention("LONG"), /invalid BRIDGE_PI_CACHE_RETENTION/);
});

test("default (unset) leaves hosts on upstream default: no PI_CACHE_RETENTION in sessionEnv", () => {
	assert.equal(config.piCacheRetention, null);
	const env = sessionEnv(fakeHandle);
	assert.ok(!("PI_CACHE_RETENTION" in env));
});

test("configured value threads through sessionEnv (child process, env set pre-import)", () => {
	const src = [
		'process.env.BRIDGE_AUTH_TOKEN = "m10-test-token";',
		'process.env.BRIDGE_PG_URL = "postgres://m10:m10@127.0.0.1:1/m10";',
		'process.env.BRIDGE_PI_CACHE_RETENTION = "long";',
		'const { config } = await import("./src/config.ts");',
		'const { sessionEnv } = await import("./src/supervisor.ts");',
		'const env = sessionEnv({ id: "s-m10", workspaces: [], mcpSelection: null });',
		'console.log(JSON.stringify({ cfg: config.piCacheRetention, env }));',
	].join("\n");
	const out = execFileSync(process.execPath, ["--input-type=module", "-e", src], {
		cwd: bridgeDir,
		encoding: "utf8",
	});
	const got = JSON.parse(out.trim());
	assert.equal(got.cfg, "long");
	assert.equal(got.env.PI_CACHE_RETENTION, "long");
});

test("invalid value fails boot loudly", () => {
	const src = [
		'process.env.BRIDGE_AUTH_TOKEN = "x";',
		'process.env.BRIDGE_PG_URL = "postgres://x:x@127.0.0.1:1/x";',
		'process.env.BRIDGE_PI_CACHE_RETENTION = "24h";',
		'await import("./src/config.ts");',
	].join("\n");
	try {
		execFileSync(process.execPath, ["--input-type=module", "-e", src], { cwd: bridgeDir, stdio: "pipe" });
		assert.fail("expected boot to fail");
	} catch (err) {
		assert.match(String(err.stderr), /invalid BRIDGE_PI_CACHE_RETENTION/);
	}
});

test("sessionEnv keeps existing M8 behavior (workspaces + mcp selection)", () => {
	const env = sessionEnv({
		id: "s-m10b",
		workspaces: [{ workspaceDir: "a" }, { workspaceDir: "b" }],
		mcpSelection: { servers: ["docs"] },
	});
	assert.equal(env.PI_RELAY_WORKSPACE_DIRS, "a,b");
	assert.ok(env.PI_RELAY_MCP_CATALOG.endsWith("catalog.json"));
	assert.ok(!("PI_CACHE_RETENTION" in env)); // unset in this process
});
