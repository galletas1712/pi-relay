import { test } from "node:test";
import assert from "node:assert/strict";
import { loadMcpConfig, parseMcpConfig, configFingerprint, serverFingerprint, toolEnabled } from "../src/mcp/config.ts";

const FIXTURE = `
[servers.docs]
transport = { type = "streamable_http", url = "https://mcp.example.com/mcp" }
allow_all_tools = true

[servers.local]
transport = { type = "stdio", command = "node", args = ["server.js", "--fast"], inherit_env = ["HOME", "PATH"] }
enabled_tools = ["search"]
startup_timeout_ms = 5000

[servers.legacy]
command = "/usr/bin/true"
allow_all_tools = true

[servers.tok]
allow_all_tools = true
[servers.tok.transport]
type = "streamable_http"
url = "http://127.0.0.1:9999/mcp"
[servers.tok.transport.auth]
type = "bearer_env"
env = "MOCK_MCP_TOKEN"

[servers.oa]
allow_all_tools = true
[servers.oa.transport]
type = "streamable_http"
url = "http://localhost:8580/mcp"
[servers.oa.transport.auth]
type = "oauth"
client_id = "cid"
scopes = ["mcp:read"]
`;

test("parse + validate fixture", () => {
	const cfg = parseMcpConfig(FIXTURE);
	assert.equal(cfg.servers.size, 5);
	const docs = cfg.servers.get("docs");
	assert.equal(docs.transport.type, "streamable_http");
	assert.equal(docs.allowAllTools, true);
	const local = cfg.servers.get("local");
	assert.equal(local.transport.type, "stdio");
	assert.deepEqual(local.transport.inheritEnv.sort(), ["HOME", "PATH"]);
	assert.equal(local.startupTimeoutMs, 5000);
	assert.equal(toolEnabled(local, "search"), true);
	assert.equal(toolEnabled(local, "nope"), false);
	assert.equal(cfg.servers.get("legacy").transport.type, "stdio");
	assert.equal(cfg.servers.get("tok").transport.auth.type, "bearer_env");
	assert.equal(cfg.servers.get("oa").transport.auth.type, "oauth");
});

test("fingerprints are deterministic + secret env hashed", () => {
	const a = configFingerprint(parseMcpConfig(FIXTURE));
	const b = configFingerprint(parseMcpConfig(FIXTURE));
	assert.equal(a, b);
	assert.match(a, /^[0-9a-f]{64}$/);
	const secretCfg = parseMcpConfig(`
[servers.x]
command = "node"
env = { PLAIN_VAR = "v1" }
allow_all_tools = true
`);
	assert.equal(serverFingerprint(secretCfg.servers.get("x")).includes("v1"), false);
});

test("validation rejects: https required (non-loopback), secret literal env, missing enabled_tools, url creds", () => {
	assert.throws(() => parseMcpConfig('[servers.x]\ntransport={type="streamable_http",url="http://example.com/mcp"}\nallow_all_tools=true'), /HTTPS/);
	assert.throws(() => parseMcpConfig('[servers.x]\ncommand="node"\nenv={MY_TOKEN="x"}\nallow_all_tools=true'), /secret-like/);
	assert.throws(() => parseMcpConfig('[servers.x]\ncommand="node"'), /enabled_tools/);
	assert.throws(() => parseMcpConfig('[servers.x]\ntransport={type="streamable_http",url="https://u:p@example.com/mcp"}\nallow_all_tools=true'), /credentials/);
	assert.throws(() => parseMcpConfig('[servers.x]\ntransport={type="streamable_http",url="https://example.com/mcp#f"}\nallow_all_tools=true'), /fragment/);
});

test("live pi-relay mcp.toml (read-only reference) parses under the ported semantics", () => {
	const live = process.env.HOME + "/.config/pi-relay/runtime/mcp.toml";
	try {
		const cfg = loadMcpConfig(live);
		assert.ok(cfg.servers.size > 0, "live config has servers");
	} catch (err) {
		if (String(err).includes("open MCP config")) return; // file absent on this machine: skip
		throw err;
	}
});
