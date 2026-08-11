// M8-M1 — kernel-mediated MCP round trip: session.create with an mcpSelection
// → bridge writes the session catalog + generated per-server skills → the host
// spawns with PI_RELAY_MCP_CATALOG / PI_RELAY_MCP_CREDENTIALS /
// PRIME_HARNESS_EXTRA_SKILLS_DIRS → the kernel pre-imports mcp_bearer → the
// model calls mock.echo through CatalogMcpIntegration and the echo returns.
// Also asserts selection gating rejects an unselected tool from the kernel.
// Trace: m8-m1-kernel.jsonl
import { existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { BridgeClient, collectText, assert } from "./client.mjs";
import { BRIDGE_DIR, BRIDGE_URL, ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";
import { startMockOAuthMcp } from "./mock-oauth-mcp.mjs";

const mock = await startMockOAuthMcp({ requireOAuth: false, staticBearer: "mock-static-token-abc123" });
const stateRoot = join(BRIDGE_DIR, "data", "workspace-state");

const c = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, join(TRACES_DIR, "m8-m1-kernel.jsonl"));
await c.connect();
const idem = `m8m1-${Date.now()}`;

try {
	// point the bridge at the mock and reload
	writeFileSync(
		join(BRIDGE_DIR, "data", "mcp.toml"),
		`[servers.bearer]
transport = { type = "streamable_http", url = "${mock.url}", auth = { type = "bearer_env", env = "MOCK_MCP_TOKEN" } }
allow_all_tools = true
`,
	);
	const rl = await c.call("mcp.reload", {});
	assert(rl.result?.ok === true, `mcp.reload ok (${JSON.stringify(rl.result ?? rl.error)})`);

	const inv = await c.call("mcp.inventory", {});
	const revision = inv.result.revision;
	assert(typeof revision === "string" && revision.length > 0, "inventory revision for selection pinning");
	const created = await c.call("session.create", {
		name: "m1",
		mcpSelection: { inventory_revision: revision, servers: [{ server: "bearer", tools: ["mock.echo"] }] },
		idempotencyKey: `${idem}-sess`,
	});
	assert(!created.error, `session.create ok (${JSON.stringify(created.error ?? "")})`);
	const sessionId = created.result.sessionId;

	// artifacts on disk before the host even finished booting
	const mcpDir = join(stateRoot, "sessions", sessionId, "mcp");
	assert(existsSync(join(mcpDir, "catalog.json")), "session catalog written");
	const catalog = JSON.parse(readFileSync(join(mcpDir, "catalog.json"), "utf8"));
	assert(catalog.servers.length === 1 && catalog.servers[0].server === "bearer", "catalog pins the bearer server");
	assert(catalog.servers[0].transport.type === "streamable_http", "catalog has transport");
	assert(catalog.servers[0].transport.auth.kind === "bearer_env", "catalog has auth kind");
	assert(catalog.servers[0].transport.auth.env === "MOCK_MCP_TOKEN", "catalog names the bearer env var");
	assert(!JSON.stringify(catalog).includes("mock-static-token-abc123"), "catalog never carries the token itself");
	const skillDir = join(mcpDir, "skills", "mcp-bearer");
	assert(existsSync(join(skillDir, "SKILL.md")), "generated SKILL.md");
	assert(existsSync(join(skillDir, "pyproject.toml")), "generated pyproject.toml");
	assert(existsSync(join(skillDir, "src", "mcp_bearer", "__init__.py")), "generated python module");

	await c.call("session.attach", { id: sessionId, fromSeq: 0 });

	// positive: the model drives the generated skill through the kernel
	const p = await c.call("prompt.send", {
		sessionId,
		text: [
			"In your IPython kernel, run exactly this code (the mcp_bearer module is pre-imported):",
			"",
			"import mcp_bearer",
			"r = await mcp_bearer.call_tool(\"mock.echo\", {\"text\": \"M1-KERNEL-ROUNDTRIP-77\"})",
			"print(\"M1-RESULT\", repr(r))",
			"",
			"Then reply with the M1-RESULT line you saw and a final line exactly: M1-DONE",
		].join("\n"),
		idempotencyKey: `${idem}-prompt`,
	});
	assert(p.result?.accepted === true, "prompt accepted");
	await c.waitIdleAfter(sessionId, c.headSeq(sessionId), 300000);
	let text = collectText(c.events, sessionId);
	assert(text.includes("M1-KERNEL-ROUNDTRIP-77"), "echo result came back through the kernel MCP round trip");
	assert(text.includes("M1-DONE"), "model completed");

	// negative: unselected tool must be rejected by the kernel-side gate
	const p2 = await c.call("prompt.send", {
		sessionId,
		text: [
			"In your IPython kernel, run exactly this code:",
			"",
			"import mcp_bearer",
			"try:",
			"    await mcp_bearer.call_tool(\"mock.time\", {})",
			"    print(\"M1-GATE unexpectedly allowed\")",
			"except Exception as e:",
			"    print(\"M1-GATE\", type(e).__name__, str(e)[:80])",
			"",
			"Then reply with the M1-GATE line you saw and a final line exactly: M1-DONE2",
		].join("\n"),
		idempotencyKey: `${idem}-prompt2`,
	});
	assert(p2.result?.accepted === true, "prompt 2 accepted");
	await c.waitIdleAfter(sessionId, c.headSeq(sessionId), 300000);
	text = collectText(c.events, sessionId);
	assert(text.includes("M1-GATE"), "model reported the gate outcome");
	assert(!text.includes("M1-GATE unexpectedly allowed"), "unselected tool rejected kernel-side");
	assert(text.includes("M1-DONE2"), "model completed 2");

	await c.call("session.delete", { id: sessionId });
	assert(!existsSync(mcpDir), "session.delete removed the mcp artifacts");
	console.log("M1 kernel round trip: PASS");
} finally {
	c.close();
	await mock.close();
}
