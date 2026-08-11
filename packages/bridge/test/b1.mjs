// B1 — happy path: create → prompt → streamed response + tool exec;
// disconnect mid-stream → reattach with fromSeq → resumed tail with no gaps/dupes.
import { BridgeClient, collectText, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";
import { join } from "node:path";

const trace = join(TRACES_DIR, "m5-b1.jsonl");
const token = authToken();

const c1 = new BridgeClient(BRIDGE_URL, token, ORIGIN, trace);
await c1.connect();
const created = await c1.call("session.create", { idempotencyKey: `b1-create-${Date.now()}`, name: "b1" });
assert(!created.error, `session.create ok (${created.result?.sessionId})`);
const sessionId = created.result.sessionId;
const att = await c1.call("session.attach", { id: sessionId, fromSeq: 0 });
assert(!att.error && att.result.replayed >= 1, `attach replayed from seq 0 (replayed=${att.result?.replayed})`);

const p = await c1.call("prompt.send", {
	sessionId,
	text: "Use the ipython tool for this: run one cell computing the sum of squares 1 through 10 and print it as print(\"B1-SUM=\" + str(sum(i*i for i in range(1,11)))). Then reply with exactly B1-DONE.",
	idempotencyKey: `b1-prompt-${Date.now()}`,
});
assert(p.result?.accepted === true, "prompt.send accepted");

// wait until we have a handful of events (mid-stream), then drop the connection
await c1.waitEvent((ev) => ev.sessionId === sessionId && ev.seq >= 3, 240000, "seq>=3");
c1.close();
await new Promise((r) => setTimeout(r, 300));
const H = Math.max(...c1.events.filter((e) => e.sessionId === sessionId).map((e) => e.seq));
console.log(`  · conn1 high-water seq = ${H}`);

// reattach from the high-water mark on a fresh connection
const c2 = new BridgeClient(BRIDGE_URL, token, ORIGIN, trace);
await c2.connect();
const att2 = await c2.call("session.attach", { id: sessionId, fromSeq: H });
assert(!att2.error, "reattach ok");
const firstReplayed = c2.events.find((e) => e.replayed);
assert(firstReplayed === undefined || firstReplayed.seq === H + 1, `first replayed seq is H+1 (${firstReplayed?.seq})`);
assert(c2.events.filter((e) => e.replayed).every((e) => e.replayed === true), "replayed events flagged");

await c2.waitIdleAfter(sessionId, H, 300000);

// union of both connections' views must be exactly 1..finalHead, no dupes
const s1 = new Set(c1.events.filter((e) => e.sessionId === sessionId).map((e) => e.seq));
const s2 = c2.events.filter((e) => e.sessionId === sessionId).map((e) => e.seq);
const union = [...s1, ...s2].sort((a, b) => a - b);
const finalHead = union[union.length - 1];
assert(s2.every((s) => !s1.has(s)), "no duplicate seqs across reconnect");
assert(union.length === finalHead && union[0] === 1, `contiguous 1..${finalHead} with no gaps`);

const all = [...c1.events, ...c2.events].filter((e) => e.sessionId === sessionId);
assert(all.some((e) => e.event === "tool.exec" && e.data?.phase === "start" && e.data?.toolName === "ipython"), "tool.exec start (ipython)");
assert(all.some((e) => e.event === "tool.exec" && e.data?.phase === "end" && e.data?.isError === false), "tool.exec end ok");
assert(all.some((e) => e.event === "message.delta" && e.data?.kind === "text"), "streamed text deltas");
const text = collectText(all, sessionId);
assert(text.includes("B1-SUM=385") || JSON.stringify(all).includes("B1-SUM=385"), "tool printed B1-SUM=385");
assert(text.includes("B1-DONE"), "final reply B1-DONE");
const st = await c2.call("session.getState", { sessionId });
assert(st.result?.state === "idle" && st.result?.hostAlive === true, "getState idle + host alive");
c2.close();
console.log("B1 PASS");
process.exit(0);
