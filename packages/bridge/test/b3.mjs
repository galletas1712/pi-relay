// B3 — kill -9 the session host mid-turn → supervisor respawns (pi --session
// resume) → getState consistent → follow-up works → prime-rlm kernel state
// restored from the post-cell snapshot.
import { BridgeClient, collectText, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, BRIDGE_DIR, authToken } from "./harness.mjs";
import { join } from "node:path";
import { existsSync } from "node:fs";

const trace = join(TRACES_DIR, "m5-b3.jsonl");
const c = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, trace);
await c.connect();
const created = await c.call("session.create", { name: "b3" });
const sessionId = created.result.sessionId;
await c.call("session.attach", { id: sessionId, fromSeq: 0 });

// turn 1: set a kernel variable (fast cell → debounced snapshot flush)
let head = c.headSeq(sessionId);
await c.call("prompt.send", {
	sessionId,
	text: "Use the ipython tool. Run exactly one cell: marker_b3 = \"TOPAZ-OWL-77\"; print(\"B3-MARKER-SET\"). Then reply with exactly B3-TURN1-DONE.",
});
await c.waitIdleAfter(sessionId, head, 300000);
assert(collectText(c.events, sessionId).includes("B3-TURN1-DONE"), "turn 1 done");

// wait for the kernel snapshot to hit disk (debounce is 1500ms post-cell)
const snap = join(BRIDGE_DIR, "data", "sessions", "prime", sessionId, "kernel", "kernel-state.dill");
{
	const t0 = Date.now();
	while (!existsSync(snap) && Date.now() - t0 < 15000) await new Promise((r) => setTimeout(r, 250));
}
assert(existsSync(snap), `kernel snapshot written (${snap})`);

// turn 2: long sleep cell; kill -9 the host while the cell is running
head = c.headSeq(sessionId);
const st1 = await c.call("session.getState", { sessionId });
const hostPid = st1.result.hostPid;
assert(Number.isInteger(hostPid) && hostPid > 0, `host pid known (${hostPid})`);
await c.call("prompt.send", {
	sessionId,
	text: "Use the ipython tool. Run exactly one cell: import asyncio; await asyncio.sleep(45); print(\"B3-CELL2-DONE\"). Then reply with exactly B3-TURN2-DONE.",
});
await c.waitEvent(
	(ev) => ev.sessionId === sessionId && ev.event === "tool.exec" && ev.data?.phase === "start" && ev.seq > head,
	240000,
	"cell2 start",
);
process.kill(hostPid, "SIGKILL");
console.log(`  · killed host pid ${hostPid} mid-cell`);

const exited = await c.waitEvent(
	(ev) => ev.sessionId === sessionId && ev.event === "session.state" && ev.data?.state === "host_exited",
	20000,
	"host_exited",
);
assert(!!exited, "session.state host_exited emitted");
const crashed = await c.waitEvent(
	(ev) => ev.sessionId === sessionId && ev.event === "session.error" && ev.data?.code === "host_crashed",
	20000,
	"host_crashed",
);
assert(crashed.data.maybeLostQueued !== undefined, "session.error host_crashed emitted with maybe-lost count");
const resp = await c.waitEvent(
	(ev) => ev.sessionId === sessionId && ev.event === "session.state" && ev.data?.state === "host_respawned",
	120000,
	"host_respawned",
);
assert(resp.data.generation >= 2, `host respawned (generation ${resp.data.generation})`);

// getState consistent after respawn
const st2 = await c.call("session.getState", { sessionId });
assert(st2.result.state === "idle" && st2.result.hostAlive === true, "getState idle + host alive after respawn");
assert(st2.result.hostGeneration >= 2 && st2.result.hostPid !== hostPid, "generation bumped, new pid");

// follow-up: kernel variable must survive via snapshot restore
head = c.headSeq(sessionId);
await c.call("prompt.send", {
	sessionId,
	text: "Use the ipython tool. Run exactly one cell: print(\"B3-RESTORED=\" + str(marker_b3)). Then reply with the exact line the cell printed.",
});
await c.waitIdleAfter(sessionId, head, 300000);
const all = JSON.stringify(c.events.filter((e) => e.sessionId === sessionId));
assert(all.includes("B3-RESTORED=TOPAZ-OWL-77"), "kernel marker restored after kill -9");
c.close();
console.log("B3 PASS");
process.exit(0);
