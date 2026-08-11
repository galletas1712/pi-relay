// M8 G1+G2 — session titles (sidecar GLM via shim) and transcript rebuild.
// G1: unnamed session → first settle → session.renamed with a generated title.
// G2: spool trimmed → attach(0) fails with event_gap → getState carries
// transcript.blocks rebuilt from the pi session JSONL (user text + tool calls).
// Trace: m8-gaps.jsonl
import { readFileSync } from "node:fs";
import { BridgeClient, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken, psql } from "./harness.mjs";

const c = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, join0(TRACES_DIR, "m8-gaps.jsonl"));
function join0(a, b) { return `${a}/${b}`; }
await c.connect();
const idem = `m8gaps-${Date.now()}`;

const created = await c.call("session.create", { idempotencyKey: `${idem}-sess` });
assert(!created.error, "session.create ok");
const sessionId = created.result.sessionId;
await c.call("session.attach", { id: sessionId, fromSeq: 0 });

const MARKER = `G2-TRANSCRIPT-MARKER-${Date.now() % 100000}`;
const p = await c.call("prompt.send", {
	sessionId,
	text: `Reply with exactly this line and nothing else: ${MARKER}`,
	idempotencyKey: `${idem}-prompt`,
});
assert(p.result?.accepted === true, "prompt accepted");
await c.waitIdleAfter(sessionId, c.headSeq(sessionId), 300000);

// ---- G1: title sidecar -------------------------------------------------------
let renamed = null;
const deadline = Date.now() + 120000;
while (Date.now() < deadline) {
	renamed = c.events.find((e) => e.event === "session.renamed" && e.sessionId === sessionId);
	if (renamed) break;
	await new Promise((r) => setTimeout(r, 1000));
}
assert(renamed, "session.renamed emitted after first settle");
const title = renamed.data.name;
assert(typeof title === "string" && title.length >= 3 && title.length <= 80, `title shape (${JSON.stringify(title)})`);
console.log("  ✓ title:", JSON.stringify(title));

const st1 = await c.call("session.getState", { sessionId });
assert(st1.result.name === title, "getState reflects the generated title");

// ---- G2: transcript rebuild ---------------------------------------------------
const st2 = await c.call("session.getState", { sessionId });
const blocks = st2.result.transcript?.blocks;
assert(Array.isArray(blocks) && blocks.length >= 2, `transcript.blocks present (${blocks?.length})`);
const userBlock = blocks.find((b) => b.kind === "message" && b.role === "user" && b.text.includes(MARKER));
assert(userBlock, "user message block carries the marker");
const assistantBlock = blocks.find((b) => b.kind === "message" && b.role === "assistant" && b.text.includes(MARKER));
assert(assistantBlock, "assistant reply block carries the marker");

// trimmed-spool recovery path: wipe the spool, attach from 0 → event_gap,
// then getState → transcript.blocks is how the client rebuilds.
psql(`DELETE FROM event_spool WHERE session_id = '${sessionId}'`);
await c.call("session.detach", { id: sessionId });
const reattach = await c.call("session.attach", { id: sessionId, fromSeq: 0 });
assert(reattach.error?.code === "event_gap", `attach past trim → event_gap (got ${reattach.error?.code ?? "no error"})`);
const st3 = await c.call("session.getState", { sessionId });
assert(st3.result.transcript.blocks.some((b) => b.text?.includes(MARKER)), "rebuild source intact after spool trim");

// tool blocks appear when the transcript has tool calls (M1 session shape):
// this session used no tools, so assert shape only on a tool-bearing session file
// via the unit-level rebuild (covered by web eventStore tests + m8-m1 transcript check).
await c.call("session.delete", { id: sessionId });
console.log("M8 G1+G2: PASS");
c.close();
