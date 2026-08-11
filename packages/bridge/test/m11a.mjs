// M11a verification (contract v0.2):
//   Part A (dogfood :8731, OAuth rig) — V2 session.setModel / setThinkingLevel
//     with a real turn + typed errors, V3 session.create{model}.
//   Part B (test :8730, GLM rig) — V4 subagent.transcript drill-down
//     (running + completed rlm child), V5 comms.list + comms.message events.
// Traces: m11a-v2v3-dogfood.jsonl, m11a-v4v5-comms.jsonl
import { BridgeClient, assert, collectText } from "./client.mjs";
import { ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";
import { readFileSync } from "node:fs";

const P = (ok, name) => console.log(`${ok ? "✓" : "✗ FAIL"} ${name}`);
const RUN = Date.now().toString(36);

// ---------------------------------------------------------------- Part A
console.log("===== Part A: model surface on dogfood (:8731) =====");
const dogToken = readFileSync(new URL("../../../.pi/dogfood/bridge-token", import.meta.url).pathname, "utf8").trim();
const dog = new BridgeClient("ws://127.0.0.1:8731", dogToken, ORIGIN, `${TRACES_DIR}/m11a-v2v3-dogfood.jsonl`);
await dog.connect();

// V3: session.create{model} — applied before the row is visible, returned in result
console.log("V3: session.create with model=openai-codex/gpt-5.6-luna");
const createdA = await dog.call("session.create", { name: `m11a-a-${RUN}`, model: "openai-codex/gpt-5.6-luna", idempotencyKey: `m11a-a-${RUN}` });
assert(!createdA.error, `create failed: ${JSON.stringify(createdA.error)}`);
const sidA = createdA.result.sessionId;
P(createdA.result.model?.provider === "openai-codex" && createdA.result.model?.modelId === "gpt-5.6-luna", "create result carries the applied model");
await dog.call("session.attach", { id: sidA });
const stA0 = (await dog.call("session.getState", { sessionId: sidA })).result;
P(stA0.live?.model === "openai-codex/gpt-5.6-luna", `getState live.model = ${stA0.live?.model}`);
P(typeof stA0.live?.thinkingLevel === "string", `getState live.thinkingLevel = ${stA0.live?.thinkingLevel}`);

// V3b: create with an INVALID model → typed error, no orphan row
const badCreate = await dog.call("session.create", { name: `m11a-bad-${RUN}`, model: "openai-codex/nope-9000", idempotencyKey: `m11a-bad-${RUN}` });
P(badCreate.error?.code === "model_not_found", `create invalid model → model_not_found (got ${badCreate.error?.code})`);
const listAfterBad = (await dog.call("session.list", {})).result.sessions;
P(!listAfterBad.some((s) => s.name === `m11a-bad-${RUN}`), "no orphan session row after failed create");

// V2: setModel passthrough + session.model event + getState reconcile
console.log("V2: setModel luna → sol");
const setP = dog.waitEvent((ev) => ev.event === "session.model" && ev.sessionId === sidA && ev.data?.modelId === "gpt-5.6-sol", 30000, "session.model sol");
const setRes = await dog.call("session.setModel", { sessionId: sidA, provider: "openai-codex", modelId: "gpt-5.6-sol" });
assert(!setRes.error, `setModel failed: ${JSON.stringify(setRes.error)}`);
P(setRes.result.provider === "openai-codex" && setRes.result.modelId === "gpt-5.6-sol", "setModel result carries provider+modelId");
await setP;
P(true, "session.model event emitted on setModel");
const stA1 = (await dog.call("session.getState", { sessionId: sidA })).result;
P(stA1.live?.model === "openai-codex/gpt-5.6-sol", `getState reconciles to sol (got ${stA1.live?.model})`);

// V2b: setThinkingLevel + session.model event
const tlP = dog.waitEvent((ev) => ev.event === "session.model" && ev.sessionId === sidA && ev.data?.thinkingLevel === "xhigh", 30000, "session.model xhigh");
const tlRes = await dog.call("session.setThinkingLevel", { sessionId: sidA, level: "xhigh" });
P(!tlRes.error && tlRes.result.thinkingLevel === "xhigh", `setThinkingLevel → xhigh (got ${JSON.stringify(tlRes.result ?? tlRes.error)})`);
await tlP;
P(true, "session.model event emitted on setThinkingLevel");
P(Array.isArray(tlRes.result.availableLevels) && tlRes.result.availableLevels.includes("xhigh"), "availableLevels returned");

// V2c: typed errors
const notFound = await dog.call("session.setModel", { sessionId: sidA, provider: "openai-codex", modelId: "nope-9000" });
P(notFound.error?.code === "model_not_found", `setModel unknown id → model_not_found (got ${notFound.error?.code})`);
// Upstream truth (rpc-mode set_model): models from providers without auth are
// NOT in the available snapshot, so the gate rejects them with "Model not
// found" BEFORE auth is consulted. models.list's available:false/authConfigured
// flags are the UI signal; model_unavailable covers the post-gate checkAuth
// failure ("No API key for …", e.g. an auth race on a listed provider).
const noAuth = await dog.call("session.setModel", { sessionId: sidA, provider: "openai", modelId: "gpt-5" });
P(noAuth.error?.code === "model_not_found", `setModel unauthed provider → model_not_found at the snapshot gate (got ${noAuth.error?.code})`);
const badLevel = await dog.call("session.setThinkingLevel", { sessionId: sidA, level: "bogus" });
P(badLevel.error?.code === "bad_request", `setThinkingLevel bogus → bad_request (got ${badLevel.error?.code})`);

// V2d: real turn on the switched model
console.log("V2: real turn on gpt-5.6-sol");
const headA = dog.headSeq(sidA);
await dog.call("prompt.send", { sessionId: sidA, idempotencyKey: `m11a-p-${RUN}`, text: "Reply with exactly: M11A-OK" });
await dog.waitIdleAfter(sidA, headA, 180000);
const txtA = collectText(dog.events, sidA);
P(txtA.includes("M11A-OK"), "real turn completed on the switched model");
const stA2 = (await dog.call("session.getState", { sessionId: sidA })).result;
P(stA2.live?.model === "openai-codex/gpt-5.6-sol", "model persisted across the turn");

// V2e: model choice persisted to the session file (model_change entry)
const sfA = stA2.sessionFile;
assert(sfA, "sessionFile known");
const linesA = readFileSync(sfA, "utf8").trim().split("\n").map((l) => JSON.parse(l));
const mc = [...linesA].reverse().find((e) => e.type === "model_change");
P(mc?.modelId === "gpt-5.6-sol" && mc?.provider === "openai-codex", `session file model_change = ${mc?.provider}/${mc?.modelId}`);
const tlc = [...linesA].reverse().find((e) => e.type === "thinking_level_change");
P(tlc?.thinkingLevel === "xhigh", `session file thinking_level_change = ${tlc?.thinkingLevel}`);
dog.close();

// ---------------------------------------------------------------- Part B
console.log("===== Part B: subagent drill-down + comms on GLM (:8730) =====");
const c = new BridgeClient("ws://127.0.0.1:8730", authToken(), ORIGIN, `${TRACES_DIR}/m11a-v4v5-comms.jsonl`);
await c.connect();
const createdB = await c.call("session.create", { name: `m11a-b-${RUN}`, idempotencyKey: `m11a-b-${RUN}` });
assert(!createdB.error, `create B failed: ${JSON.stringify(createdB.error)}`);
const sidB = createdB.result.sessionId;
await c.call("session.attach", { id: sidB });

console.log("V4/V5: GLM spawns one rlm child that computes + messages back");
const headB = c.headSeq(sidB);
await c.call("prompt.send", { sessionId: sidB, idempotencyKey: `m11a-pb-${RUN}`, text: [
	"Spawn one subagent with `await rlm(\"In your IPython kernel compute 17*23, then send the result to your parent: await agent_message.send('child-result:391', receiver_role='parent'), then finish.\")`. It returns a handle immediately at admission; note the child's name.",
	"Then immediately send that child a message: `await agent_message.send('PING-73', receiver_name=<the child name>)`.",
	"End your turn with: SPAWNED. When the child's reply arrives later, reply with: CHILD-SAID + what it sent.",
].join("\n") });

// V4a: child admitted → tree shows it → transcript while RUNNING (retry through subagent_not_found)
const adm = await c.waitEvent((ev) => ev.event === "subagent.lifecycle" && ev.sessionId === sidB && ev.data?.phase === "admitted", 240000, "child admitted");
const childId = adm.data.rlm_child_id;
console.log("  · child admitted:", childId);
let trRun = null;
for (let i = 0; i < 30; i++) {
	const r = await c.call("subagent.transcript", { sessionId: sidB, childId });
	if (!r.error) { trRun = r.result; break; }
	assert(r.error.code === "subagent_not_found", `transcript error must be subagent_not_found while admitting (got ${r.error.code})`);
	await new Promise((res) => setTimeout(res, 2000));
}
P(!!trRun, "subagent.transcript resolves for the running child (after admission retries)");
if (trRun) {
	P(typeof trRun.sessionFile === "string" && trRun.sessionFile.includes("sub-"), `child session file resolved: ${trRun.sessionFile.split("/").slice(-2).join("/")}`);
	P(Array.isArray(trRun.blocks) && Array.isArray(trRun.replCells), "blocks + replCells arrays present");
}
const badTr = await c.call("subagent.transcript", { sessionId: sidB, childId: "rlm_00000000" });
P(badTr.error?.code === "subagent_not_found", `unknown childId → subagent_not_found (got ${badTr.error?.code})`);

// V5a: outbound leg — parent sent PING-73 → comms.message event + comms.list record
console.log("V5: comms visibility");
const outEv = await c.waitEvent(
	(ev) => ev.event === "comms.message" && ev.sessionId === sidB && ev.data?.direction !== "in" && typeof ev.data?.message === "string" && ev.data.message.includes("PING-73"),
	240000,
	"outbound comms.message",
);
P(outEv.data?.deliveryStatus === "delivered" || outEv.data?.deliveryStatus === "failed" || outEv.data?.deliveryStatus === "persisted",
	`outbound comms.message terminal status = ${outEv.data?.deliveryStatus}`);
P(outEv.data?.role === "child" && typeof outEv.data?.receiverName === "string", `outbound carries role=child + receiverName (${outEv.data?.receiverName})`);

// V4b: child completes → transcript has assistant text + repl cells
await c.waitEvent((ev) => ev.event === "subagent.lifecycle" && ev.sessionId === sidB && ev.data?.rlm_child_id === childId && ev.data?.phase === "completed", 300000, "child completed");
const trDone = (await c.call("subagent.transcript", { sessionId: sidB, childId })).result;
P(trDone.blocks.some((b) => b.kind === "message" && b.role === "assistant" && b.text.length > 0), "completed child transcript has assistant text");
P(trDone.replCells.length > 0 && trDone.replCells.some((cell) => cell.status === "done"), `completed child has done repl cells (${trDone.replCells.length})`);
const cellText = JSON.stringify(trDone.replCells.map((cell) => [cell.code, cell.outputs.map((o) => o.data)]));
P(cellText.includes("391"), "child repl cell computed 17*23=391");

// V5b: inbound leg — child's reply → comms.message direction:in + turn 2
const inEv = await c.waitEvent((ev) => ev.event === "comms.message" && ev.sessionId === sidB && ev.data?.direction === "in", 240000, "inbound comms.message");
P(typeof inEv.data?.message === "string" && inEv.data.message.includes("child-result:391"), "inbound comms.message carries child content");
P(inEv.data?.fromName != null || inEv.data?.fromSessionId != null, "inbound comms.message identifies the sender");
// anchor on the inbound comms event's seq: the turn-2 idle may have ALREADY
// been delivered (child completion and the parent's reply race); waitEvent
// scans already-received events first, so a past idle still matches.
await c.waitIdleAfter(sidB, inEv.seq, 300000);
const txtB = collectText(c.events, sidB);
P(txtB.includes("CHILD-SAID"), "parent processed the child message (turn 2)");

// V5c: comms.list — outbound + inbound folded, chronological
const comms = (await c.call("comms.list", { sessionId: sidB })).result;
P(comms.messages.length >= 2, `comms.list has ${comms.messages.length} messages (>=2)`);
const outbound = comms.messages.filter((m) => m.direction === "out");
const inbound = comms.messages.filter((m) => m.direction === "in");
P(outbound.length >= 1 && outbound.some((m) => m.content.includes("PING-73")), "comms.list outbound PING-73 present");
P(outbound.every((m) => ["delivered", "failed", "persisted"].includes(m.deliveryStatus)), `outbound terminal statuses: ${outbound.map((m) => m.deliveryStatus).join(",")}`);
P(inbound.length >= 1 && inbound.some((m) => m.content.includes("child-result:391") && m.deliveryStatus === "delivered"), "comms.list inbound child-result delivered");
const ts = comms.messages.map((m) => m.ts);
P(ts.every((v, i) => i === 0 || v >= ts[i - 1]), "comms.list sorted by ts");

// V5d: comms.message events replay on fresh attach (spool durability)
const c2 = new BridgeClient("ws://127.0.0.1:8730", authToken(), ORIGIN, `${TRACES_DIR}/m11a-v4v5-replay.jsonl`);
await c2.connect();
await c2.call("session.attach", { id: sidB, fromSeq: 0 });
await new Promise((res) => setTimeout(res, 2000));
const replayed = c2.events.filter((ev) => ev.event === "comms.message" && ev.sessionId === sidB);
P(replayed.length >= 2, `comms.message events replay on fresh attach (${replayed.length})`);
c2.close();
c.close();
console.log("M11A VERIFY DONE");
process.exit(0);
