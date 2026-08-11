// M9 R6: SPA DOM proof — headless Chrome drives the bridge-profile SPA through
// the vite /__bridge-ws proxy. Asserts the ReplPane renders replayed cells
// (user + model provenance badges, image display, collapsed traceback), then
// types a new cell and Shift+Enter-runs it end to end. Trace: m9-r6.jsonl
import { BridgeClient, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";
import { spawn } from "node:child_process";
import { mkdirSync } from "node:fs";
import WebSocket from "ws";

const RUN = Date.now().toString(36);
const token = authToken();
const VITE_PORT = process.env.VITE_PORT ?? "8788";
const CDP_PORT = 9222;

// ---- seed a session with a rich repl history ---------------------------------
console.log("R6: seed session with user/model/error/image cells");
const seed = new BridgeClient(BRIDGE_URL, token, ORIGIN, `${TRACES_DIR}/m9-r6.jsonl`);
await seed.connect();
const created = await seed.call("session.create", { name: "m9-r6" });
const sid = created.result.sessionId;
await seed.call("session.attach", { id: sid });
for (let i = 0; i < 60; i++) {
	const probe = await seed.call("repl.execute", { sessionId: sid, code: "pass", client_cell_id: `r6-probe-${RUN}` });
	if (probe.result?.accepted) break;
	await new Promise((r) => setTimeout(r, 500));
}
await seed.call("repl.execute", { sessionId: sid, code: "r6v = 'dom-proof'\nprint('r6-user-out')", client_cell_id: `r6-u1-${RUN}` });
await seed.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === `u_r6-u1-${RUN}` && ev.data?.status === "done", 30000, "u1 done");
const pngB64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
await seed.call("repl.execute", {
	sessionId: sid,
	code: `from IPython.display import Image, display\nimport base64\ndisplay(Image(data=base64.b64decode("${pngB64}")))`,
	client_cell_id: `r6-img-${RUN}`,
});
await seed.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === `u_r6-img-${RUN}` && ev.data?.status === "done", 30000, "img done");
await seed.call("repl.execute", { sessionId: sid, code: "raise RuntimeError('r6-traceback')", client_cell_id: `r6-err-${RUN}` });
await seed.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === `u_r6-err-${RUN}` && ev.data?.status === "error", 30000, "err done");
await seed.call("prompt.send", {
	sessionId: sid,
	text: "Use the ipython tool to run exactly this python code (one tool call): print('r6-model-out'). Then reply done.",
	idempotencyKey: `m9-r6-prompt-${RUN}`,
});
const modelQ = await seed.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.provenance === "model" && ev.data?.status === "queued", 120000, "model cell queued");
const modelCellId = modelQ.data.cell_id;
await seed.waitEvent((ev) => ev.event === "repl.cell" && ev.data?.cell_id === modelCellId && ev.data?.status === "done", 120000, "model cell done");
await seed.waitSettled(sid, 180000);
console.log(`  ✓ seeded; model cell ${modelCellId}`);
seed.close();

// ---- vite dev server -----------------------------------------------------------
console.log("R6: start vite dev server");
const vite = spawn("npx", ["vite", "--port", VITE_PORT, "--strictPort"], {
	cwd: new URL("../../../packages/web", import.meta.url).pathname,
	env: { ...process.env },
	stdio: ["ignore", "pipe", "pipe"],
});
let viteUp = false;
vite.stdout.on("data", (d) => { if (String(d).includes("Local:")) viteUp = true; });
vite.stderr.on("data", (d) => process.stderr.write(`[vite] ${d}`));
for (let i = 0; i < 100 && !viteUp; i++) {
	await new Promise((r) => setTimeout(r, 200));
	try { const res = await fetch(`http://127.0.0.1:${VITE_PORT}/`); if (res.ok) { viteUp = true; break; } } catch {}
}
assert(viteUp, "vite dev server up");

