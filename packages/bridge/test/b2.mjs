// B2 — steer mid-turn changes behavior: long-running cell, steer while it
// runs, model must incorporate the steering message after the cell returns.
import { BridgeClient, collectText, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";
import { join } from "node:path";

const trace = join(TRACES_DIR, "m5-b2.jsonl");
const c = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, trace);
await c.connect();
const created = await c.call("session.create", { name: "b2" });
const sessionId = created.result.sessionId;
await c.call("session.attach", { id: sessionId, fromSeq: 0 });
const head = c.headSeq(sessionId);

const p = await c.call("prompt.send", {
	sessionId,
	text: [
		"Use the ipython tool for this. Run exactly ONE ipython cell with this code:",
		"",
		"import asyncio",
		"await asyncio.sleep(30)",
		'print("B2-BASE")',
		"",
		"When the cell returns, end your turn replying with exactly B2-TURN-DONE and nothing else.",
	].join("\n"),
});
assert(p.result?.accepted === true, "prompt accepted");

// steer while the sleep cell is running
await c.waitEvent(
	(ev) => ev.sessionId === sessionId && ev.event === "tool.exec" && ev.data?.phase === "start",
	240000,
	"tool start",
);
const st = await c.call("session.steer", {
	sessionId,
	text: "Change of plan: after the sleep cell returns, your reply must instead be exactly B2-STEERED (one line, nothing else).",
});
assert(st.result?.accepted === true && st.result?.queued === false, "steer accepted while running");
const busy = await c.call("prompt.send", { sessionId, text: "should be rejected" });
assert(busy.error?.code === "session_busy", "prompt.send while running → session_busy");

await c.waitIdleAfter(sessionId, head, 300000);
const text = collectText(c.events, sessionId);
console.log("  · final text:", JSON.stringify(text.slice(-200)));
assert(text.includes("B2-STEERED"), "final reply reflects the steer (B2-STEERED)");
c.close();
console.log("B2 PASS");
process.exit(0);
