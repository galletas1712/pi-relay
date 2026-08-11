// M3 (real mcp.inventory) + M2 (OAuth state machine vs mock) + control-plane
// mcp.call — all through the LIVE bridge on 127.0.0.1:8730.
// Trace: .pi/m1-demo/traces/m8-mcp-inventory.jsonl + m8-mcp-oauth.jsonl
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { BridgeClient } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken, BRIDGE_DIR } from "./harness.mjs";
import { startMockOAuthMcp } from "./mock-oauth-mcp.mjs";

let bearerMock, oauthMock, fastMock, client;

/** request/response with error rejection (client.call resolves with the raw frame). */
async function req(method, params = {}) {
	const resp = await client.call(method, params);
	if (resp.error) {
		const err = new Error(resp.error.message);
		err.code = resp.error.code;
		throw err;
	}
	return resp.result;
}

function waitForEvent(name, timeoutMs = 10000) {
	return client.waitEvent((ev) => ev.event === name, timeoutMs, name);
}

function eventMark() {
	return client.events.length;
}

function waitForEventAfter(name, mark, timeoutMs = 10000) {
	return client.waitEvent((ev) => ev.event === name && client.events.indexOf(ev) >= mark, timeoutMs, `${name} after mark`);
}

before(async () => {
	bearerMock = await startMockOAuthMcp({ requireOAuth: false, staticBearer: "mock-static-token-abc123" });
	oauthMock = await startMockOAuthMcp({ requireOAuth: true, tokenTtlSeconds: 3600 });
	fastMock = await startMockOAuthMcp({ requireOAuth: true, tokenTtlSeconds: 1 });
	writeFileSync(
		join(BRIDGE_DIR, "data", "mcp.toml"),
		`[servers.bearer]
transport = { type = "streamable_http", url = "${bearerMock.url}", auth = { type = "bearer_env", env = "MOCK_MCP_TOKEN" } }
allow_all_tools = true

[servers.oauthmock]
transport = { type = "streamable_http", url = "${oauthMock.url}", auth = { type = "oauth", scopes = ["mcp:read"], callback_timeout_ms = 30000 } }
allow_all_tools = true

[servers.oauthfast]
transport = { type = "streamable_http", url = "${fastMock.url}", auth = { type = "oauth" } }
allow_all_tools = true

[servers.localcmd]
transport = { type = "stdio", command = "node", args = ["${join(BRIDGE_DIR, "test", "mock-stdio-mcp.cjs")}"] }
enabled_tools = ["stdio.ping"]
startup_timeout_ms = 15000
`,
		{ mode: 0o600 },
	);
	client = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, join(TRACES_DIR, "m8-mcp-inventory.jsonl"));
	await client.connect();
});

after(async () => {
	client?.close();
	await bearerMock?.close();
	await oauthMock?.close();
	await fastMock?.close();
});

test("M3: mcp.reload + real mcp.inventory across bearer/oauth/stdio mocks", async () => {
	const reload = await req("mcp.reload");
	assert.equal(reload.ok, true, JSON.stringify(reload));
	assert.match(reload.revision, /^[0-9a-f]{64}$/);

	const inv = await req("mcp.inventory");
	assert.match(inv.revision, /^[0-9a-f]{64}$/);
	const byId = Object.fromEntries(inv.servers.map((s) => [s.server, s]));
	assert.deepEqual(Object.keys(byId).sort(), ["bearer", "localcmd", "oauthfast", "oauthmock"]);

	// bearer: healthy with both mock tools, filtered correctly, fingerprinted
	assert.equal(byId.bearer.health, "healthy");
	assert.deepEqual(byId.bearer.tools.map((t) => t.raw_name), ["mock.echo", "mock.time"]);
	assert.ok(byId.bearer.tools[0].context_token_estimate > 0);
	assert.match(byId.bearer.revision, /^[0-9a-f]{64}$/);

	// stdio: enabled_tools filter hides stdio.hidden
	assert.equal(byId.localcmd.health, "healthy");
	assert.deepEqual(byId.localcmd.tools.map((t) => t.raw_name), ["stdio.ping"]);

	// oauth server with no login: inventory lists it revoked (login required)
	assert.equal(byId.oauthmock.health, "revoked");
	assert.equal(byId.oauthmock.tools.length, 0);

	// mcp.status reports auth kinds without exposing tokens
	const status = await req("mcp.status");
	const st = Object.fromEntries(status.servers.map((s) => [s.server, s]));
	assert.equal(st.bearer.auth_kind, "bearer");
	assert.equal(st.bearer.status, "bearer");
	assert.equal(st.oauthmock.auth_kind, "oauth");
	assert.equal(st.oauthmock.status, "login_required");
	assert.equal(st.localcmd.auth_kind, "none");

	// control-plane mcp.call through the bearer mock
	const called = await req("mcp.call", { server: "bearer", tool: "mock.echo", arguments: { text: "M3-ROUNDTRIP" } });
	assert.equal(called.content[0].text, "M3-ROUNDTRIP");

	// enabled_tools enforcement on the stdio server
	await assert.rejects(req("mcp.call", { server: "localcmd", tool: "stdio.hidden", arguments: {} }), /not enabled/);
});