// ---- headless chrome + raw CDP ---------------------------------------------------
console.log("R6: launch headless chrome");
mkdirSync("/tmp/m9-chrome", { recursive: true });
const chrome = spawn("google-chrome", [
	"--headless=new", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${CDP_PORT}`,
	"--user-data-dir=/tmp/m9-chrome", "--window-size=1400,900", "about:blank",
], { stdio: ["ignore", "ignore", "pipe"] });
chrome.stderr.on("data", () => {});
let targets = null;
for (let i = 0; i < 50 && !targets; i++) {
	await new Promise((r) => setTimeout(r, 200));
	try { targets = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json(); } catch {}
}
assert(targets, "CDP endpoint up");
const page = targets.find((t) => t.type === "page");
const cdp = new WebSocket(page.webSocketDebuggerUrl, { maxPayload: 64 * 1024 * 1024 });
await new Promise((res, rej) => { cdp.on("open", res); cdp.on("error", rej); });
let cdpId = 0;
const cdpPending = new Map();
cdp.on("message", (d) => {
	const msg = JSON.parse(String(d));
	if (msg.id && cdpPending.has(msg.id)) { cdpPending.get(msg.id)(msg); cdpPending.delete(msg.id); }
});
const cdpCall = (method, params = {}) => new Promise((resolve) => {
	const id = ++cdpId;
	cdpPending.set(id, resolve);
	cdp.send(JSON.stringify({ id, method, params }));
});
const evaluate = async (expr) => {
	const res = await cdpCall("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
	if (res.result?.exceptionDetails) throw new Error(`page eval failed: ${JSON.stringify(res.result.exceptionDetails).slice(0, 300)}`);
	return res.result?.result?.value;
};

const url = `http://127.0.0.1:${VITE_PORT}/?backend=bridge&s=${sid}`;
await cdpCall("Page.enable");
await cdpCall("Page.navigate", { url });
console.log(`  · navigated to ${url}`);

// wait for the repl pane + replayed cells
let cellCount = 0;
for (let i = 0; i < 150; i++) {
	await new Promise((r) => setTimeout(r, 200));
	cellCount = await evaluate(`document.querySelectorAll(".repl-pane .repl-cell").length`).catch(() => 0);
	if (cellCount >= 5) break; // probe + u1 + img + err + model
}
assert(cellCount >= 5, `repl pane rendered >=5 replayed cells (got ${cellCount})`);
console.log(`  ✓ repl pane rendered ${cellCount} cells`);

const provenances = await evaluate(`[...document.querySelectorAll(".repl-pane .repl-cell")].map((c) => c.dataset.provenance)`);
assert(provenances.includes("user"), "user-provenance cells in DOM");
assert(provenances.includes("model"), "model-provenance cell in DOM");
const badges = await evaluate(`[...document.querySelectorAll(".repl-pane .repl-cell span")].map((s) => s.textContent)`);
assert(badges.includes("model"), "'model' badge rendered");
assert(badges.includes("you"), "'you' badge rendered");
const statuses = await evaluate(`[...document.querySelectorAll(".repl-pane .repl-cell")].map((c) => c.dataset.status)`);
assert(statuses.includes("done"), "done status rendered");
assert(statuses.includes("error"), "error status rendered");
assert(await evaluate(`document.querySelectorAll(".repl-pane .repl-cell img").length >= 1`), "png display rendered as <img>");
assert(await evaluate(`[...document.querySelectorAll(".repl-pane details.repl-trace")].length >= 1`), "traceback collapsed in <details>");
assert(await evaluate(`document.body.textContent.includes("r6-user-out")`), "user cell stdout in DOM");
assert(await evaluate(`document.body.textContent.includes("r6-model-out")`), "model cell stdout in DOM");
assert(await evaluate(`document.body.textContent.includes("idle")`), "idle busy-indicator rendered");
console.log("  ✓ replay render: badges, statuses, image, collapsed traceback, outputs");

console.log("R6: type a cell and Shift+Enter");
await evaluate(`(() => {
	const ta = document.querySelector(".repl-pane textarea.repl-input");
	const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, "value").set;
	setter.call(ta, "print('r6-dom-' + r6v)");
	ta.dispatchEvent(new Event("input", { bubbles: true }));
	return ta.value;
})()`);
	await evaluate(`(() => {
	const ta = document.querySelector(".repl-pane textarea.repl-input");
	ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", shiftKey: true, bubbles: true, cancelable: true }));
	return true;
})()`);
let domCell = false;
for (let i = 0; i < 150; i++) {
	await new Promise((r) => setTimeout(r, 200));
	domCell = await evaluate(`document.body.textContent.includes("r6-dom-dom-proof")`).catch(() => false);
	if (domCell) break;
}
assert(domCell, "Shift+Enter cell executed and output 'r6-dom-dom-proof' rendered (shared namespace: r6v resolved)");
console.log("  ✓ interactive cell ran; shared namespace variable resolved");

console.log("R6: pane toggle");
await evaluate(`[...document.querySelectorAll("button")].find((b) => b.title.startsWith("Toggle repl"))?.click()`);
await new Promise((r) => setTimeout(r, 500));
assert(await evaluate(`document.querySelectorAll(".repl-pane").length === 0`), "toggle hides the pane");
await evaluate(`[...document.querySelectorAll("button")].find((b) => b.title.startsWith("Toggle repl"))?.click()`);
await new Promise((r) => setTimeout(r, 700));
assert(await evaluate(`document.querySelectorAll(".repl-pane").length === 1`), "toggle re-shows the pane with state intact");
assert(await evaluate(`document.body.textContent.includes("r6-dom-dom-proof")`), "console state survived toggle");

console.log("R6: PASS");
chrome.kill("SIGKILL");
vite.kill("SIGTERM");
process.exit(0);
