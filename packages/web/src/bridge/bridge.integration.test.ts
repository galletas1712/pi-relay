/**
 * M6 integration tests against the REAL running bridge at 127.0.0.1:8730.
 *
 * Gated: BRIDGE_INTEGRATION=1 npx vitest run packages/web/src/bridge/bridge.integration.test.ts
 * Auth:  token read from .pi/m1-demo/.bridge-auth-token; Node ws sets the
 *        Authorization + allowlisted Origin upgrade headers (the browser path
 *        uses the vite /__bridge-ws proxy instead).
 * Traces: .pi/m1-demo/traces/m6-f{1,2,3,4}.jsonl — every request, response,
 *         event, and assertion is recorded.
 *
 * F1 happy path: create → attach(fromSeq 0) → prompt with ipython marker.
 * F2 mid-stream reconnect: drop the socket during a turn; resume from the
 *    watermark; assert seq contiguity (no dupes, no holes).
 * F3 subagent fanout: rlm children → subagent.lifecycle + subagent.tree.
 * F4 idempotency + typed errors: double-create replay, conflict,
 *    session_not_found, session_busy, workspace.* session_not_found on
 *    unknown ids (real since M8), mcp.* not_implemented, method_not_found.
 * M8 SPA path: createBridgeWorkspaceBackend over the real bridge (m8-w2-spa).
 */
import { mkdirSync, readFileSync, appendFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { BridgeClient, BridgeRequestError, type BridgeWebSocket } from "./client.ts";
import { BridgeSessionStore } from "./sessionStore.ts";
import { seqContiguity, type MessageBlock, type ToolExecBlock } from "./eventStore.ts";
import type { ContractEventEnvelope } from "./types.ts";

const RUN = process.env.BRIDGE_INTEGRATION === "1";
const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "../../../..");
const tokenFile = process.env.BRIDGE_TOKEN_FILE ?? path.join(repoRoot, ".pi/m1-demo/.bridge-auth-token");
const tracesDir = path.join(repoRoot, ".pi/m1-demo/traces");
const bridgeUrl = process.env.BRIDGE_WS_URL ?? "ws://127.0.0.1:8730";

const nodeRequire = createRequire(import.meta.url);
const NodeWebSocket = nodeRequire("ws") as new (url: string, options: { headers: Record<string, string> }) => BridgeWebSocket;

let lastSocket: BridgeWebSocket | null = null;

class Tracer {
	private readonly file: string;
	constructor(name: string) {
		mkdirSync(tracesDir, { recursive: true });
		this.file = path.join(tracesDir, name);
		writeFileSync(this.file, "");
	}
	record(kind: string, payload: unknown): void {
		appendFileSync(this.file, JSON.stringify({ t: new Date().toISOString(), kind, payload }) + "\n");
	}
}

function makeClient(options: { reconnectDelayMs?: number } = {}): BridgeClient {
	const token = readFileSync(tokenFile, "utf8").replace(/[\r\n]/g, "");
	const client = new BridgeClient(bridgeUrl, {
		webSocketFactory: (url) => {
			lastSocket = new NodeWebSocket(url, {
				headers: { authorization: `Bearer ${token}`, origin: "http://localhost:3000" },
			});
			return lastSocket;
		},
		reconnectDelayMs: options.reconnectDelayMs ?? 50,
		requestTimeoutMs: 60_000,
	});
	return client;
}

/** Wait until predicate() holds; throws with context on timeout. */
async function waitFor(desc: string, predicate: () => boolean, timeoutMs: number): Promise<void> {
	const start = Date.now();
	while (Date.now() - start < timeoutMs) {
		if (predicate()) return;
		await new Promise((r) => setTimeout(r, 100));
	}
	throw new Error(`timeout waiting for: ${desc}`);
}

const createdSessionIds: string[] = [];

