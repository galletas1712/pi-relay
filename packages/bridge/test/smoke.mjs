// Smoke: create session, prompt, wait idle, print final text.
import { BridgeClient, collectText } from "./client.mjs";

const url = process.env.BRIDGE_URL ?? "ws://127.0.0.1:8730";
const token = process.env.BRIDGE_AUTH_TOKEN;
const trace = process.env.TRACE ?? "/tmp/m5-smoke.jsonl";

const c = new BridgeClient(url, token, "http://localhost:3000", trace);
await c.connect();
const created = await c.call("session.create", { idempotencyKey: "smoke-1" });
console.log("create →", JSON.stringify(created.result ?? created.error));
const sessionId = created.result.sessionId;
await c.call("session.attach", { id: sessionId, fromSeq: 0 });
const head = c.headSeq(sessionId);
const p = await c.call("prompt.send", {
	sessionId,
	text: "Reply with exactly SMOKE-OK and nothing else.",
	idempotencyKey: "smoke-prompt-2",
});
console.log("prompt →", JSON.stringify(p.result ?? p.error));
await c.waitIdleAfter(sessionId, head, 300000);
const text = collectText(c.events, sessionId);
console.log("FINAL TEXT:", JSON.stringify(text));
const state = await c.call("session.getState", { sessionId });
console.log("state →", JSON.stringify(state.result, null, 1).slice(0, 600));
c.close();
process.exit(0);
