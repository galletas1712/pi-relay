// B5 — two concurrent sessions: isolated kernels, isolated session files,
// independent event streams over one multiplexed WS connection.
import { BridgeClient, collectText, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";
import { join } from "node:path";

const trace = join(TRACES_DIR, "m5-b5.jsonl");
const c = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, trace);
await c.connect();

const a = (await c.call("session.create", { name: "b5-a" })).result.sessionId;
const b = (await c.call("session.create", { name: "b5-b" })).result.sessionId;
assert(a !== b, "distinct session ids");
await c.call("session.attach", { id: a, fromSeq: 0 });
await c.call("session.attach", { id: b, fromSeq: 0 });

// set a distinct kernel variable in each, concurrently
const headA = c.headSeq(a);
const headB = c.headSeq(b);
await c.call("prompt.send", {
	sessionId: a,
	text: 'Use the ipython tool. Run exactly one cell: alpha_b5 = "A-ONLY-MARKER"; print("B5-A-SET"). Then reply with exactly B5-A-DONE.',
});
await c.call("prompt.send", {
	sessionId: b,
	text: 'Use the ipython tool. Run exactly one cell: beta_b5 = "B-ONLY-MARKER"; print("B5-B-SET"). Then reply with exactly B5-B-DONE.',
});
await Promise.all([c.waitIdleAfter(a, headA, 300000), c.waitIdleAfter(b, headB, 300000)]);
const textA = collectText(c.events, a);
const textB = collectText(c.events, b);
assert(textA.includes("B5-A-DONE") && !textB.includes("B5-A-DONE"), "session A completed its own turn");
assert(textB.includes("B5-B-DONE") && !textA.includes("B5-B-DONE"), "session B completed its own turn");

// cross-read: A sees alpha but NOT beta; B sees beta but NOT alpha
const headA2 = c.headSeq(a);
const headB2 = c.headSeq(b);
await c.call("prompt.send", {
	sessionId: a,
	text: [
		"Use the ipython tool. Run exactly one cell with this code:",
		'try: print("B5-ALPHA=" + alpha_b5)',
		"except NameError: print(\"B5-ALPHA-MISSING\")",
		'try: print("B5-BETA=" + beta_b5)',
		"except NameError: print(\"B5-BETA-MISSING\")",
		"Then reply with exactly B5-A-CHECKED.",
	].join("\n"),
});
await c.call("prompt.send", {
	sessionId: b,
	text: [
		"Use the ipython tool. Run exactly one cell with this code:",
		'try: print("B5-ALPHA=" + alpha_b5)',
		"except NameError: print(\"B5-ALPHA-MISSING\")",
		'try: print("B5-BETA=" + beta_b5)',
		"except NameError: print(\"B5-BETA-MISSING\")",
		"Then reply with exactly B5-B-CHECKED.",
	].join("\n"),
});
await Promise.all([c.waitIdleAfter(a, headA2, 300000), c.waitIdleAfter(b, headB2, 300000)]);

const evA = JSON.stringify(c.events.filter((e) => e.sessionId === a));
const evB = JSON.stringify(c.events.filter((e) => e.sessionId === b));
assert(evA.includes("B5-ALPHA=A-ONLY-MARKER"), "A kernel has alpha");
assert(evA.includes("B5-BETA-MISSING") && !evA.includes("B5-BETA=B-ONLY-MARKER"), "A kernel does NOT have beta");
assert(evB.includes("B5-BETA=B-ONLY-MARKER"), "B kernel has beta");
assert(evB.includes("B5-ALPHA-MISSING") && !evB.includes("B5-ALPHA=A-ONLY-MARKER"), "B kernel does NOT have alpha");

// independent event seqs + separate session files
const stA = (await c.call("session.getState", { sessionId: a })).result;
const stB = (await c.call("session.getState", { sessionId: b })).result;
assert(stA.sessionFile !== stB.sessionFile, "distinct session files");
assert(stA.hostPid !== stB.hostPid, "distinct host processes");
assert(stA.live.messageCount >= 4 && stB.live.messageCount >= 4, "each transcript holds its own turns");
c.close();
console.log("B5 PASS");
process.exit(0);
