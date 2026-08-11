// M9 R4: idempotent repl.execute replay + typed errors — repl_unavailable,
// replay:true on same client_cell_id, idempotency_conflict on param mismatch,
// session_not_found, bad_request (oversize code), host_down after SIGKILL,
// and HostCrashed on the in-flight cell (spooled, replayable). Trace: m9-r4.jsonl
import { BridgeClient, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken, stopBridge, startBridge } from "./harness.mjs";

const RUN = Date.now().toString(36);
console.log("  · restarting bridge with BRIDGE_RESPAWN_DELAY_MS=20000");
await stopBridge();
await startBridge({ BRIDGE_RESPAWN_DELAY_MS: "20000" });

const c = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, `${TRACES_DIR}/m9-r4.jsonl`);
await c.connect();

console.log("R4: create session");
const created = await c.call("session.create", { name: "m9-r4" });
const sid = created.result.sessionId;
await c.call("session.attach", { id: sid });

console.log("R4: typed error — repl_unavailable before the extension reports ready");
const first = await c.call("repl.execute", { sessionId: sid, code: "pass", client_cell_id: `r4-unavail-${RUN}` });
if (first.error) {
	assert(first.error.code === "repl_unavailable", `first probe refused with repl_unavailable (got ${first.error.code})`);
} else {
	console.log("  · host reported ready before first probe (accepted) — repl_unavailable path covered by R1/R2 probe loops");
}
let ack = first;
for (let i = 0; i < 60 && !ack.result?.accepted; i++) {
	await new Promise((r) => setTimeout(r, 500));
	ack = await c.call("repl.execute", { sessionId: sid, code: "pass", client_cell_id: `r4-unavail-${RUN}` });
}
assert(ack.result?.accepted === true, "capability eventually ready");

console.log("R4: idempotency — same client_cell_id + same params replays");
const key = `r4-idem-${RUN}`;
const a1 = await c.call("repl.execute", { sessionId: sid, code: "r4x = 7\nprint('r4-first')", client_cell_id: key });
assert(a1.result?.accepted === true && a1.result.cell_id === `u_${key}`, "first execute accepted");
await c.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === `u_${key}` && ev.data?.status === "done", 30000, "idem cell done");
const a2 = await c.call("repl.execute", { sessionId: sid, code: "r4x = 7\nprint('r4-first')", client_cell_id: key });
assert(a2.result?.accepted === true && a2.result.cell_id === `u_${key}` && a2.result.replay === true, "same params → replay:true, same cell_id");
const queuedCount = c.events.filter((ev) => ev.event === "repl.cell" && ev.data?.cell_id === `u_${key}` && ev.data?.status === "queued").length;
assert(queuedCount === 1, `exactly one queued event for the idempotent cell (got ${queuedCount})`);

console.log("R4: typed error — idempotency_conflict on param mismatch");
const a3 = await c.call("repl.execute", { sessionId: sid, code: "print('different')", client_cell_id: key });
assert(a3.error?.code === "idempotency_conflict", `mismatched params → idempotency_conflict (got ${a3.error?.code ?? a3.result})`);

console.log("R4: typed error — session_not_found");
const nf = await c.call("repl.execute", { sessionId: "no-such-session", code: "pass", client_cell_id: `r4-nf-${RUN}` });
assert(nf.error?.code === "session_not_found", `unknown session → session_not_found (got ${nf.error?.code})`);

console.log("R4: typed error — bad_request on oversize code");
const big = await c.call("repl.execute", { sessionId: sid, code: "x" + "1".repeat(1024 * 1024), client_cell_id: `r4-big-${RUN}` });
assert(big.error?.code === "bad_request", `oversize code → bad_request (got ${big.error?.code})`);

console.log("R4: host_down — SIGKILL the host mid-cell");
const st = await c.call("session.getState", { sessionId: sid });
const hostPid = st.result.hostPid;
assert(hostPid > 0, "host pid captured");
const long = await c.call("repl.execute", { sessionId: sid, code: "import time; time.sleep(60)", client_cell_id: `r4-long-${RUN}` });
assert(long.result?.accepted === true, "long cell accepted");
await c.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === `u_r4-long-${RUN}` && ev.data?.status === "running", 30000, "long cell running");
process.kill(hostPid, "SIGKILL");
console.log(`  · killed host pid ${hostPid}`);
await c.waitEvent((ev) => ev.sessionId === sid && ev.event === "session.state" && ev.data?.state === "host_exited", 20000, "host_exited");
const crashed = await c.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === `u_r4-long-${RUN}` && ev.data?.status === "error", 20000, "HostCrashed cell event");
assert(crashed.data.error?.ename === "HostCrashed", `in-flight cell errored HostCrashed (got ${crashed.data.error?.ename})`);

const down = await c.call("repl.execute", { sessionId: sid, code: "pass", client_cell_id: `r4-down-${RUN}` });
assert(down.error?.code === "host_down", `repl.execute while down → host_down (got ${down.error?.code ?? JSON.stringify(down.result)})`);
assert(down.error?.data?.state === "host_down", `host_down carries handle-state detail (got ${down.error?.data?.state})`);

console.log("R4: recovery — respawn after 20s, kernel restarted (namespace reset)");
await c.waitEvent((ev) => ev.sessionId === sid && ev.event === "session.state" && ev.data?.state === "host_respawned", 120000, "host_respawned");
let back = null;
for (let i = 0; i < 60; i++) {
	back = await c.call("repl.execute", { sessionId: sid, code: "print(r4x)", client_cell_id: `r4-back-${RUN}` });
	if (back.result?.accepted) break;
	await new Promise((r) => setTimeout(r, 500));
}
assert(back.result?.accepted === true, "repl.execute accepted after respawn");
const nameErr = await c.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === `u_r4-back-${RUN}` && ev.data?.status === "error", 60000, "post-respawn cell error");
assert(nameErr.data.error?.ename === "NameError", `fresh kernel: r4x undefined → NameError (got ${nameErr.data.error?.ename})`);

console.log("R4: PASS");
await c.call("session.delete", { sessionId: sid }).catch(() => {});
c.close();

console.log("  · restoring bridge with default env");
await stopBridge();
await startBridge();
process.exit(0);
