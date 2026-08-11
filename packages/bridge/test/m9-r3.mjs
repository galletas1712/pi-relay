// M9 R3: kill the socket mid-cell; reconnect with fromSeq=watermark; the
// bridge replays watermark+1..head contiguously (replayed:true) and the cell
// finishes live with every tick accounted for exactly once. Trace: m9-r3.jsonl
import { BridgeClient, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";

const token = authToken();
const client = new BridgeClient(BRIDGE_URL, token, ORIGIN, `${TRACES_DIR}/m9-r3.jsonl`);
await client.connect();

console.log("R3: create session + capability wait");
const created = await client.call("session.create", { name: "m9-r3" });
const sid = created.result.sessionId;
await client.call("session.attach", { id: sid });
for (let i = 0; i < 60; i++) {
	const ack = await client.call("repl.execute", { sessionId: sid, code: "pass", client_cell_id: "r3-probe" });
	if (ack.result?.accepted) break;
	await new Promise((r) => setTimeout(r, 500));
}

console.log("R3: long ticking cell");
const cell = await client.call("repl.execute", {
	sessionId: sid,
	code: "import time\nfor i in range(30):\n    print(f'tick{i:02d}', flush=True)\n    time.sleep(0.2)",
	client_cell_id: "r3-tick",
});
assert(cell.result?.accepted, "tick cell accepted");

await client.waitEvent((ev) => ev.event === "repl.output" && ev.data?.cell_id === "u_r3-tick" && (ev.data?.data ?? "").includes("tick04"), 30000, "first ticks");
const watermark = client.headSeq(sid);
console.log(`  ✓ ticks flowing; killing socket at watermark ${watermark}`);

console.log("R3: hard-kill the socket");
	client.ws.terminate();
await new Promise((r) => setTimeout(r, 300));
client.close();

console.log("R3: reconnect + attach at watermark");
const client2 = new BridgeClient(BRIDGE_URL, token, ORIGIN, `${TRACES_DIR}/m9-r3.jsonl`);
await client2.connect();
const attach = await client2.call("session.attach", { id: sid, fromSeq: watermark });
assert(attach.result?.headSeq >= watermark, "attach returned headSeq");

const done = await client2.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === "u_r3-tick" && ev.data?.status === "done", 60000, "tick cell done on live stream");
console.log(`  ✓ cell done at seq ${done.seq}`);

console.log("R3: replay contiguity — frames from watermark+1, strictly +1, no dups");
const evs = client2.events.filter((ev) => ev.sessionId === sid);
assert(evs.length > 0, "client2 received events");
const replayed = evs.filter((ev) => ev.replayed === true);
assert(replayed.length > 0, "replay frames flagged replayed:true");
assert(replayed[0].seq === watermark + 1, `replay starts exactly at watermark+1 (${replayed[0].seq} == ${watermark + 1})`);
for (let i = 1; i < evs.length; i++) {
	assert(evs[i].seq === evs[i - 1].seq + 1, `contiguous ${evs[i - 1].seq} -> ${evs[i].seq}`);
}
const seqs = new Set(evs.map((ev) => ev.seq));
assert(seqs.size === evs.length, "no duplicate seqs on the reconnected stream");

console.log("R3: tick completeness across kill (union of both sockets, deduped by seq)");
const outputs = new Map();
for (const c of [client, client2]) {
	for (const ev of c.events) {
		if (ev.event === "repl.output" && ev.data?.cell_id === "u_r3-tick" && ev.data?.stream === "stdout") {
			outputs.set(ev.seq, ev.data.data ?? "");
		}
	}
}
const allText = [...outputs.values()].join("");
const ticks = new Set([...allText.matchAll(/tick(\d\d)/g)].map((m) => Number(m[1])));
assert(ticks.size === 30, `all 30 ticks present exactly (got ${ticks.size})`);

console.log("R3: PASS");
await client2.call("session.delete", { sessionId: sid }).catch(() => {});
client2.close();
process.exit(0);
