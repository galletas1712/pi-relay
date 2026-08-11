// M9 R2: mid-turn serialization. GLM runs a long ipython cell; a user
// repl.execute lands mid-cell and must queue at the KERNEL (not the agent),
// run only after the model cell finishes, with clean per-cell output
// attribution and no extra agent turn. Trace: m9-r2.jsonl
import { BridgeClient, assert, collectText } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";

const token = authToken();
const RUN = Date.now().toString(36);
const client = new BridgeClient(BRIDGE_URL, token, ORIGIN, `${TRACES_DIR}/m9-r2.jsonl`);
await client.connect();

console.log("R2: create session + capability wait");
const created = await client.call("session.create", { name: "m9-r2" });
const sid = created.result.sessionId;
await client.call("session.attach", { id: sid });
for (let i = 0; i < 60; i++) {
	const ack = await client.call("repl.execute", { sessionId: sid, code: "pass", client_cell_id: "r2-probe" });
	if (ack.result?.accepted) break;
	if (ack.error?.code !== "repl_unavailable") throw new Error(`probe error: ${JSON.stringify(ack.error)}`);
	await new Promise((r) => setTimeout(r, 500));
}

console.log("R2: prompt GLM to run a long ipython cell");
const head0 = client.headSeq(sid);
const prompt = await client.call("prompt.send", {
	sessionId: sid,
	text: "Use the ipython tool to run exactly this python code (one tool call, do not modify the code):\nimport time\ntime.sleep(10)\nprint('model-slept')\nThen reply with the word done.",
	idempotencyKey: `m9-r2-prompt-${RUN}`,
});
assert(prompt.result?.seq !== undefined, "prompt accepted");

console.log("R2: wait for the model's cell (queued carries provenance/tool_call_id)");
const modelQueued = await client.waitEvent(
	(ev) => ev.event === "repl.cell" && ev.sessionId === sid && ev.data?.provenance === "model" && ev.data?.status === "queued",
	120000,
	"model cell queued",
);
const modelCellId = modelQueued.data.cell_id;
assert(modelCellId.startsWith("m_"), `model cell id has m_ prefix (got ${modelCellId})`);
assert(typeof modelQueued.data.tool_call_id === "string", "model cell carries tool_call_id");
assert(modelQueued.data.code?.includes("time.sleep"), "model cell queued carries code echo");
const modelRunning = await client.waitEvent(
	(ev) => ev.event === "repl.cell" && ev.data?.cell_id === modelCellId && ev.data?.status === "running",
	120000,
	"model cell running",
);
console.log(`  ✓ model cell ${modelCellId} running (tool_call_id ${modelQueued.data.tool_call_id})`);

console.log("R2: user repl.execute lands mid-model-cell");
const mid = await client.call("repl.execute", { sessionId: sid, code: "r2_marker = 99\nprint('user-ran')", client_cell_id: "r2-mid" });
assert(mid.result?.accepted === true && mid.result.cell_id === "u_r2-mid", "user cell accepted mid-turn");

const userQueued = await client.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === "u_r2-mid" && ev.data?.status === "queued", 15000, "user cell queued");
assert(userQueued.data.provenance === "user", "user cell provenance user");
assert(userQueued.seq > modelRunning.seq, "user queued strictly after model running");
assert((userQueued.data.position ?? 0) >= 0, "user queued carries position");

const modelDone = await client.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === modelCellId && (ev.data?.status === "done" || ev.data?.status === "error"), 120000, "model cell settled");
assert(modelDone.data.status === "done", `model cell done (got ${modelDone.data.status})`);
const userRunning = await client.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === "u_r2-mid" && ev.data?.status === "running", 30000, "user cell running");
assert(userRunning.seq > modelDone.seq, "SERIALIZATION: user cell ran only after model cell finished");
console.log(`  ✓ serialization: user running seq ${userRunning.seq} > model done seq ${modelDone.seq}`);

const userDone = await client.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === "u_r2-mid" && ev.data?.status === "done", 30000, "user cell done");
assert(userDone.seq > userRunning.seq, "user done after running");

console.log("R2: output attribution");
const modelOut = client.events.filter((ev) => ev.event === "repl.output" && ev.data?.cell_id === modelCellId);
const userOut = client.events.filter((ev) => ev.event === "repl.output" && ev.data?.cell_id === "u_r2-mid");
assert(modelOut.some((ev) => (ev.data?.data ?? "").includes("model-slept")), "model cell output has model-slept");
assert(!modelOut.some((ev) => (ev.data?.data ?? "").includes("user-ran")), "model cell output does NOT contain user output");
assert(userOut.some((ev) => (ev.data?.data ?? "").includes("user-ran")), "user cell output has user-ran");
assert(!userOut.some((ev) => (ev.data?.data ?? "").includes("model-slept")), "user cell output does NOT contain model output");

console.log("R2: turn settles idle with no extra turn from the user cell");
await client.waitSettled(sid, 180000);
const stateEvents = client.events.filter((ev) => ev.event === "session.state" && ev.sessionId === sid && ev.seq > head0);
const runningCount = stateEvents.filter((ev) => ev.data?.state === "running").length;
assert(runningCount === 1, `exactly one running state (one turn; user cell started no turn) — got ${runningCount}`);
const text = collectText(client.events, sid);
assert(!text.includes("user-ran"), "model transcript text does not echo user cell output");
console.log("  ✓ single turn; transcript free of user-cell output");

console.log("R2: PASS");
await client.call("session.delete", { sessionId: sid }).catch(() => {});
client.close();
process.exit(0);
