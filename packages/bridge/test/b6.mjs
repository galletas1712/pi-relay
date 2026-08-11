// B6 — kill the BRIDGE mid-life → hosts die via stdin EOF → restart bridge →
// sessions reconciled from PG, hosts respawned, clients reattach with
// fromSeq continuity, kernel state restored.
import { BridgeClient, collectText, assert } from "./client.mjs";
import {
	BRIDGE_URL, ORIGIN, TRACES_DIR, authToken,
	killBridge, startBridge, waitDead, pidAlive, psql,
} from "./harness.mjs";
import { join } from "node:path";

const trace = join(TRACES_DIR, "m5-b6.jsonl");
const c = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, trace);
await c.connect();
const created = await c.call("session.create", { name: "b6" });
const sessionId = created.result.sessionId;
await c.call("session.attach", { id: sessionId, fromSeq: 0 });

let head = c.headSeq(sessionId);
await c.call("prompt.send", {
	sessionId,
	text: 'Use the ipython tool. Run exactly one cell: marker_b6 = "JADE-FALCON-42"; print("B6-SET"). Then reply with exactly B6-TURN1-DONE.',
});
await c.waitIdleAfter(sessionId, head, 300000);
assert(collectText(c.events, sessionId).includes("B6-TURN1-DONE"), "turn 1 done");

const st1 = (await c.call("session.getState", { sessionId })).result;
const hostPid = st1.hostPid;
const H = c.headSeq(sessionId);
console.log(`  · high-water seq=${H}, host pid=${hostPid} — killing the BRIDGE`);
await killBridge();
c.close();

// hosts must exit on bridge death (stdin EOF)
assert(await waitDead(hostPid, 15000), "session host exited after bridge kill");
const pgCount = psql(`SELECT count(*) FROM sessions WHERE id='${sessionId}'`);
assert(pgCount === "1", "session row survives in PG control plane");

console.log("  · restarting bridge");
await startBridge();

// sessions listed from PG, host respawned by boot reconciliation
const c2 = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, trace);
await c2.connect();
const list = (await c2.call("session.list")).result;
const row = list.sessions.find((s) => s.sessionId === sessionId);
assert(!!row, "session.list includes the PG-restored session");
assert(row.hostAlive === true && row.hostGeneration >= 2, `host respawned on boot (generation ${row.hostGeneration})`);
assert(row.hostPid !== hostPid, "new host pid");

// reattach from the pre-kill high-water mark: seq continuity, no gap
const att = await c2.call("session.attach", { id: sessionId, fromSeq: H });
assert(!att.error, "reattach ok");
const replayed = c2.events.filter((e) => e.sessionId === sessionId && e.replayed);
if (replayed.length) assert(replayed[0].seq === H + 1, `replay resumes at seq H+1 (${replayed[0].seq})`);
assert(
	c2.events.some((e) => e.sessionId === sessionId && e.event === "session.state" && e.data?.state === "host_respawned" && e.data?.reason === "bridge_restart"),
	"host_respawned(bridge_restart) visible on reattach",
);
assert(att.result.headSeq > H, "head advanced beyond pre-kill watermark");

// follow-up works and kernel marker survived (dispose flush + snapshot restore)
const head2 = c2.headSeq(sessionId);
await c2.call("prompt.send", {
	sessionId,
	text: 'Use the ipython tool. Run exactly one cell: print("B6-RESTORED=" + str(marker_b6)). Then reply with exactly B6-TURN2-DONE.',
});
await c2.waitIdleAfter(sessionId, head2, 300000);
const all = JSON.stringify(c2.events.filter((e) => e.sessionId === sessionId));
assert(all.includes("B6-RESTORED=JADE-FALCON-42"), "kernel marker restored after bridge restart");
assert(collectText(c2.events, sessionId).includes("B6-TURN2-DONE"), "turn 2 done after restart");
c2.close();
console.log("B6 PASS");
process.exit(0);
