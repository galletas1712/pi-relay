// M11b verification (contract v0): fork/switch/getForkPoints/getEntries/
// commands/compact over WS.
//   Part A (test rig :8730, GLM): real turn → fork points carry
//     parentId/timestamp/onActiveBranch (file-parse path) → session.getEntries
//     full text → /fork (branch-file surgery + workspace snapshot when managed)
//     → divergent writes land in the right branch files → /switch at a
//     user-message boundary → getCommands → compact.
//   Part B (dogfood :8731): read-only — getForkPoints/getEntries/getCommands on
//     the long-lived dogfood session file.
// Trace: m11b-contract.jsonl
import { BridgeClient, assert, collectText } from "./client.mjs";
import { ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const P = (ok, name) => console.log(`${ok ? "✓" : "✗ FAIL"} ${name}`);

/** waitEvent scans buffered events too, so gate on a seq watermark captured
 * right before the prompt that starts the turn. */
const seqWatermark = (client, sid) => Math.max(0, ...client.events.filter((e) => e.sessionId === sid).map((e) => e.seq ?? 0));
const waitIdleAfter = (client, sid, minSeq, what) =>
	client.waitEvent((ev) => ev.event === "session.state" && ev.sessionId === sid && ev.data?.state === "idle" && (ev.seq ?? 0) > minSeq, 120000, what);
const RUN = Date.now().toString(36);
let failures = 0;
const check = (ok, name) => { P(ok, name); if (!ok) failures += 1; };

// ---------------------------------------------------------------- Part A
console.log("===== Part A: fork/switch on test rig (:8730, GLM) =====");
const test = new BridgeClient("ws://127.0.0.1:8730", authToken(), ORIGIN, `${TRACES_DIR}/m11b-contract.jsonl`);
await test.connect();

const chosenId = `m11b-${RUN}`;
const created = await test.call("session.create", {
	sessionId: chosenId,
	name: `m11b-a-${RUN}`,
	cwd: "/tmp",
	idempotencyKey: `m11b-a-${RUN}`,
});
check(!created.error, `create failed: ${JSON.stringify(created.error)}`);
check(created.result?.sessionId === chosenId, `client-chosen sessionId honored (${created.result?.sessionId})`);
const sid = created.result.sessionId;
await test.call("session.attach", { id: sid });

// A turn with a tool call so transcript has user/assistant/tool blocks
const wm1 = seqWatermark(test, sid);
const done1 = waitIdleAfter(test, sid, wm1, "turn 1");
const s1 = await test.call("prompt.send", { sessionId: sid, text: "Reply with exactly: ALPHA", idempotencyKey: `m11b-p1-${RUN}` });
check(!s1.error, `prompt 1 failed: ${JSON.stringify(s1.error)}`);
await done1;
P(true, "turn 1 completed");

const pts1 = (await test.call("session.getForkPoints", { sessionId: sid })).result;
check(Array.isArray(pts1.points) && pts1.points.length >= 1, `getForkPoints returns ≥1 point (${pts1.points?.length})`);
const pt = pts1.points.find((p) => p.preview.includes("ALPHA"));
check(!!pt, "fork point for the ALPHA user message exists");
check(typeof pt.parentId === "string", `fork point carries parentId (file-parse path): ${pt.parentId?.slice(0, 8)}`);
check(typeof pt.timestamp === "string", `fork point carries timestamp: ${pt.timestamp}`);
check(pt.onActiveBranch === true, "fork point marked onActiveBranch");

// session.getEntries: full text, arbitrary ids
const ge = (await test.call("session.getEntries", { sessionId: sid, ids: [pt.entryId] })).result;
check(ge.entries?.length === 1 && ge.entries[0].text.includes("ALPHA"), `getEntries full text: "${ge.entries?.[0]?.text?.slice(0, 40)}"`);
check(ge.entries[0].role === "user", "getEntries role=user");

// second turn, then fork at current state (entryId omitted)
const wm2 = seqWatermark(test, sid);
const done2 = waitIdleAfter(test, sid, wm2, "turn 2");
await test.call("prompt.send", { sessionId: sid, text: "Reply with exactly: BETA", idempotencyKey: `m11b-p2-${RUN}` });
await done2;
P(true, "turn 2 completed");

const fork = await test.call("session.fork", { sessionId: sid, idempotencyKey: `m11b-fork-${RUN}` });
check(!fork.error, `fork failed: ${JSON.stringify(fork.error)}`);
const childId = fork.result?.childId ?? fork.result?.newSessionId;
check(typeof childId === "string" && childId !== sid, `fork returned childId ${String(childId).slice(0, 8)}`);
P(true, `workspace snapshot info: ${JSON.stringify(fork.result?.workspaceSnapshot ?? null)}`);

// idempotent replay
const forkReplay = await test.call("session.fork", { sessionId: sid, idempotencyKey: `m11b-fork-${RUN}` });
check((forkReplay.result?.childId ?? forkReplay.result?.newSessionId) === childId, "fork replay returns the same child");

// child state: parentSessionId set, transcript copied
const childState = (await test.call("session.getState", { sessionId: childId })).result;
check(childState.parentSessionId === sid, "child parentSessionId = parent");
const childBlocks = childState.transcript?.blocks ?? [];
check(childBlocks.some((b) => b.kind === "message" && b.role === "user" && b.text.includes("BETA")), "child transcript includes both turns");

// divergent writes: parent turn 3 lands only in parent's active branch file
const wm3 = seqWatermark(test, sid);
const done3 = waitIdleAfter(test, sid, wm3, "turn 3");
await test.call("prompt.send", { sessionId: sid, text: "Reply with exactly: DELTA", idempotencyKey: `m11b-p3-${RUN}` });
await done3;
await test.call("session.attach", { id: childId });
const wmC = seqWatermark(test, childId);
const doneC = waitIdleAfter(test, childId, wmC, "child turn");
await test.call("prompt.send", { sessionId: childId, text: "Reply with exactly: EPSILON", idempotencyKey: `m11b-pc-${RUN}` });
await doneC;
P(true, "divergent turns completed on both branches");

// branch files: parent active file lacks EPSILON, child file lacks DELTA
const dataDir = new URL("../data/sessions", import.meta.url).pathname;
const files = readdirSync(dataDir).filter((f) => f.includes(sid) || f.includes(childId));
P(true, `session files: ${files.join(", ") || "(none?)"}`);
const parentMain = files.filter((f) => f.includes(sid) && !f.includes(".")).concat(files.filter((f) => f.startsWith(sid)));
const readIf = (name) => { try { return readFileSync(join(dataDir, name), "utf8"); } catch { return ""; } };
const parentText = files.filter((f) => f.includes(sid)).map(readIf).join("\n");
const childText = files.filter((f) => f.includes(childId)).map(readIf).join("\n");
check(parentText.includes("DELTA"), "parent branch contains DELTA");
check(!parentText.includes("EPSILON"), "parent branch files exclude EPSILON");
check(childText.includes("EPSILON"), "child branch contains EPSILON");
check(!childText.includes("DELTA"), "child branch excludes DELTA");

// /switch: back to the ALPHA point (parentId boundary) on the PARENT session
const sw = await test.call("session.switch", { sessionId: sid, entryId: pt.entryId, idempotencyKey: `m11b-sw-${RUN}` });
check(!sw.error, `switch failed: ${JSON.stringify(sw.error)}`);
// boundary semantics: switching to the ALPHA point makes ALPHA's parentId the
// leaf — the ALPHA user message itself leaves the active view (the composer
// prefill carries it via getEntries / switch result selectedText)
check(sw.result?.selectedText?.includes("ALPHA"), `switch result carries selectedText for composer prefill: "${sw.result?.selectedText?.slice(0, 40)}"`);
const stSw = (await test.call("session.getState", { sessionId: sid })).result;
const swText = JSON.stringify(stSw.transcript?.blocks ?? []);
check(!swText.includes("ALPHA"), "post-switch active transcript excludes the boundary message itself");
check(!swText.includes("DELTA"), "post-switch transcript excludes DELTA (switched away)");
// restore-text path: off-branch entries still readable via getEntries
const geOff = (await test.call("session.getEntries", { sessionId: sid, ids: [pt.entryId] })).result;
check(geOff.entries?.[0]?.text.includes("ALPHA"), "getEntries reads off-branch entries after switch");
const ptsAfter = (await test.call("session.getForkPoints", { sessionId: sid })).result;
const deltaPt = ptsAfter.points.find((p) => p.preview.includes("DELTA"));
check(!!deltaPt && deltaPt.onActiveBranch === false, "DELTA point listed, off active branch");
const alphaPt = ptsAfter.points.find((p) => p.preview.includes("ALPHA"));
check(!!alphaPt && alphaPt.onActiveBranch === false, "ALPHA point listed (boundary sits past the leaf)");
// switch forward again to DELTA
const sw2 = await test.call("session.switch", { sessionId: sid, entryId: deltaPt.entryId, idempotencyKey: `m11b-sw2-${RUN}` });
check(!sw2.error, `switch-forward failed: ${JSON.stringify(sw2.error)}`);
const stSw2 = (await test.call("session.getState", { sessionId: sid })).result;
const sw2Text = JSON.stringify(stSw2.transcript?.blocks ?? []);
check(sw2Text.includes("BETA"), "switched forward: BETA branch visible again");
check(!sw2Text.includes("DELTA"), "switched forward: DELTA boundary past the leaf");

// getCommands + compact
const cmds = (await test.call("session.commands", { sessionId: sid })).result;
check(Array.isArray(cmds.commands) && cmds.commands.length > 0, `getCommands → ${cmds.commands?.length} commands`);
// compact passthrough: a 4-message session is legitimately "too small" for
// pi — the contract obligation is the typed host_error, not a forced
// compaction. (Real compaction rendering is verified in the V2 UI flow.)
const cmp = await test.call("session.compact", { sessionId: sid });
if (cmp.error) {
	check(cmp.error.code === "host_error" && String(cmp.error.message).includes("Nothing to compact"), `compact typed refusal: ${cmp.error.code}: ${cmp.error.message}`);
} else {
	await new Promise((r) => setTimeout(r, 1500));
	const stCmp = (await test.call("session.getState", { sessionId: sid })).result;
	check((stCmp.transcript?.blocks ?? []).some((b) => b.kind === "compaction"), "compaction block in transcript after compact");
}

await test.close();

// ---------------------------------------------------------------- Part B
console.log("===== Part B: read-only on dogfood (:8731) =====");
const dogToken = readFileSync(new URL("../../../.pi/dogfood/bridge-token", import.meta.url).pathname, "utf8").trim();
const dog = new BridgeClient("ws://127.0.0.1:8731", dogToken, ORIGIN, `${TRACES_DIR}/m11b-contract.jsonl`);
await dog.connect();
const sessions = (await dog.call("session.list", {})).result.sessions;
// pick the richest session: most user-message boundaries across branch files
let dogfood = null, dpts = null;
for (const s of sessions) {
	const pts = (await dog.call("session.getForkPoints", { sessionId: s.sessionId })).result;
	if (!dpts || pts.points.length > dpts.points.length) { dogfood = s; dpts = pts; }
}
P(true, `dogfood session: ${dogfood.name} (${dogfood.sessionId.slice(0, 8)})`);
check(dpts.points.length >= 1, `dogfood fork points: ${dpts.points.length} (dogfood sessions are single-prompt tool runs)`);
check(dpts.points.every((p) => typeof p.parentId === "string" && typeof p.timestamp === "string"), "all dogfood points carry parentId+timestamp");
check(dpts.points.some((p) => p.onActiveBranch === true), "some dogfood points on active branch");
const dgeIds = dpts.points.slice(0, 3).map((p) => p.entryId);
const dge = (await dog.call("session.getEntries", { sessionId: dogfood.sessionId, ids: dgeIds })).result;
check(dge.entries.length === dgeIds.length && dge.entries.every((e) => e.text.length > 0), `dogfood getEntries x${dgeIds.length}: "${dge.entries[0]?.text.slice(0, 40)}…"`);
const dcmds = (await dog.call("session.commands", { sessionId: dogfood.sessionId })).result;
check(Array.isArray(dcmds.commands) && dcmds.commands.length > 0, `dogfood getCommands → ${dcmds.commands?.length}`);
await dog.close();

console.log(failures === 0 ? "\nALL M11B CONTRACT CHECKS PASSED" : `\n${failures} FAILURES`);
process.exit(failures === 0 ? 0 : 1);
