// B7 — prime-rlm subagent visibility: spawn an rlm child via the ipython
// tool → subagent.lifecycle events stream (admitted → completed) and
// subagent.tree exposes the child row.
import { BridgeClient, collectText, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";
import { join } from "node:path";

const trace = join(TRACES_DIR, "m5-b7.jsonl");
const c = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, trace);
await c.connect();
const sessionId = (await c.call("session.create", { name: "b7" })).result.sessionId;
await c.call("session.attach", { id: sessionId, fromSeq: 0 });
const head = c.headSeq(sessionId);

await c.call("prompt.send", {
	sessionId,
	text: [
		"Use the ipython tool for everything below. The `rlm` object and the `agent_message` module are available in your kernel.",
		"",
		"Run exactly ONE ipython cell containing ALL of the following code (nothing else):",
		"",
		'h = await rlm("In ipython run exactly: await agent_message.send(\"B7-CHILD-PONG\", receiver_role=\"parent\") — then stop.", name="b7child")',
		'print("CHILD_SPAWNED", h)',
		"",
		'Then END YOUR TURN immediately. Reply with only "B7-SPAWNED" and stop. Do NOT poll or wait for the child — its message will arrive later and wake you up.',
	].join("\n"),
});

const admitted = await c.waitEvent(
	(ev) => ev.sessionId === sessionId && ev.event === "subagent.lifecycle" && ev.data?.phase === "admitted",
	600000,
	"subagent admitted",
);
assert(admitted.data.session_name === "b7child", `lifecycle admitted for b7child (rlm_child_id=${admitted.data.rlm_child_id})`);
await c.waitIdleAfter(sessionId, head, 600000);
assert(collectText(c.events, sessionId).includes("B7-SPAWNED"), "spawn turn completed");

// child runs: wait for completion lifecycle
const completed = await c.waitEvent(
	(ev) => ev.sessionId === sessionId && ev.event === "subagent.lifecycle" && ev.data?.phase === "completed",
	600000,
	"subagent completed",
);
assert(completed.data.rlm_child_id === admitted.data.rlm_child_id, "completed phase for the same child");

const tree = (await c.call("subagent.tree", { sessionId })).result;
assert(!tree.error, "subagent.tree ok");
const childRow = tree.children.find((ch) => ch.name === "b7child");
assert(!!childRow, "tree contains b7child");
assert(childRow.depth === 1, "child at depth 1");
assert(childRow.status === "completed", `child status completed (got ${childRow.status})`);
console.log("  · tree:", JSON.stringify(tree.children));
c.close();
console.log("B7 PASS");
process.exit(0);
