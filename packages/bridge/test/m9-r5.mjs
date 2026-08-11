// M9 R5: model-cell provenance end-to-end — a second client attaching
// fromSeq=0 mid-cell replays the model cell from the spool with provenance
// "model" + tool_call_id + code echo; session.getState exposes the active
// model cell while it runs. Trace: m9-r5.jsonl
import { BridgeClient, assert, collectText } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";

const RUN = Date.now().toString(36);
const token = authToken();
const a = new BridgeClient(BRIDGE_URL, token, ORIGIN, `${TRACES_DIR}/m9-r5.jsonl`);
await a.connect();

console.log("R5: create session + capability wait");
const created = await a.call("session.create", { name: "m9-r5" });
const sid = created.result.sessionId;
await a.call("session.attach", { id: sid });
for (let i = 0; i < 60; i++) {
	const probe = await a.call("repl.execute", { sessionId: sid, code: "pass", client_cell_id: `r5-probe-${RUN}` });
	if (probe.result?.accepted) break;
	await new Promise((r) => setTimeout(r, 500));
}

console.log("R5: GLM runs a slow ipython cell");
await a.call("prompt.send", {
	sessionId: sid,
	text: "Use the ipython tool to run exactly this python code (one tool call, do not modify the code):\nimport time\ntime.sleep(8)\nprint('r5-model-out')\nThen reply with the word done.",
	idempotencyKey: `m9-r5-prompt-${RUN}`,
});
const modelQueued = await a.waitEvent(
	(ev) => ev.event === "repl.cell" && ev.sessionId === sid && ev.data?.provenance === "model" && ev.data?.status === "queued",
	120000,
	"model cell queued",
);
const cellId = modelQueued.data.cell_id;
assert(cellId.startsWith("m_"), `model cell id m_ prefixed (got ${cellId})`);
assert(typeof modelQueued.data.tool_call_id === "string" && modelQueued.data.tool_call_id.length > 0, "tool_call_id present");
assert(modelQueued.data.code?.includes("time.sleep"), "code echo present on queued frame");
await a.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === cellId && ev.data?.status === "running", 60000, "model cell running");
console.log(`  ✓ model cell ${cellId} running`);

console.log("R5: session.getState exposes the active model cell");
const stMid = await a.call("session.getState", { sessionId: sid });
assert(stMid.result?.repl?.active_cell === cellId, `getState repl.active_cell == ${cellId} (got ${stMid.result?.repl?.active_cell})`);

console.log("R5: second client attaches fromSeq=0 — model cell replays from spool");
const b = new BridgeClient(BRIDGE_URL, token, ORIGIN, `${TRACES_DIR}/m9-r5.jsonl`);
await b.connect();
const att = await b.call("session.attach", { id: sid, fromSeq: 0 });
assert(att.result?.replayed >= 0, `attach replayed ${att.result?.replayed} frames`);
const bQueued = await b.waitEvent(
	(ev) => ev.event === "repl.cell" && ev.data?.cell_id === cellId && ev.data?.status === "queued",
	30000,
	"replayed model queued on client B",
);
assert(bQueued.replayed === true, "client B saw the model cell as a replayed frame");
assert(bQueued.data.provenance === "model", "replayed frame keeps provenance model");
assert(bQueued.data.tool_call_id === modelQueued.data.tool_call_id, "replayed frame keeps tool_call_id");

console.log("R5: both clients see the cell finish with attributed output");
const doneA = await a.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === cellId && ev.data?.status === "done", 120000, "done on A");
const doneB = await b.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === cellId && ev.data?.status === "done", 120000, "done on B");
assert(doneB.data.status === "done", "client B live-done");
const outA = a.events.filter((ev) => ev.event === "repl.output" && ev.data?.cell_id === cellId);
const outB = b.events.filter((ev) => ev.event === "repl.output" && ev.data?.cell_id === cellId);
assert(outA.some((ev) => (ev.data?.data ?? "").includes("r5-model-out")), "client A sees model cell stdout");
	assert(outB.some((ev) => (ev.data?.data ?? "").includes("r5-model-out")), "client B sees model cell stdout (replay and/or live)");

await a.waitSettled(sid, 180000);
const stAfter = await a.call("session.getState", { sessionId: sid });
assert(stAfter.result?.repl?.active_cell === null, "getState repl.active_cell null after settle");
const text = collectText(a.events, sid);
assert(text.toLowerCase().includes("done"), "model replied after the tool call");

console.log("R5: PASS");
await a.call("session.delete", { sessionId: sid }).catch(() => {});
a.close();
b.close();
process.exit(0);
