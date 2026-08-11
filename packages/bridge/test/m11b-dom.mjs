// M11b V2: real-model UI flows through the LEGACY app on the bridge profile.
//   Part A (test rig :8788, GLM): seeded project session (managed git
//     workspace on btrfs) — transcript render (plain chat flow, tool behind
//     legacy expandable, no generic cards), model picker filtered to
//     authenticated-only at the adapter, /fork from the composer (REAL btrfs
//     snapshot), /switch at a user-message boundary (picker dialog, rewind,
//     composer prefill), REPL rail tab round-trip.
//   Part B (dogfood :8789, gpt-5.6-sol): rich legacy session renders comms as
//     orange expandables + collapsed tool runs + SUBAGENTS rail navigation;
//     M11c: the See-more threshold counts TEXT-BEARING agent messages (the
//     rich turn's 62 tool-call-only steps no longer inflate it → no toggle,
//     full flow inline); the REAL owner dogfood session (the empty-bubble
//     report) must render no empty assistant bubbles; fresh session: live
//     chat turn with tool collapse; model picker lists exactly the
//     authenticated providers (codex + nvidia-inference, no anthropic) and a
//     live model switch round-trips.
// Traces: m11b-dom-test.jsonl / m11b-dom-dogfood.jsonl; M11c DOM assertions:
// m11c-bubbles-test.jsonl / m11c-bubbles-dogfood.jsonl
import { BridgeClient, assert } from "./client.mjs";
import { ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";
import { readFileSync, existsSync, mkdirSync, writeFileSync, appendFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { spawn } from "node:child_process";
import WebSocket from "ws";

const P = (ok, name) => { console.log(`${ok ? "✓" : "✗ FAIL"} ${name}`); if (!ok) failures += 1; };
let failures = 0;
// M11c: DOM assertion trace (one JSONL line per empty-bubble check).
let m11cTrace = null;
function traceM11c(kind, payload) {
	if (!m11cTrace) return;
	appendFileSync(m11cTrace, JSON.stringify({ t: new Date().toISOString(), kind, payload }) + "\n");
}
const RUN = Date.now().toString(36);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- chrome/CDP scaffold (from m9-r6) ---------------------------------------
let cdp, cdpId = 0;
const cdpPending = new Map();
let chrome = null;
async function launchChrome(tag) {
	const dir = `/tmp/m11b-chrome-${tag}`;
	mkdirSync(dir, { recursive: true });
	const port = tag === "test" ? 9223 : 9224;
	chrome = spawn("google-chrome", [
		"--headless=new", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${port}`,
		`--user-data-dir=${dir}`, "--window-size=1400,900", "about:blank",
	], { stdio: ["ignore", "ignore", "ignore"] });
	let targets = null;
	for (let i = 0; i < 60 && !targets; i++) {
		await sleep(250);
		try { targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json(); } catch {}
	}
	assert(targets, "CDP endpoint up");
	const page = targets.find((t) => t.type === "page");
	cdp = new WebSocket(page.webSocketDebuggerUrl, { maxPayload: 64 * 1024 * 1024 });
	await new Promise((res, rej) => { cdp.on("open", res); cdp.on("error", rej); });
	cdp.on("message", (d) => {
		const msg = JSON.parse(String(d));
		if (msg.id && cdpPending.has(msg.id)) { cdpPending.get(msg.id)(msg); cdpPending.delete(msg.id); }
	});
}
const cdpCall = (method, params = {}) => new Promise((resolve) => {
	const id = ++cdpId;
	cdpPending.set(id, resolve);
	cdp.send(JSON.stringify({ id, method, params }));
});
const evaluate = async (expr) => {
	const res = await cdpCall("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
	if (res.result?.exceptionDetails) throw new Error(`page eval failed: ${JSON.stringify(res.result.exceptionDetails).slice(0, 400)}`);
	return res.result?.result?.value;
};
async function waitFor(what, expr, timeoutMs = 30000) {
	const t0 = Date.now();
	while (Date.now() - t0 < timeoutMs) {
		const v = await evaluate(expr).catch(() => undefined);
		if (v) return v;
		await sleep(300);
	}
	throw new Error(`timeout waiting for ${what}`);
}
async function navigate(url) {
	await cdpCall("Page.enable");
	await cdpCall("Page.navigate", { url });
}
async function clickSessionRow(name) {
	// The session list loads asynchronously — retry find+click until the row
	// appears (single-shot clicks race the list fetch and silently no-op).
	try {
		await waitFor(`session row ${name}`, `(() => {
			const rows = [...document.querySelectorAll(".session-list-items li")];
			const r = rows.find((x) => (x.textContent ?? "").includes(${JSON.stringify(name)}) || (name && (x.textContent ?? "").includes(name)));
			if (!r) return false;
			(r.querySelector("button") ?? r).click();
			return true;
		})()`, 30000);
		// confirm the click actually selected the session (row re-renders can
		// swallow the first click while the list keeps refreshing)
		await waitFor(`session row ${name} selected`, `(() => {
			const rows = [...document.querySelectorAll(".session-list-items li")];
			const r = rows.find((x) => (x.textContent ?? "").includes(${JSON.stringify(name)}) || (name && (x.textContent ?? "").includes(name)));
			if (!r) return false;
			const selected = r.classList.contains("selected") || r.querySelector('[aria-current="page"]') !== null;
			if (!selected) { (r.querySelector("button") ?? r).click(); return false; }
			return true;
		})()`, 30000);
		return true;
	} catch {
		return false;
	}
}
async function clickText(selector, text) {
	return evaluate(`(() => {
		const els = [...document.querySelectorAll(${JSON.stringify(selector)})];
		const el = els.find((e) => (e.textContent ?? "").includes(${JSON.stringify(text)}));
		if (!el) return false;
		el.click();
		return true;
	})()`);
}
async function typeIntoTextarea(selector, text) {
	return evaluate(`(() => {
		const ta = document.querySelector(${JSON.stringify(selector)});
		if (!ta) return false;
		ta.focus();
		const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, "value").set;
		setter.call(ta, ${JSON.stringify(text)});
		ta.dispatchEvent(new Event("input", { bubbles: true }));
		return true;
	})()`);
}
function stopChrome() { try { chrome?.kill("SIGKILL"); } catch {} chrome = null; }

const seqWatermark = (client, sid) => Math.max(0, ...client.events.filter((e) => e.sessionId === sid).map((e) => e.seq ?? 0));
const waitIdleAfter = (client, sid, minSeq, what) =>
	client.waitEvent((ev) => ev.event === "session.state" && ev.sessionId === sid && ev.data?.state === "idle" && (ev.seq ?? 0) > minSeq, 180000, what);

// ============================================================== Part A (test rig)
console.log("===== Part A: legacy UI on test rig (:8788, GLM) =====");
m11cTrace = `${TRACES_DIR}/m11c-bubbles-test.jsonl`;
writeFileSync(m11cTrace, "");
const test = new BridgeClient("ws://127.0.0.1:8730", authToken(), ORIGIN, `${TRACES_DIR}/m11b-dom-test.jsonl`);
await test.connect();

// scratch git remote (repo-local scratch, btrfs volume) for a managed workspace
const scratch = `/home/schwinns/pi-relay/.pi/m1-demo/scratch/m11b-repo-${RUN}`;
execFileSync("git", ["init", "-q", "-b", "main", scratch]);
writeFileSync(`${scratch}/README.md`, `m11b dom ${RUN}\n`);
execFileSync("git", ["-C", scratch, "add", "."]);
execFileSync("git", ["-C", scratch, "-c", "user.email=m11b@test", "-c", "user.name=m11b", "-c", "commit.gpgsign=false", "commit", "-qm", "init"]);

const proj = await test.call("project.create", {
	name: `m11b-dom-${RUN}`,
	workspaces: [{ kind: "git", workspaceDir: "m11b-repo", remoteUrl: `file://${scratch}`, remoteBranch: "main" }],
	idempotencyKey: `m11b-dom-proj-${RUN}`,
});
assert(!proj.error, `project.create failed: ${JSON.stringify(proj.error)}`);
const projectId = proj.result.projectId ?? proj.result.id;
console.log(`  · project ${projectId} (managed git workspace from file:// scratch)`);

const createdA = await test.call("session.create", { projectId, name: `m11b-dom-${RUN}`, idempotencyKey: `m11b-dom-a-${RUN}` });
assert(!createdA.error, `session.create failed: ${JSON.stringify(createdA.error)}`);
const sidA = createdA.result.sessionId;
await test.call("session.attach", { id: sidA });
const wl = (await test.call("workspace.list", { sessionId: sidA })).result;
P(wl.managed === true && wl.btrfs === true, `workspace managed+btrfs (managed=${wl.managed} btrfs=${wl.btrfs})`);

let wm = seqWatermark(test, sidA);
let idle1 = waitIdleAfter(test, sidA, wm, "turn DOMALPHA");
await test.call("prompt.send", { sessionId: sidA, text: "Reply with exactly: DOMALPHA", idempotencyKey: `m11b-dom-p1-${RUN}` });
await idle1;
wm = seqWatermark(test, sidA);
let idle2 = waitIdleAfter(test, sidA, wm, "turn DOMTOOL");
await test.call("prompt.send", { sessionId: sidA, text: "Use the ipython tool to run print('DOMTOOL'), then reply with exactly: DOMDONE", idempotencyKey: `m11b-dom-p2-${RUN}` });
await idle2;
console.log("  · seeded: two GLM turns (second with ipython tool)");

// available-model expectation for the picker (owner rule: auth-only)
const models = (await test.call("models.list", {})).result.models.filter((m) => m.available === true);
console.log(`  · models.list available=${models.length} providers=${[...new Set(models.map((m) => m.provider))].join(",")}`);

await launchChrome("test");
await navigate(`http://127.0.0.1:8788/?backend=bridge`);
await waitFor("sidebar", `!!document.querySelector('nav[aria-label="Sessions"]')`, 60000);
P(true, "legacy sidebar renders on ?backend=bridge");

// select the project, then the session (projects load async — retry the click)
const projClicked = await waitFor("project row", `(() => {
	const b = [...document.querySelectorAll("button.project-row-primary")].find((x) => (x.textContent ?? "").includes("m11b-dom-${RUN}"));
	if (!b) return false;
	b.click();
	return true;
})()`, 30000).then(() => true).catch(() => false);
P(projClicked, "project row clicked (session list filtered to the seeded project)");
const rowFound = await waitFor("session row", `(() => {
	const rows = [...document.querySelectorAll('.session-list-items li')];
	return rows.some((r) => (r.textContent ?? "").includes("m11b-dom-${RUN}"));
})()`, 30000).then(() => true).catch(() => false);
P(rowFound, "seeded session row visible in legacy session list");
await clickSessionRow(`m11b-dom-${RUN}`);
await waitFor("user bubbles", `document.querySelectorAll(".message-row.user-row .user-bubble").length >= 2`, 30000);
P(true, "user messages render as legacy user bubbles");
const domDone = await waitFor("assistant reply", `document.body.textContent.includes("DOMDONE")`, 30000).then(() => true).catch(() => false);
P(domDone, "assistant text in plain chat flow (DOMDONE visible)");
// B5: both seeded turns have ≤3 agent messages → NO See-more toggle at all;
// their full flow renders inline (detail auto-loads).
const seeMoreState = await evaluate(`(() => {
	const labels = [...document.querySelectorAll("button")].map((b) => (b.textContent ?? "").trim());
	return {
		seeMore: labels.filter((t) => t === "See more" || t === "Show details").length,
		rawPayloadLeaked: document.body.textContent.includes('"tool_call_id"') || document.body.textContent.includes("ipython({"),
	};
})()`);
P(seeMoreState.seeMore === 0, `B5: turns with ≤3 agent messages render NO See-more toggle (got ${seeMoreState.seeMore})`);
P(!seeMoreState.rawPayloadLeaked, "no raw tool payloads in the plain chat flow");
// B3: the ipython cell sits behind the per-turn "Used 1 tool" expandable —
// its I/O stays hidden until the group opens (no per-tool dropdown).
const groupOk = await waitFor("Used-tools group head", `!!document.querySelector(".tool-run-group-head")`, 30000).then(() => true).catch(() => false);
P(groupOk, "ipython tool call hidden behind the legacy Used-tools expandable");
const hiddenState = await evaluate(`(() => {
	const bodies = [...document.querySelectorAll(".tool-run-item-body")];
	return { visibleBodies: bodies.filter((b) => b.offsetParent !== null).length };
})()`);
P(hiddenState.visibleBodies === 0, "tool I/O hidden until expanded");
await evaluate(`(() => {
	[...document.querySelectorAll(".tool-run-group-head")].filter((h) => h.getAttribute("aria-expanded") !== "true").forEach((h) => h.click());
	return true;
})()`);
await sleep(800);
const toolVisible = await evaluate(`(() => {
	const bodies = [...document.querySelectorAll(".tool-run-item-body, .tool-run-detail, [class*=tool-run]")];
	return bodies.some((b) => b.offsetParent !== null && (b.textContent ?? "").includes("DOMTOOL"));
})()`);
P(toolVisible, "expanding the Used-tools group reveals the ipython cell I/O (DOMTOOL)");

// M11c (empty-bubble fix): textless assistant steps (tool-call-only, or
// thinking-only after the adapter drops thinking) must not render as empty
// assistant bubbles — they surface as grouped tool runs instead.
const bubbleA = await evaluate(`(() => {
	const rows = [...document.querySelectorAll(".message-row.assistant-row")];
	return { rows: rows.length, empty: rows.filter((r) => ((r.textContent ?? "").trim().length === 0)).length };
})()`);
traceM11c("partA.empty-bubbles", bubbleA);
P(bubbleA.rows >= 2, `seeded session renders assistant rows (${bubbleA.rows})`);
P(bubbleA.empty === 0, `M11c: no empty assistant bubbles in the seeded session (${bubbleA.empty} empty of ${bubbleA.rows})`);
const genericCards = await evaluate(`document.querySelectorAll("[class*=bridge-card], [class*=generic-card]").length`);
P(genericCards === 0, "no generic/bridge cards in the transcript");

// model picker: adapter-filtered (authenticated only)
const legacyKind = (provider) => (provider === "anthropic" ? "claude" : "openai");
const expectedIds = (list) => list.map((m) => `${legacyKind(m.provider)}:${m.id}`);
const picker = await evaluate(`(() => {
	const sel = document.querySelector('select[aria-label="Model"]');
	if (!sel) return null;
	return { count: sel.options.length, values: [...sel.options].map((o) => o.value), value: sel.value };
})()`);
P(!!picker && picker.count === models.length, `model picker lists exactly the ${models.length} authenticated models (got ${picker?.count})`);
P(!!picker && expectedIds(models).every((id) => picker.values.includes(id)), "picker covers every authenticated model in the legacy kind:model idiom");
P(!!picker && picker.values.every((v) => expectedIds(models).includes(v)), "no unauthenticated/synthetic options leak into the picker");

// /fork from the composer (REAL btrfs snapshot expected: session has projectId)
await typeIntoTextarea("textarea", "/fork");
await evaluate(`(() => { const b = document.querySelector('button[aria-label="send message"]'); if (b) { b.click(); return true; } return false; })()`);
let child = null;
for (let i = 0; i < 40 && !child; i++) {
	await sleep(500);
	const list = (await test.call("session.list", {})).result.sessions;
	child = list.find((s) => s.parentSessionId === sidA);
}
P(!!child, "/fork created a child session (parentSessionId set)");
if (child) {
	const url = await evaluate("window.location.href");
	P(url.includes(child.sessionId), `app navigated to the fork child (${child.sessionId.slice(0, 8)})`);
	const cwl = (await test.call("workspace.list", { sessionId: child.sessionId })).result;
	P(cwl.managed === true, "fork child workspace is managed");
	const subPath = `/home/schwinns/pi-relay/packages/bridge/data/workspace-state/sessions/${child.sessionId}/cwd`;
	P(existsSync(subPath), `fork child workspace exists: workspace-state/sessions/${child.sessionId.slice(0, 8)}/cwd`);
	let isSubvol = false;
	if (existsSync(subPath)) {
		// btrfs subvolume roots always have inode 256; readable without privileges.
		const ino = execFileSync("stat", ["-c", "%i", subPath], { encoding: "utf8" }).trim();
		const fs = execFileSync("stat", ["-f", "-c", "%T", subPath], { encoding: "utf8" }).trim();
		isSubvol = ino === "256" && fs === "btrfs";
		console.log(`  · child cwd inode=${ino} fs=${fs}`);
	}
	P(isSubvol, "REAL btrfs snapshot subvolume (inode 256 on btrfs)");
}

// /switch on the PARENT: dialog → pick DOMALPHA boundary → rewind + prefill
await clickSessionRow(`m11b-dom-${RUN}`);
await waitFor("parent selected", `document.querySelectorAll(".message-row.user-row .user-bubble").length >= 2`, 20000);
await typeIntoTextarea("textarea", "/switch");
await evaluate(`(() => { const b = document.querySelector('button[aria-label="send message"]'); if (b) { b.click(); return true; } return false; })()`);
const dialogUp = await waitFor("switch dialog", `!!document.querySelector('[role="dialog"]')`, 15000).then(() => true).catch(() => false);
P(dialogUp, "/switch opens the legacy history picker dialog");
const nOptions = await waitFor("switch targets", `document.querySelectorAll('[role="dialog"] button[aria-label^="Switch to User message"]').length >= 1 ? document.querySelectorAll('[role="dialog"] button[aria-label^="Switch to User message"]').length : false`, 15000).catch(() => 0);
P(nOptions >= 1, `boundary rows render as Switch-to-User-message buttons (${nOptions})`);
const clicked = await evaluate(`(() => {
	const btns = [...document.querySelectorAll('[role="dialog"] button[aria-label^="Switch to User message"]')];
	const b = btns.find((x) => (x.getAttribute("aria-label") ?? "").includes("DOMALPHA")) ?? btns.at(-1);
	if (!b) return false;
	b.click();
	return true;
})()`);
if (clicked) {
	await sleep(1500);
	const prefill = await evaluate(`(() => { const ta = document.querySelector("textarea"); return ta ? ta.value : ""; })()`);
	P(prefill.includes("DOMALPHA"), `composer prefilled with the boundary message ("${prefill.slice(0, 40)}…")`);
	let stillDone = true;
	for (let i = 0; i < 20; i++) {
		await sleep(500);
		stillDone = await evaluate(`(() => { const el = document.querySelector(".message-scroll"); return el ? (el.textContent ?? "").includes("DOMDONE") : false; })()`);
		if (!stillDone) break;
	}
	P(!stillDone, "transcript rewound: post-boundary assistant reply no longer rendered");
}

// REPL rail tab
const replTab = await evaluate(`(() => { const b = document.querySelector("#inspector-tab-repl"); if (b) { b.click(); return true; } return false; })()`);
P(replTab, "REPL tab present in the inspector rail");
if (replTab) {
	await waitFor("repl pane", `!!document.querySelector(".repl-pane")`, 10000);
	P(true, "REPL pane renders (bash-block idiom)");
	await typeIntoTextarea(".repl-pane textarea", `print('domrepl-${RUN}')`);
	await evaluate(`(() => {
		const ta = document.querySelector(".repl-pane textarea");
		ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", shiftKey: true, bubbles: true }));
		return true;
	})()`);
	const replOut = await waitFor("repl output", `document.body.textContent.includes("domrepl-${RUN}")`, 45000).then(() => true).catch(() => false);
	if (!replOut) console.log("  · repl pane text:", JSON.stringify(await evaluate(`(document.querySelector(".repl-pane") ?? document.body).innerText`)));
	P(replOut, "REPL cell executed end-to-end (domrepl output)");
}
stopChrome();
test.close();

// ============================================================== Part B (dogfood)
console.log("===== Part B: legacy UI on dogfood (:8789, gpt-5.6-sol) =====");
m11cTrace = `${TRACES_DIR}/m11c-bubbles-dogfood.jsonl`;
writeFileSync(m11cTrace, "");
const dogToken = readFileSync(new URL("../../../.pi/dogfood/bridge-token", import.meta.url).pathname, "utf8").trim();
const dog = new BridgeClient("ws://127.0.0.1:8731", dogToken, ORIGIN, `${TRACES_DIR}/m11b-dom-dogfood.jsonl`);
await dog.connect();
const dogModels = (await dog.call("models.list", {})).result.models.filter((m) => m.available === true);
const dogProviders = [...new Set(dogModels.map((m) => m.provider))].sort();
console.log(`  · dogfood available providers: ${dogProviders.join(", ")} (${dogModels.length} models)`);

// fresh live session for chat/tool-collapse/model-switch
const createdB = await dog.call("session.create", { name: `m11b-domdf-${RUN}`, cwd: "/tmp", idempotencyKey: `m11b-domdf-${RUN}` });
const sidB = createdB.result.sessionId;
await dog.call("session.attach", { id: sidB });

await launchChrome("dog");
await navigate(`http://127.0.0.1:8789/?backend=bridge`);
await waitFor("sidebar", `!!document.querySelector('nav[aria-label="Sessions"]')`, 60000);

// B1: rich legacy session — comms expandables, collapsed tools, subagent rail
const richFound = await clickSessionRow("m6-06:06:35");
P(richFound, "rich dogfood session row found (m6-06:06:35)");
if (richFound) {
	const richOk = await waitFor("rich transcript", `document.querySelectorAll(".message-row").length >= 2`, 60000).then(() => true).catch(() => false);
	if (!richOk) {
		console.log("  · URL:", await evaluate("window.location.href"));
		console.log("  · rows:", await evaluate(`document.querySelectorAll(".message-row").length`));
		console.log("  · text:", JSON.stringify((await evaluate("document.body.innerText") ?? "").slice(0, 400)));
	}
	assert(richOk, "rich transcript rendered");
	// older turns paginate behind the legacy load-older control; walk all the
	// way back, then expand every collapsed detail section (comms blocks live
	// inside turn details).
	for (let i = 0; i < 20; i++) {
		const more = await evaluate(`(() => {
			const b = document.querySelector(".turn-card-load-older") ?? [...document.querySelectorAll("button")].find((x) => /older|load more/i.test(x.textContent ?? ""));
			if (!b) return false;
			b.click();
			return true;
		})()`);
		if (!more) break;
		await sleep(1200);
	}
	// M11c (empty-bubble fix): the See-more threshold counts TEXT-BEARING
	// agent messages — the rich turn's 62 tool-call-only steps no longer
	// inflate the count (62 tools + 3 texts = 3 agent messages → NO toggle).
	// The full flow (grouped tool runs + comms) auto-loads inline instead.
	const richFlow = await waitFor(
		"rich turn flow auto-load (no See-more)",
		`document.querySelectorAll(".comms-message").length >= 1 && document.querySelectorAll(".tool-run-group-head").length >= 1`,
		60000,
	).then(() => true).catch(() => false);
	const seeMoreCount = await evaluate(`[...document.querySelectorAll("button")].filter((x) => (x.textContent ?? "").trim() === "See more").length`);
	traceM11c("partB.rich-see-more", { richFlow, seeMoreCount });
	P(richFlow, "rich turn flow auto-loads inline (grouped tool runs + comms, no toggle needed)");
	P(seeMoreCount === 0, `B5/M11c: 62 tool calls + 3 texts = 3 agent messages → NO See-more toggle (${seeMoreCount})`);
	const commsCount = await evaluate(`document.querySelectorAll(".comms-message").length`);
	P(commsCount >= 1, `comms render as legacy comms-message expandables (${commsCount})`);
	const collapsed = await evaluate(`[...document.querySelectorAll(".comms-message")].every((c) => !c.classList.contains("expanded"))`);
	P(collapsed, "comms blocks collapsed by default");
	await evaluate(`(() => { const t = document.querySelector(".comms-message .comms-message-toggle"); if (t) t.click(); return true; })()`);
	await sleep(300);
	const expanded = await evaluate(`!!document.querySelector(".comms-message.expanded .comms-message-body")`);
	P(expanded, "comms expandable opens to the full message body");
	const toolRuns = await evaluate(`document.querySelectorAll("[class*=tool-run]").length`);
	P(toolRuns >= 1, `tool runs present behind legacy expandables (${toolRuns})`);
	// SUBAGENTS rail = Agents tab (default): subagent rows navigate
	const subRows = await evaluate(`document.querySelectorAll("[class*=subagent], [class*=delegation]").length`);
	P(subRows >= 1, `SUBAGENTS rail renders the delegation tree (${subRows} nodes)`);
}

// M11c: the REAL owner dogfood session from the empty-bubble report
// (session_b09254c2, "Look into the Dynamo operator code…"). Read-only:
// select + render. Every visible assistant row must carry text — the 7
// tool-call-only steps render as grouped tool runs, the 3 real texts as
// bubbles, and the turn stays under the See-more threshold (no toggle).
// NOTE: three dogfood sessions share this prompt (the owner retried); the
// report session is the one whose final assistant text is a raw
// "<tool_calls>" blob — click matching rows until that marker renders.
let ownerFound = false;
for (let attempt = 0; attempt < 5 && !ownerFound; attempt++) {
	const clicked = await evaluate(`(() => {
		const rows = [...document.querySelectorAll(".session-list-items li")].filter((x) => (x.textContent ?? "").includes("Look into the Dynamo operator code"));
		const row = rows[${attempt}] ?? null;
		if (!row) return false;
		(row.querySelector("button") ?? row).click();
		return true;
	})()`);
	if (!clicked) break;
	const isReport = await waitFor(
		"report session marker (<tool_calls> summary text)",
		`(document.querySelector(".message-list-shell")?.textContent ?? "").includes("<tool_calls>")`,
		45000,
	).then(() => true).catch(() => false);
	if (isReport) ownerFound = true;
}
P(ownerFound, "owner dogfood session row found (the empty-bubble report session)");
if (ownerFound) {
	const ownerOk = await waitFor("owner transcript", `document.querySelectorAll(".message-row").length >= 2`, 60000).then(() => true).catch(() => false);
	assert(ownerOk, "owner session transcript rendered");
	const ownerFlow = await waitFor(
		"owner turn flow auto-load",
		`document.querySelectorAll(".tool-run-group-head").length >= 1`,
		60000,
	).then(() => true).catch(() => false);
	const bubbleState = await evaluate(`(() => {
		const rows = [...document.querySelectorAll(".message-row.assistant-row")];
		const empty = rows.filter((r) => ((r.textContent ?? "").trim().length === 0)).length;
		const texts = [...document.querySelectorAll(".assistant-content")].filter((c) => ((c.textContent ?? "").trim().length > 0)).length;
		const groups = document.querySelectorAll(".tool-run-group-head").length;
		const seeMore = [...document.querySelectorAll("button")].filter((x) => (x.textContent ?? "").trim() === "See more").length;
		return { rows: rows.length, empty, texts, groups, seeMore };
	})()`);
	traceM11c("partB.owner-empty-bubbles", { ownerFlow, ...bubbleState });
	P(ownerFlow, "owner turn flow auto-loads inline (grouped tool runs render)");
	P(bubbleState.rows >= 3, `owner session renders assistant rows (${bubbleState.rows})`);
	P(bubbleState.empty === 0, `M11c: NO empty assistant bubbles in the owner session (${bubbleState.empty} empty of ${bubbleState.rows})`);
	P(bubbleState.texts === 3, `M11c: exactly the 3 text-bearing agent messages render as bubbles (${bubbleState.texts})`);
	P(bubbleState.groups >= 2, `tool-call-only steps render as grouped tool runs (${bubbleState.groups} groups)`);
	P(bubbleState.seeMore === 0, `M11c: 3 agent messages ≤ threshold → no See-more toggle (${bubbleState.seeMore})`);
}

// B2: live gpt-5.6-sol turn with tool call
const newRow = await clickSessionRow(`m11b-domdf-${RUN}`);
P(newRow, "fresh dogfood session row selected");
wm = seqWatermark(dog, sidB);
const dfIdle = waitIdleAfter(dog, sidB, wm, "dogfood tool turn");
await typeIntoTextarea("textarea", "Use the ipython tool to run print('DOMDF') then reply with exactly: DOMDFDONE");
await evaluate(`(() => { const b = document.querySelector('button[aria-label="send message"]'); if (b) { b.click(); return true; } return false; })()`);
const dfDone = await waitFor("DOMDFDONE", `document.body.textContent.includes("DOMDFDONE")`, 180000).then(() => true).catch(() => false);
P(dfDone, "composer → real gpt-5.6-sol turn → assistant reply in chat flow");
await dfIdle.catch(() => {});
// B5: the fresh turn (≤3 agent messages) has no See-more toggle; its ipython
// cell sits behind the "Used 1 tool" group head — open it if the group
// collapsed after the turn completed.
const dfGroupOk = await waitFor("dogfood Used-tools group head", `!!document.querySelector(".tool-run-group-head")`, 30000).then(() => true).catch(() => false);
P(dfGroupOk, "DOMDF ipython call behind the Used-tools expandable");
await evaluate(`(() => {
	[...document.querySelectorAll(".tool-run-group-head")].filter((h) => h.getAttribute("aria-expanded") !== "true").forEach((h) => h.click());
	return true;
})()`);
await sleep(500);
const dfTool = await evaluate(`document.body.textContent.includes("DOMDF") && !!document.querySelector("[class*=tool-run]")`);
P(dfTool, "expanding the group reveals the DOMDF tool I/O");

// B3: model picker — exactly codex + nvidia-inference, no anthropic
const dogPicker = await evaluate(`(() => {
	const sel = document.querySelector('select[aria-label="Model"]');
	if (!sel) return null;
	return { count: sel.options.length, values: [...sel.options].map((o) => o.value) };
})()`);
P(!!dogPicker && dogPicker.count === dogModels.length, `dogfood picker = exactly the ${dogModels.length} authenticated models (got ${dogPicker?.count}; providers ${dogProviders.join(",")})`);
P(!!dogPicker && dogPicker.values.every((v) => expectedIds(dogModels).includes(v)) && expectedIds(dogModels).every((id) => dogPicker.values.includes(id)), "picker values == available set (auth-only filter, legacy ids)");
P(!!dogPicker && dogPicker.values.includes("openai:gpt-5.6-sol") && dogPicker.values.some((v) => v.includes("glm") || v.includes("GLM")), "codex + nvidia-inference entries present");

// B4: live model switch to GLM via the picker, then a turn
const glmBridge = dogModels.find((m) => m.provider === "nvidia-inference" && /glm/i.test(m.id));
const glmValue = glmBridge ? `${legacyKind(glmBridge.provider)}:${glmBridge.id}` : undefined;
if (glmValue) {
	const modelEv = dog.waitEvent((ev) => ev.event === "session.model" && ev.sessionId === sidB && (ev.data?.modelId ?? "").length > 0, 30000, "session.model after switch");
	await evaluate(`(() => {
		const sel = document.querySelector('select[aria-label="Model"]');
		const setter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, "value").set;
		setter.call(sel, ${JSON.stringify(glmValue)});
		sel.dispatchEvent(new Event("change", { bubbles: true }));
		return true;
	})()`);
	const ev = await modelEv.catch(() => null);
	P(!!ev && ev.data.modelId === glmBridge.id, `session.model event after picker switch (${ev?.data?.modelId})`);
	wm = seqWatermark(dog, sidB);
	const glmIdle = waitIdleAfter(dog, sidB, wm, "GLM turn on dogfood");
	await typeIntoTextarea("textarea", "Reply with exactly: DOMGLM");
	await evaluate(`(() => { const b = document.querySelector('button[aria-label="send message"]'); if (b) { b.click(); return true; } return false; })()`);
	const glmDone = await waitFor("DOMGLM", `document.body.textContent.includes("DOMGLM")`, 180000).then(() => true).catch(() => false);
	P(glmDone, "post-switch GLM turn completes through the same UI");
	await glmIdle.catch(() => {});
	// restore gpt-5.6-sol
	await dog.call("session.setModel", { sessionId: sidB, provider: "openai-codex", modelId: "gpt-5.6-sol" });
}
stopChrome();
dog.close();

console.log(failures === 0 ? "\nALL M11B DOM CHECKS PASSED" : `\n${failures} FAILURES`);
process.exit(failures === 0 ? 0 : 1);
