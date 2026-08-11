// B4 — commands sent while the host is DOWN stay pending in the durable
// journal and are delivered, in order, after respawn. Sends a followUp AND a
// prompt while down. rpc follow_up on an idle host only queues in-memory
// (rpc-park), so the bridge converts it to `prompt` at drain time (turn 1),
// then paces the journaled prompt behind turn-1 settle (turn 2).
import { BridgeClient, collectText, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken, stopBridge, startBridge, psql } from "./harness.mjs";
import { join } from "node:path";

const trace = join(TRACES_DIR, "m5-b4.jsonl");

console.log("  · restarting bridge with BRIDGE_RESPAWN_DELAY_MS=20000");
await stopBridge();
await startBridge({ BRIDGE_RESPAWN_DELAY_MS: "20000" });

const c = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, trace);
await c.connect();
const created = await c.call("session.create", { name: "b4" });
const sessionId = created.result.sessionId;
await c.call("session.attach", { id: sessionId, fromSeq: 0 });

let head = c.headSeq(sessionId);
await c.call("prompt.send", {
	sessionId,
	text: 'Use the ipython tool. Run exactly one cell: print("B4-TURN1"). Then reply with exactly B4-TURN1-DONE.',
});
await c.waitIdleAfter(sessionId, head, 300000);
assert(collectText(c.events, sessionId).includes("B4-TURN1-DONE"), "turn 1 done");

const st1 = await c.call("session.getState", { sessionId });
const hostPid = st1.result.hostPid;
process.kill(hostPid, "SIGKILL");
console.log(`  · killed host pid ${hostPid}`);
await c.waitEvent(
	(ev) => ev.sessionId === sessionId && ev.event === "session.state" && ev.data?.state === "host_exited",
	20000,
	"host_exited",
);

// host down for ~20s: queue a follow-up AND a trigger prompt, in that order
const fu = await c.call("session.followUp", {
	sessionId,
	text: 'Use the ipython tool. Run exactly one cell: print("B4-DELIVERED"). Then reply with exactly B4-FOLLOWUP-DONE.',
});
assert(fu.result?.accepted === true && fu.result?.queued === true, "followUp queued while host down");
const pr = await c.call("prompt.send", {
	sessionId,
	text: "Reply with exactly B4-TRIGGER-DONE.",
});
assert(pr.result?.accepted === true && pr.result?.queued === true, "prompt queued while host down");
const rows = psql(`SELECT count(*) FROM command_journal WHERE session_id='${sessionId}' AND status='pending'`);
assert(rows === "2", `both commands pending in journal (${rows})`);

// after the delay: respawn → drain converts follow_up→prompt (turn 1), paces,
// then delivers the journaled prompt (turn 2)
await c.waitEvent(
	(ev) => ev.sessionId === sessionId && ev.event === "session.state" && ev.data?.state === "host_respawned",
	120000,
	"host_respawned",
);
head = c.headSeq(sessionId);
await c.waitIdleAfter(sessionId, head, 300000);
let text = collectText(c.events, sessionId);
assert(JSON.stringify(c.events.filter((e) => e.sessionId === sessionId)).includes("B4-DELIVERED"), "queued follow-up ran as turn 1 (B4-DELIVERED printed)");
assert(text.includes("B4-FOLLOWUP-DONE"), "converted follow-up turn completed");
head = c.headSeq(sessionId);
await c.waitIdleAfter(sessionId, head, 300000);
text = collectText(c.events, sessionId);
assert(text.includes("B4-TRIGGER-DONE"), "queued prompt ran as turn 2 after pacing");
const pending = psql(`SELECT count(*) FROM command_journal WHERE session_id='${sessionId}' AND status='pending'`);
assert(pending === "0", "journal fully drained (0 pending)");
c.close();

console.log("  · restoring bridge with default env");
await stopBridge();
await startBridge();
console.log("B4 PASS");
process.exit(0);
