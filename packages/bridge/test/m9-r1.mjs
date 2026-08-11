// M9 R1: basic user-cell lifecycle — repl.execute → queued → running → done,
// stdout streamed, shared namespace across cells, typed error cell, display
// image output, and session.getState repl block. Trace: m9-r1.jsonl
import { BridgeClient, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";

const token = authToken();
const client = new BridgeClient(BRIDGE_URL, token, ORIGIN, `${TRACES_DIR}/m9-r1.jsonl`);
await client.connect();

console.log("R1: create session");
const created = await client.call("session.create", { name: "m9-r1" });
assert(created.result?.sessionId, "session.create returned a sessionId");
const sid = created.result.sessionId;
await client.call("session.attach", { id: sid });

// wait for prime-rlm repl capability (extension session_start marker). Retry a
// no-op cell with a fixed client_cell_id: idempotent, runs at most once.
console.log("R1: wait for repl capability");
let ack = null;
for (let i = 0; i < 60; i++) {
	ack = await client.call("repl.execute", { sessionId: sid, code: "pass", client_cell_id: "r1-probe" });
	if (ack.result?.accepted) break;
	if (ack.error?.code !== "repl_unavailable") throw new Error(`unexpected probe error: ${JSON.stringify(ack.error)}`);
	await new Promise((r) => setTimeout(r, 500));
}
assert(ack?.result?.accepted === true, "repl.execute accepted after capability wait");
assert(ack.result.cell_id === "u_r1-probe", `deterministic cell_id u_r1-probe (got ${ack?.result?.cell_id})`);

console.log("R1: cell A — stdout + shared namespace");
const cellA = await client.call("repl.execute", { sessionId: sid, code: "x = 41\nprint('m9-r1 hello')", client_cell_id: "r1-a" });
assert(cellA.result?.accepted && cellA.result.cell_id === "u_r1-a", "cell A accepted as u_r1-a");

const queuedA = await client.waitEvent((ev) => ev.event === "repl.cell" && ev.sessionId === sid && ev.data?.cell_id === "u_r1-a" && ev.data?.status === "queued", 30000, "cell A queued");
assert(queuedA.data.provenance === "user", "cell A queued with provenance user");
assert(queuedA.data.code.includes("m9-r1 hello"), "cell A queued carries code echo");
assert(typeof queuedA.data.queued_at === "string", "cell A queued_at present");

const runningA = await client.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === "u_r1-a" && ev.data?.status === "running", 30000, "cell A running");
assert(runningA.seq > queuedA.seq, "running after queued");
assert(typeof runningA.data.started_at === "string", "cell A started_at present");

const outA = await client.waitEvent((ev) => ev.event === "repl.output" && ev.data?.cell_id === "u_r1-a" && ev.data?.stream === "stdout" && (ev.data?.data ?? "").includes("m9-r1 hello"), 30000, "cell A stdout");
assert(outA.seq > runningA.seq, "stdout after running");

const doneA = await client.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === "u_r1-a" && ev.data?.status === "done", 30000, "cell A done");
assert(doneA.seq > outA.seq, "done after stdout");
assert(typeof doneA.data.duration_ms === "number" && doneA.data.duration_ms >= 0, "cell A duration_ms present");
assert(typeof doneA.data.finished_at === "string", "cell A finished_at present");

console.log("R1: cell B — shared namespace (x + 1)");
const cellB = await client.call("repl.execute", { sessionId: sid, code: "print(x + 1)", client_cell_id: "r1-b" });
assert(cellB.result?.accepted, "cell B accepted");
await client.waitEvent((ev) => ev.event === "repl.output" && ev.data?.cell_id === "u_r1-b" && (ev.data?.data ?? "").trim() === "42", 30000, "cell B output 42");
await client.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === "u_r1-b" && ev.data?.status === "done", 30000, "cell B done");
console.log("  ✓ shared namespace: x + 1 == 42");

console.log("R1: cell C — error cell");
const cellC = await client.call("repl.execute", { sessionId: sid, code: "raise ValueError('r1-boom')", client_cell_id: "r1-c" });
assert(cellC.result?.accepted, "cell C accepted");
await client.waitEvent((ev) => ev.event === "repl.output" && ev.data?.cell_id === "u_r1-c" && ev.data?.stream === "error" && (ev.data?.data ?? "").includes("r1-boom"), 30000, "cell C traceback output");
const errC = await client.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === "u_r1-c" && ev.data?.status === "error", 30000, "cell C error status");
assert(errC.data.error?.ename === "ValueError", `cell C ename ValueError (got ${errC.data.error?.ename})`);
assert(errC.data.error?.evalue.includes("r1-boom"), "cell C evalue carries message");

console.log("R1: cell D — display image (png)");
// 1x1 transparent PNG
const pngB64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
const cellD = await client.call("repl.execute", {
	sessionId: sid,
	code: `from IPython.display import Image, display\nimport base64\ndisplay(Image(data=base64.b64decode("${pngB64}")))`,
	client_cell_id: "r1-d",
});
assert(cellD.result?.accepted, "cell D accepted");
const imgD = await client.waitEvent((ev) => ev.event === "repl.output" && ev.data?.cell_id === "u_r1-d" && ev.data?.stream === "display", 30000, "cell D display output");
assert(imgD.data.mime_type === "image/png", `cell D mime image/png (got ${imgD.data.mime_type})`);
assert(imgD.data.data.length > 50, "cell D image payload present");
await client.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === "u_r1-d" && ev.data?.status === "done", 30000, "cell D done");

console.log("R1: session.getState repl block");
const st = await client.call("session.getState", { sessionId: sid });
assert(st.result?.repl, "getState has repl block");
assert(st.result.repl.active_cell === null, "repl.active_cell null when idle");
assert(st.result.repl.queue_depth === 0, "repl.queue_depth 0 when idle");

console.log("R1: repl event seq contiguity");
const replEvents = client.events.filter((ev) => ev.sessionId === sid && ev.event.startsWith("repl."));
assert(replEvents.length >= 12, `at least 12 repl events (got ${replEvents.length})`);
for (let i = 1; i < replEvents.length; i++) {
	assert(replEvents[i].seq > replEvents[i - 1].seq, `repl seqs strictly increasing (${replEvents[i - 1].seq} -> ${replEvents[i].seq})`);
}

console.log("R1: PASS");
await client.call("session.delete", { sessionId: sid }).catch(() => {});
client.close();
process.exit(0);
