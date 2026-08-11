// B8 — negative security: Origin allowlist, bearer token, 8 MiB frame cap,
// typed error taxonomy, idempotency-key replay/conflict, event_gap.
import WebSocket from "ws";
import { BridgeClient, collectText, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken, psql } from "./harness.mjs";
import { join } from "node:path";

const trace = join(TRACES_DIR, "m5-b8.jsonl");
const token = authToken();

function tryUpgrade(headers) {
	return new Promise((resolve) => {
		const ws = new WebSocket(BRIDGE_URL, { headers });
		ws.on("open", () => { ws.close(); resolve({ status: 101 }); });
		ws.on("unexpected-response", (_req, res) => resolve({ status: res.statusCode }));
		ws.on("error", () => resolve({ status: -1 }));
	});
}

// 1. origin discipline
let r = await tryUpgrade({ Authorization: `Bearer ${token}`, Origin: "https://evil.example" });
assert(r.status === 403, `disallowed origin rejected (${r.status})`);
r = await tryUpgrade({ Authorization: `Bearer ${token}` }); // no Origin at all
assert(r.status === 403, `missing origin rejected (${r.status})`);
r = await tryUpgrade({ Authorization: `Bearer ${token}`, Origin: ORIGIN });
assert(r.status === 101, "allowlisted origin accepted");

// 2. bearer token
r = await tryUpgrade({ Authorization: "Bearer wrong-token", Origin: ORIGIN });
assert(r.status === 401, `bad token rejected (${r.status})`);
r = await tryUpgrade({ Origin: ORIGIN });
assert(r.status === 401, `missing token rejected (${r.status})`);

// 3. frame cap: 9 MiB single message must be killed with 1009
{
	const ws = new WebSocket(BRIDGE_URL, { headers: { Authorization: `Bearer ${token}`, Origin: ORIGIN }, maxPayload: 32 * 1024 * 1024 });
	await new Promise((res, rej) => { ws.on("open", res); ws.on("error", rej); });
	const big = JSON.stringify({ id: "big", method: "session.list", padding: "x".repeat(9 * 1024 * 1024) });
	const closeCode = await new Promise((res) => {
		ws.on("close", (code) => res(code));
		ws.send(big);
	});
	assert(closeCode === 1009, `oversize frame closed with 1009 (got ${closeCode})`);
}

// 4. typed errors on a healthy connection
const c = new BridgeClient(BRIDGE_URL, token, ORIGIN, trace);
await c.connect();
c.ws.send("this is not json");
await new Promise((res) => setTimeout(res, 300));
let bad = await c.call("no.such.method", {});
assert(bad.error?.code === "method_not_found", "method_not_found typed");
// M8: workspace.* + mcp.* are REAL now — assert the implemented-surface errors.
bad = await c.call("workspace.read_file", { sessionId: "x" });
assert(bad.error?.code === "session_not_found" || bad.error?.code === "bad_request", `workspace.read_file typed (got ${bad.error?.code})`);
{
	const inv = await c.call("mcp.inventory", {});
	assert(inv.error === undefined && typeof inv.result === "object", "mcp.inventory real");
}
bad = await c.call("session.getState", { sessionId: "00000000-0000-0000-0000-000000000000" });
assert(bad.error?.code === "session_not_found", "session_not_found typed");
bad = await c.call("prompt.send", { sessionId: "x" }); // missing text
assert(bad.error?.code === "bad_request", "bad_request typed");

// 5. idempotency: session.create
const key = `b8-create-${Date.now()}`;
const first = await c.call("session.create", { idempotencyKey: key, name: "b8" });
const second = await c.call("session.create", { idempotencyKey: key, name: "b8" });
assert(second.result?.sessionId === first.result?.sessionId && second.result?.replay === true, "create replay returns same session");
const conflict = await c.call("session.create", { idempotencyKey: key, name: "different" });
assert(conflict.error?.code === "idempotency_conflict", "same key + different params → idempotency_conflict");
const sessionId = first.result.sessionId;

// 6. idempotency: prompt.send applies once
await c.call("session.attach", { id: sessionId, fromSeq: 0 });
const head = c.headSeq(sessionId);
const pkey = `b8-prompt-${Date.now()}`;
const p1 = await c.call("prompt.send", { sessionId, text: 'Reply with exactly B8-ONCE.', idempotencyKey: pkey });
const p2 = await c.call("prompt.send", { sessionId, text: 'Reply with exactly B8-ONCE.', idempotencyKey: pkey });
assert(p1.result?.accepted === true && p2.result?.replay === true, "prompt replay flagged");
await c.waitIdleAfter(sessionId, head, 300000);
const n = psql(`SELECT count(*) FROM command_journal WHERE session_id='${sessionId}' AND idempotency_key='${pkey}'`);
assert(n === "1", `exactly one journal row for the key (${n})`);
const userStarts = psql(
	`SELECT count(*) FROM event_spool WHERE session_id='${sessionId}' AND event='message.delta' AND payload->>'kind'='start' AND payload->>'role'='user'`,
);
assert(userStarts === "1", `prompt applied exactly once (${userStarts} user message in spool)`);

// 7. event_gap: trim spool rows directly (test PG), then attach below the floor
psql(`DELETE FROM event_spool WHERE session_id='${sessionId}' AND seq <= 2`);
const c2 = new BridgeClient(BRIDGE_URL, token, ORIGIN, trace);
await c2.connect();
const gap = await c2.call("session.attach", { id: sessionId, fromSeq: 0 });
assert(gap.error?.code === "event_gap" && gap.error?.data?.minAvailable >= 3, `event_gap typed with minAvailable (${gap.error?.data?.minAvailable})`);
c2.close();
c.close();
console.log("B8 PASS");
process.exit(0);