describe.skipIf(!RUN)("m6 bridge integration (real bridge)", () => {
	let client: BridgeClient;
	let store: BridgeSessionStore;
	const events: ContractEventEnvelope[] = [];
	let tracer: Tracer;

	function wireEvents(c: BridgeClient): void {
		c.onEvent((ev) => {
			events.push(ev);
			tracer?.record("event", ev);
		});
	}

	beforeAll(async () => {
		client = makeClient();
		store = new BridgeSessionStore(client);
		wireEvents(client);
		await client.connect();
	}, 30_000);

	afterAll(async () => {
		// M8 added session.delete: full teardown (host + workspace subvolume + PG rows).
		for (const sessionId of createdSessionIds) {
			try {
				await client.request("session.delete", { id: sessionId });
			} catch { /* best-effort */ }
			try {
				await client.request("session.detach", { id: sessionId });
			} catch { /* best-effort */ }
		}
		client.close();
	});

	async function tracedRequest<T>(method: string, params: Record<string, unknown>): Promise<T> {
		tracer.record("request", { method, params });
		try {
			const result = await client.request<T>(method, params);
			tracer.record("response", { method, result });
			return result;
		} catch (error) {
			tracer.record("error", {
				method,
				code: error instanceof BridgeRequestError ? error.code : "transport",
				message: error instanceof Error ? error.message : String(error),
				...(error instanceof BridgeRequestError && error.data !== undefined ? { data: error.data } : {}),
			});
			throw error;
		}
	}

	async function newSession(name: string): Promise<string> {
		const res = await tracedRequest<{ sessionId: string }>("session.create", { name });
		createdSessionIds.push(res.sessionId);
		return res.sessionId;
	}

	function appliedSeqs(sessionId: string): number[] {
		return events.filter((e) => e.sessionId === sessionId).map((e) => e.seq);
	}

	/** Assistant-authored text only — the optimistic user echo contains the
	 * prompt text (including marker strings), so it must not satisfy waits. */
	function assistantText(sessionId: string): string {
		const blocks = store.getSnapshot(sessionId)?.blocks ?? [];
		return blocks
			.filter((b): b is MessageBlock => b.kind === "message" && b.role === "assistant" && b.local !== true)
			.map((b) => b.text)
			.join("\n");
	}

	function toolBlocks(sessionId: string): ToolExecBlock[] {
		return (store.getSnapshot(sessionId)?.blocks ?? []).filter((b): b is ToolExecBlock => b.kind === "tool");
	}

	it("F1: happy path — create, attach, prompt with ipython marker", async () => {
		tracer = new Tracer("m6-f1.jsonl");
		events.length = 0;
		const marker = `M6-F1-${Math.random().toString(36).slice(2, 8)}`;
		const sessionId = await newSession("m6-f1");
		await store.attach(sessionId);
		tracer.record("attach", { sessionId, watermark: store.getSnapshot(sessionId)?.watermark });

		const send = await store.sendPrompt(
			sessionId,
			[
				`Use the ipython tool to run exactly this code:`,
				``,
				`print("${marker}")`,
				``,
				`Then reply with just "M6-F1-DONE" and stop.`,
			].join("\n"),
		);
		tracer.record("prompt", { sessionId, send });

		await waitFor("turn running", () => store.getSnapshot(sessionId)?.state === "running", 60_000);
		await waitFor(
			"assistant M6-F1-DONE",
			() => store.getSnapshot(sessionId)?.state === "idle" && assistantText(sessionId).includes("M6-F1-DONE"),
			240_000,
		);

		const projection = store.getSnapshot(sessionId)!;
		const ipythonRuns = toolBlocks(sessionId).filter((tb) => tb.toolName === "ipython" && tb.done && !tb.isError);
		const markerSeen = ipythonRuns.some((tb) => (tb.result ?? "").includes(marker) || (tb.args ?? "").includes(marker));

		const contiguity = seqContiguity(appliedSeqs(sessionId));
		const state = await tracedRequest<{ headSeq: number; state: string }>("session.getState", { sessionId });
		tracer.record("assert", {
			blocks: projection.blocks.length,
			ipythonRuns: ipythonRuns.length,
			markerSeen,
			contiguity,
			watermark: projection.watermark,
			headSeq: state.headSeq,
			gap: projection.gap,
		});

		expect(ipythonRuns.length).toBeGreaterThanOrEqual(1);
		expect(markerSeen).toBe(true);
		expect(projection.gap).toBeNull();
		expect(contiguity.contiguous).toBe(true);
		expect(projection.watermark).toBe(state.headSeq);
	}, 300_000);

	it("F2: mid-stream reconnect resumes from the watermark with seq contiguity", async () => {
		tracer = new Tracer("m6-f2.jsonl");
		// dedicated client: hold the reconnect for 4 s after the drop so the
		// outage provably spans bridge-side events (the ipython loop ticks every
		// second), then assert the replay set that healed the projection.
		client.close();
		client = makeClient({ reconnectDelayMs: 4_000 });
		store = new BridgeSessionStore(client);
		wireEvents(client);
		events.length = 0;
		await client.connect();
		const marker = `M6-F2-${Math.random().toString(36).slice(2, 8)}`;
		const sessionId = await newSession("m6-f2");
		await store.attach(sessionId);
		const attachHead = store.getSnapshot(sessionId)!.watermark;
		tracer.record("attach", { sessionId, attachHead });

		await store.sendPrompt(
			sessionId,
			[
				`Use the ipython tool to run exactly this code:`,
				``,
				`import time`,
				`for i in range(6):`,
				`    print("${marker}-tick", i)`,
				`    time.sleep(1)`,
				`print("${marker}-done")`,
				``,
				`Then reply with just "M6-F2-DONE" and stop.`,
			].join("\n"),
		);

		// wait until the model is mid-turn with an ipython cell executing, then
		// drop the socket: events during the outage must be recovered by replay.
		await waitFor("turn running", () => store.getSnapshot(sessionId)?.state === "running", 60_000);
		await waitFor(
			"ipython cell executing mid-stream",
			() => toolBlocks(sessionId).some((tb) => tb.toolName === "ipython"),
			120_000,
		);
		const preDropWatermark = store.getSnapshot(sessionId)!.watermark;
		tracer.record("socket-drop", { preDropWatermark });
		lastSocket?.close();

		// client auto-reconnects after the 4 s hold; the store re-attaches at
		// fromSeq=preDropWatermark and the spool replays the missed window.
		await waitFor("client reconnected", () => client.isOpen(), 30_000);
		tracer.record("reconnected", {});

		await waitFor(
			"assistant M6-F2-DONE after resume",
			() => store.getSnapshot(sessionId)?.state === "idle" && assistantText(sessionId).includes("M6-F2-DONE"),
			240_000,
		);

		const projection = store.getSnapshot(sessionId)!;
		const sessionEvents = events.filter((e) => e.sessionId === sessionId);
		const replayedAfterReconnect = sessionEvents.filter((e) => e.replayed === true && e.seq > preDropWatermark);
		const contiguity = seqContiguity(appliedSeqs(sessionId));
		const state = await tracedRequest<{ headSeq: number }>("session.getState", { sessionId });
		tracer.record("assert", {
			preDropWatermark,
			replayedAfterReconnect: replayedAfterReconnect.map((e) => e.seq),
			contiguity,
			watermark: projection.watermark,
			headSeq: state.headSeq,
			gap: projection.gap,
		});

		expect(projection.gap).toBeNull();
		expect(contiguity.contiguous).toBe(true);
		expect(replayedAfterReconnect.length).toBeGreaterThanOrEqual(1);
		expect(projection.watermark).toBe(state.headSeq);
		const ipythonRuns = toolBlocks(sessionId).filter((tb) => tb.toolName === "ipython" && tb.done);
		expect(ipythonRuns.length).toBeGreaterThanOrEqual(1);
		expect(JSON.stringify(ipythonRuns)).toContain(`${marker}-done`);
	}, 300_000);

	it("F3: subagent fanout — lifecycle events + subagent.tree", async () => {
		tracer = new Tracer("m6-f3.jsonl");
		events.length = 0;
		const sessionId = await newSession("m6-f3");
		await store.attach(sessionId);

		await store.sendPrompt(
			sessionId,
			[
				`Use the ipython tool for everything below. The \`rlm\` object and the \`agent_message\` module are available in your kernel.`,
				``,
				`Run exactly ONE ipython cell containing ALL of the following code (nothing else):`,
				``,
				`h1 = await rlm("In ipython run exactly: await agent_message.send(\"M6-F3-PONG-A\", receiver_role=\"parent\") — then stop.", name="m6f3a")`,
				`h2 = await rlm("In ipython run exactly: await agent_message.send(\"M6-F3-PONG-B\", receiver_role=\"parent\") — then stop.", name="m6f3b")`,
				`print("CHILDREN_SPAWNED", h1, h2)`,
				``,
				`Then END YOUR TURN immediately. Reply with only "M6-F3-SPAWNED" and stop. Do NOT poll or wait for the children.`,
			].join("\n"),
		);

		await waitFor(
			"two subagents admitted",
			() => (store.getSnapshot(sessionId)?.subagents.length ?? 0) >= 2,
			240_000,
		);
		tracer.record("admitted", { subagents: store.getSnapshot(sessionId)?.subagents });

		await waitFor(
			"both children completed",
			() => {
				const subs = store.getSnapshot(sessionId)?.subagents ?? [];
				return subs.length >= 2 && subs.every((s) => s.status === "completed" || s.status === "error");
			},
			240_000,
		);

		const live = store.getSnapshot(sessionId)!.subagents;
		const tree = await tracedRequest<{ children: Array<{ rlmChildId: string; name: string | null; status: string; depth: number }> }>(
			"subagent.tree",
			{ sessionId },
		);
		tracer.record("assert", { live, tree: tree.children });

		expect(live.length).toBeGreaterThanOrEqual(2);
		expect(tree.children.length).toBeGreaterThanOrEqual(2);
		const names = tree.children.map((c) => c.name ?? "");
		expect(names.some((n) => n.includes("m6f3a"))).toBe(true);
		expect(names.some((n) => n.includes("m6f3b"))).toBe(true);
		expect(tree.children.every((c) => c.depth >= 1)).toBe(true);
	}, 420_000);

	it("F4: idempotency replay/conflict + typed error taxonomy", async () => {
		tracer = new Tracer("m6-f4.jsonl");
		events.length = 0;

		// idempotent double-create: same key + same params replays the response
		const key = `m6f4-create-${Date.now()}`;
		const first = await tracedRequest<{ sessionId: string; state: string; replay?: boolean }>("session.create", {
			name: "m6-f4",
			idempotencyKey: key,
		});
		createdSessionIds.push(first.sessionId);
		const second = await tracedRequest<{ sessionId: string; replay?: boolean }>("session.create", {
			name: "m6-f4",
			idempotencyKey: key,
		});
		expect(second.sessionId).toBe(first.sessionId);
		expect(second.replay).toBe(true);

		// same key, different params -> idempotency_conflict
		const conflict = await tracedRequest("session.create", { name: "m6-f4-different", idempotencyKey: key }).catch((e: unknown) => e);
		expect(conflict).toBeInstanceOf(BridgeRequestError);
		expect((conflict as BridgeRequestError).code).toBe("idempotency_conflict");

		// unknown session -> session_not_found
		const notFound = await tracedRequest("session.attach", { id: "00000000-0000-0000-0000-000000000000" }).catch((e: unknown) => e);
		expect((notFound as BridgeRequestError).code).toBe("session_not_found");

		// workspace.* is real since M8: unknown session -> session_not_found
		for (const method of ["workspace.list", "workspace.read_file", "workspace.git_diff"]) {
			const err = await tracedRequest(method, { sessionId: "00000000-0000-0000-0000-000000000000", path: "." }).catch((e: unknown) => e);
			expect(err).toBeInstanceOf(BridgeRequestError);
			expect((err as BridgeRequestError).code).toBe("session_not_found");
		}
		// mcp.* remains reserved until the M8 MCP manager lands
		{
			const err = await tracedRequest("mcp.inventory", {}).catch((e: unknown) => e);
			expect(err).toBeInstanceOf(BridgeRequestError);
			expect((err as BridgeRequestError).code).toBe("not_implemented");
		}

		// unknown method -> method_not_found
		const unknown = await tracedRequest("nope.nope", {}).catch((e: unknown) => e);
		expect((unknown as BridgeRequestError).code).toBe("method_not_found");

		// running turn -> second prompt is session_busy
		await store.attach(first.sessionId);
		await store.sendPrompt(
			first.sessionId,
			`Use the ipython tool to run exactly: import time; time.sleep(20); print("M6-F4-BUSY") — then reply "M6-F4-DONE".`,
		);
		await waitFor("f4 turn running", () => store.getSnapshot(first.sessionId)?.state === "running", 60_000);
		const busy = await tracedRequest("prompt.send", { sessionId: first.sessionId, text: "second while busy" }).catch((e: unknown) => e);
		expect(busy).toBeInstanceOf(BridgeRequestError);
		expect((busy as BridgeRequestError).code).toBe("session_busy");
		await tracedRequest("session.abort", { sessionId: first.sessionId });
		tracer.record("assert", { doubleCreate: "ok", conflict: "ok", typedErrors: "ok", busy: "ok" });
	}, 180_000);

	describe("M8 SPA workspace path (real bridge → host → createBridgeWorkspaceBackend)", () => {
		it("project workspaces materialize; backend reads/searches/diffs through the live bridge", async () => {
			tracer = new Tracer("m8-w2-spa.jsonl");
			const tracer8 = tracer;
			const { execFileSync } = await import("node:child_process");
			const { mkdirSync: mk, writeFileSync: wr } = await import("node:fs");
			const run = path.resolve(here, "../../../../packages/bridge/test/.tmp-m8-spa");
			rmSync(run, { recursive: true, force: true });
			const sh = (args: string[], cwd: string) =>
				execFileSync("git", ["-c", "commit.gpgsign=false", ...args], {
					cwd,
					env: { ...process.env, GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@t", GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@t" },
				});
			mk(run, { recursive: true });
			const remote = path.join(run, "remote");
			mk(remote, { recursive: true });
			sh(["init", "-b", "main"], remote);
			wr(path.join(remote, "app.ts"), "export const spa = 1;\n");
			sh(["add", "-A"], remote);
			sh(["commit", "-m", "init"], remote);
			const localSrc = path.join(run, "local-src");
			mk(localSrc, { recursive: true });
			wr(path.join(localSrc, "guide.md"), "spa local guide\n");
			tracer8.record("fixtures", { remote, localSrc });

			const created = await tracedRequest<{ sessionId: string }>("session.create", {
				name: "m8-spa",
				workspaces: [
					{ kind: "git", workspaceDir: "repo", remoteUrl: remote, remoteBranch: "main" },
					{ kind: "local", workspaceDir: "docs", sourcePath: localSrc },
				],
			});
			createdSessionIds.push(created.sessionId);
			tracer8.record("session", created);

			// The REAL adapter the SPA builds at the M8 swap point in BridgeApp.
			const { createBridgeWorkspaceBackend } = await import("./workspace.ts");
			const backend = createBridgeWorkspaceBackend(client, created.sessionId);

			const listing = await backend.listDir({ path: "" });
			tracer8.record("listDir", listing);
			expect(listing.entries.map((e) => e.path).sort()).toEqual(["docs", "repo"]);

			const file = await backend.readFile({ path: "repo/app.ts" });
			expect(file.contents).toBe("export const spa = 1;\n");
			expect(file.truncated).toBe(false);

			// write through the contract, then see it via the adapter's git views
			await tracedRequest("workspace.write_file", {
				sessionId: created.sessionId,
				path: "repo/spa-note.txt",
				contentBase64: Buffer.from("spa marker\n").toString("base64"),
			});
			const status = await backend.gitStatus();
			tracer8.record("gitStatus", status);
			expect(status.entries.some((e) => e.path === "repo/spa-note.txt" && e.status === "untracked")).toBe(true);

			const { diff } = await backend.gitDiff({});
			tracer8.record("gitDiff", { bytes: diff.length });
			expect(diff).toContain("spa-note.txt");
			expect(diff).toContain("+spa marker");

			// workspace.list surfaces the session's roots to the SPA
			const roots = await tracedRequest<{ workspaces: Array<{ workspaceDir: string; kind: string; commitOid?: string }> }>(
				"workspace.list",
				{ sessionId: created.sessionId },
			);
			expect(roots.workspaces.map((w) => w.workspaceDir).sort()).toEqual(["docs", "repo"]);
			expect(roots.workspaces.find((w) => w.workspaceDir === "repo")?.commitOid).toMatch(/^[0-9a-f]{40}$/);
			tracer8.record("assert", { spaPath: "ok" });
			rmSync(run, { recursive: true, force: true });
		}, 120_000);
	});
});