test("M2: oauth state machine — login → authorize → callback → oauth_ready → inventory healthy → refresh rotation", async (t) => {
	// swap this client's trace to the oauth trace file
	client.trace.end();
	client.tracePath = join(TRACES_DIR, "m8-mcp-oauth.jsonl");
	client.trace = (await import("node:fs")).createWriteStream(client.tracePath, { flags: "a" });

	const begin = await req("mcp.login", { server: "oauthmock" });
	assert.ok(begin.authorizationUrl.includes(oauthMock.origin + "/authorize"), begin.authorizationUrl);
	assert.ok(begin.state.length >= 16);
	assert.ok(begin.callbackPort > 0);

	// status flips to authorization_pending
	const st1 = await req("mcp.status");
	assert.equal(Object.fromEntries(st1.servers.map((s) => [s.server, s])).oauthmock.status, "authorization_pending");

	// user side: "browser" follows the authorization URL; mock auto-approves and
	// 302s to the loopback callback the bridge is listening on. Follow manually
	// (no redirect handler) so we can assert both legs.
	const authResp = await fetch(begin.authorizationUrl, { redirect: "manual" });
	assert.equal(authResp.status, 302);
	const location = authResp.headers.get("location");
	assert.ok(location.startsWith(`http://127.0.0.1:${begin.callbackPort}/oauth/callback`), location);
	const cb = new URL(location);
	assert.equal(cb.searchParams.get("state"), begin.state);
	assert.ok(cb.searchParams.get("code"));

	// drive the loopback callback (what the browser would do)
	const cbResp = await fetch(location);
	assert.equal(cbResp.status, 200);

	// mcp.authChanged broadcast arrives (connection-level event, no sessionId)
	const ev = await waitForEvent("mcp.authChanged", 10000);
	assert.equal(ev.data.server, "oauthmock");
	assert.equal(ev.data.status, "oauth_ready");

	// credential landed in the 0600 store; status oauth_ready; inventory healthy
	const st2 = await req("mcp.status");
	assert.equal(Object.fromEntries(st2.servers.map((s) => [s.server, s])).oauthmock.status, "oauth_ready");
	const inv = await req("mcp.inventory");
	const oa = inv.servers.find((s) => s.server === "oauthmock");
	assert.equal(oa.health, "healthy");
	assert.deepEqual(oa.tools.map((t) => t.raw_name), ["mock.echo", "mock.time"]);
	assert.equal(oauthMock.grants.register, 1, "DCR happened exactly once");
	assert.ok(oauthMock.grants.token >= 1);

	// tool call with the fresh token
	const called = await req("mcp.call", { server: "oauthmock", tool: "mock.echo", arguments: { text: "M2-OAUTH-CALL" } });
	assert.equal(called.content[0].text, "M2-OAUTH-CALL");

	// refresh leg: oauthfast issues 1s tokens; after expiry the next connect
	// must run the refresh_token grant exactly once and rotate the access token.
	const mark2 = eventMark();
	const begin2 = await req("mcp.login", { server: "oauthfast" });
	const authResp2 = await fetch(begin2.authorizationUrl, { redirect: "manual" });
	await fetch(authResp2.headers.get("location"));
	const ev2 = await waitForEventAfter("mcp.authChanged", mark2, 10000);
	assert.equal(ev2.data.status, "oauth_ready");

	const { OAuthCredentialRepository } = await import("../src/mcp/credentials.ts");
	const credPath = join(BRIDGE_DIR, "data", "workspace-state", "mcp-oauth-credentials.json");
	const repo = new OAuthCredentialRepository(credPath);
	const stored = await repo.get("oauthfast", fastMock.url);
	assert.ok(stored?.access_token, "credential persisted");
	assert.equal(stored.server_url, fastMock.url);
	assert.ok(stored.refresh_token, "refresh token persisted");

	await new Promise((r) => setTimeout(r, 1500)); // let the 1s token expire
	const st3 = await req("mcp.status");
	assert.equal(Object.fromEntries(st3.servers.map((s) => [s.server, s])).oauthfast.status, "reauthentication_required");
	fastMock.grants.refresh = 0;
	const called2 = await req("mcp.call", { server: "oauthfast", tool: "mock.time", arguments: {} });
	assert.equal(called2.content[0].text, "MOCK-TIME-1234");
	assert.equal(fastMock.grants.refresh, 1, "refresh_token grant used exactly once");
	const rotated = await repo.get("oauthfast", fastMock.url);
	assert.notEqual(rotated.access_token, stored.access_token, "access token rotated");
	// mode check: 0600
	const { statSync } = await import("node:fs");
	assert.equal(statSync(credPath).mode & 0o777, 0o600);
});

test("M2: denied authorization → login_required + mcp.cancel/logout semantics", async () => {
	const mark = eventMark();
	const begin = await req("mcp.login", { server: "oauthmock" });
	const denied = new URL(begin.authorizationUrl);
	denied.searchParams.set("mock_deny", "1");
	const authResp = await fetch(denied.toString(), { redirect: "manual" });
	const cbResp = await fetch(authResp.headers.get("location"));
	assert.equal(cbResp.status, 400);
	const ev = await waitForEventAfter("mcp.authChanged", mark, 10000);
	assert.equal(ev.data.status, "login_required");
	assert.match(ev.data.detail, /access_denied/);

	// cancel with nothing pending → false; start another login and cancel it
	assert.equal((await req("mcp.cancel", { server: "oauthmock" })).cancelled, false);
	await req("mcp.login", { server: "oauthmock" });
	assert.equal((await req("mcp.cancel", { server: "oauthmock" })).cancelled, true);

	// logout drops the stored credential → login_required again
	const out = await req("mcp.logout", { server: "oauthmock" });
	assert.equal(out.removed, true);
	const st = await req("mcp.status");
	assert.equal(Object.fromEntries(st.servers.map((s) => [s.server, s])).oauthmock.status, "login_required");
});
