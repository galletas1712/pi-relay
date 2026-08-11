// WSS termination + slim contract v0 (JSON-RPC over a single WebSocket).
// Security at upgrade: Origin allowlist (exact match, missing rejected),
// bearer token (Authorization header), 8 MiB frame cap (ws maxPayload → 1009).
import http from "node:http";
import { WebSocketServer, WebSocket } from "ws";
import { config } from "./config.ts";
import * as db from "./db.ts";
import * as supervisor from "./supervisor.ts";
import { BridgeError, type ContractEvent } from "./supervisor.ts";
import * as workspaces from "./workspaces.ts";
import * as projects from "./projects.ts";
import { checkIdem, mergeMethodTables, requireString, type Params } from "./routes/common.ts";
import { commsMethods } from "./routes/comms.ts";
import { mcpMethods } from "./routes/mcp.ts";
import { modelMethods } from "./routes/models.ts";
import { projectMethods } from "./routes/project.ts";
import { replMethods } from "./routes/repl.ts";
import { subagentMethods } from "./routes/subagent.ts";
import { workspaceMethods } from "./routes/workspace.ts";

interface Subscription {
	watermark: number;
}

interface Conn {
	id: number;
	socket: WebSocket;
	isAlive: boolean;
	subs: Map<string, Subscription>; // sessionId -> subscription
}

const allConns = new Set<Conn>();

/** Session-less MCP auth lifecycle events go to every connected client. */
function broadcastMcpAuthChanged(server: string, status: string, scopes?: string[], detail?: string): void {
	for (const conn of allConns) {
		sendJson(conn.socket, { event: "mcp.authChanged", data: { server, status, ...(scopes ? { scopes } : {}), ...(detail ? { detail } : {}) } });
	}
}

let nextConnId = 1;

function sendJson(ws: WebSocket, obj: Record<string, unknown>): void {
	if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(obj));
}

function rpcError(id: unknown, code: string, message: string, data?: unknown): void {
	// filled by dispatch; placeholder for typing
}

// ---- method handlers -----------------------------------------------------------

const sessionMethods: Record<string, (conn: Conn, params: Params) => Promise<unknown>> = {
	async "session.list"() {
		return { sessions: await supervisor.listSessionStates() };
	},
	async "session.create"(_conn, params) {
		const cwd = params.cwd !== undefined ? requireString(params, "cwd") : undefined;
		const name = params.name !== undefined ? requireString(params, "name") : undefined;
		const projectId = params.projectId !== undefined ? requireString(params, "projectId") : undefined;
		const decls = params.workspaces !== undefined ? projects.parseWorkspaceDecls(params.workspaces) : undefined;
		const mcpSelection = params.mcpSelection; // validated against the MCP manager in mcp.ts (P7)
		const model = params.model !== undefined ? requireString(params, "model") : undefined; // M11a: "provider/modelId"
		const clientSessionId = params.sessionId !== undefined ? requireString(params, "sessionId") : undefined; // M11b: legacy startSession pre-chosen id
		const key = params.idempotencyKey !== undefined ? requireString(params, "idempotencyKey") : undefined;
		const replay = await checkIdem(key, "session.create", { cwd, name, projectId, workspaces: decls, mcpSelection, model, sessionId: clientSessionId });
		if (replay) return { ...(replay.replay as Record<string, unknown>), replay: true };
		const res = await supervisor.createSession({ cwd, name, projectId, workspaces: decls, mcpSelection, model, sessionId: clientSessionId });
		const response = { sessionId: res.sessionId, state: res.state, ...(res.model ? { model: res.model } : {}) };
		if (key) {
			await db.idemPut({
				key,
				method: "session.create",
				sessionId: res.sessionId,
				paramsHash: db.paramsHash({ cwd, name, projectId, workspaces: decls, mcpSelection, model }),
				response,
			});
		}
		return response;
	},
	async "session.delete"(_conn, params) {
		const id = requireString(params, "id");
		return supervisor.deleteSession(id);
	},
	async "session.rename"(_conn, params) {
		const id = requireString(params, "id");
		const name = requireString(params, "name");
		return supervisor.renameSession(id, name);
	},
	async "session.attach"(conn, params) {
		const id = requireString(params, "id");
		let fromSeq: number | null = null;
		if (params.fromSeq !== undefined && params.fromSeq !== null) {
			const n = Number(params.fromSeq);
			if (!Number.isInteger(n) || n < 0) throw new BridgeError("bad_request", "params.fromSeq must be a non-negative integer");
			fromSeq = n;
		}
		const row = await db.getSession(id);
		if (!row) throw new BridgeError("session_not_found", `unknown session ${id}`);
		const head = Number(row.last_event_seq);
		if (fromSeq === null) {
			// attach at head: live-only (client loads state via session.getState)
			conn.subs.set(id, { watermark: head });
			return { sessionId: id, headSeq: head, replayed: 0 };
		}
		if (fromSeq < head) {
			const min = await supervisor.spoolFloor(id);
			// min === 0 with head > fromSeq means the spool was trimmed to EMPTY:
			// nothing at all is replayable, which is also an event_gap.
			if (min === 0 || fromSeq + 1 < min) {
				throw new BridgeError(
					"event_gap",
					min === 0
						? `events ${fromSeq + 1}..${head} were trimmed from the spool`
						: `events ${fromSeq + 1}..${min - 1} were trimmed from the spool`,
					{ minAvailable: min === 0 ? head + 1 : min, headSeq: head },
				);
			}
		}
		conn.subs.set(id, { watermark: fromSeq });
		const deliver = (ev: ContractEvent) => {
			const sub = conn.subs.get(id);
			if (!sub || ev.seq <= sub.watermark) return;
			sub.watermark = ev.seq;
			sendJson(conn.socket, { event: ev.event, sessionId: ev.sessionId, seq: ev.seq, data: ev.data, at: ev.at, replayed: true });
		};
		const res = await supervisor.replayEvents(id, fromSeq, deliver);
		return { sessionId: id, headSeq: res.headSeq, replayed: res.replayed };
	},
	async "session.detach"(conn, params) {
		const id = requireString(params, "id");
		conn.subs.delete(id);
		return { detached: true };
	},
	async "prompt.send"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		const text = requireString(params, "text");
		const key = params.idempotencyKey !== undefined ? requireString(params, "idempotencyKey") : undefined;
		const replay = await checkIdem(key, "prompt.send", { sessionId, text });
		if (replay) return { ...(replay.replay as Record<string, unknown>), replay: true };
		if (key) {
			// crash-mid-flight guard: a journal row with this key means we already accepted it
			const rows = await db.journalPending(sessionId);
			const dup = rows.find((r) => r.idempotency_key === key);
			if (dup) return { accepted: true, seq: Number(dup.seq), queued: true, replay: true };
		}
		const res = await supervisor.sendUserCommand(sessionId, "prompt", text);
		const response = { accepted: true, seq: res.seq, queued: res.queued };
		if (key) {
			await db.idemPut({
				key,
				method: "prompt.send",
				sessionId,
				paramsHash: db.paramsHash({ sessionId, text }),
				response,
			});
			// link the journal row to the key for the crash-mid-flight guard
			await db.pool.query("UPDATE command_journal SET idempotency_key = $1 WHERE seq = $2", [key, res.seq]);
		}
		return response;
	},
	async "session.steer"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		const text = requireString(params, "text");
		const res = await supervisor.sendUserCommand(sessionId, "steer", text);
		return { accepted: true, seq: res.seq, queued: res.queued };
	},
	async "session.followUp"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		const text = requireString(params, "text");
		const res = await supervisor.sendUserCommand(sessionId, "follow_up", text);
		return { accepted: true, seq: res.seq, queued: res.queued };
	},
	async "session.abort"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		const res = await supervisor.sendUserCommand(sessionId, "abort", undefined);
		return { accepted: true, seq: res.seq, queued: res.queued };
	},
	async "session.getState"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		return supervisor.getSessionState(sessionId);
	},
	async "subagent.tree"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		return supervisor.subagentTree(sessionId);
	},
	// ---- M11b: fork / switch / fork points / commands / compact ----
	async "session.fork"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		const entryId = params.entryId !== undefined && params.entryId !== null ? requireString(params, "entryId") : undefined;
		const key = params.idempotencyKey !== undefined ? requireString(params, "idempotencyKey") : undefined;
		const replay = await checkIdem(key, "session.fork", { sessionId, entryId });
		if (replay) return { ...(replay.replay as Record<string, unknown>), replay: true };
		const res = await supervisor.forkSession(sessionId, entryId);
		if (key) {
			await db.idemPut({
				key,
				method: "session.fork",
				sessionId,
				paramsHash: db.paramsHash({ sessionId, entryId }),
				response: res,
			});
		}
		return res;
	},
	async "session.switch"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		const entryId = requireString(params, "entryId");
		const key = params.idempotencyKey !== undefined ? requireString(params, "idempotencyKey") : undefined;
		const replay = await checkIdem(key, "session.switch", { sessionId, entryId });
		if (replay) return { ...(replay.replay as Record<string, unknown>), replay: true };
		const res = await supervisor.switchSession(sessionId, entryId);
		if (key) {
			await db.idemPut({
				key,
				method: "session.switch",
				sessionId,
				paramsHash: db.paramsHash({ sessionId, entryId }),
				response: res,
			});
		}
		return res;
	},
	async "session.getForkPoints"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		return supervisor.getForkPoints(sessionId);
	},
	async "session.commands"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		return supervisor.getCommands(sessionId);
	},
	async "session.getEntries"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		const ids = params.ids;
		if (!Array.isArray(ids) || ids.some((v) => typeof v !== "string")) {
			throw new BridgeError("bad_request", "params.ids must be an array of strings");
		}
		return supervisor.getSessionEntryTexts(sessionId, ids as string[]);
	},
	async "session.compact"(_conn, params) {
		const sessionId = requireString(params, "sessionId");
		return supervisor.compactSession(sessionId);
	},
};

const methods = mergeMethodTables(
	sessionMethods,
	workspaceMethods(),
	projectMethods(),
	mcpMethods(broadcastMcpAuthChanged),
	replMethods(),
	modelMethods(),
	subagentMethods(),
	commsMethods(),
);

// ---- server --------------------------------------------------------------------

export function startServer(): http.Server {
	const server = http.createServer((req, res) => {
		if (req.url === "/healthz") {
			res.writeHead(200, { "content-type": "application/json" });
			res.end(JSON.stringify({ ok: true }));
			return;
		}
		res.writeHead(404);
		res.end();
	});

	const wss = new WebSocketServer({ noServer: true, maxPayload: config.maxFrameBytes });

	server.on("upgrade", (req, socket, head) => {
		const origin = req.headers.origin;
		const auth = req.headers.authorization;
		const fail = (status: number, code: string, msg: string) => {
			socket.write(`HTTP/1.1 ${status} ${http.STATUS_CODES[status]}\r\ncontent-type: application/json\r\n\r\n${JSON.stringify({ error: { code, message: msg } })}`);
			socket.destroy();
		};
		// Origin discipline from the old contract: exactly one, exact allowlist match.
		if (typeof origin !== "string" || origin === "") {
			return fail(403, "forbidden_origin", "missing Origin header");
		}
		if (!config.allowedOrigins.includes(origin)) {
			return fail(403, "forbidden_origin", `origin not allowed: ${origin}`);
		}
		if (auth !== `Bearer ${config.authToken}`) {
			return fail(401, "unauthorized", "bad or missing bearer token");
		}
		wss.handleUpgrade(req, socket, head, (ws) => {
			wss.emit("connection", ws, req);
		});
	});

	wss.on("connection", (ws) => {
		const conn: Conn = { id: nextConnId++, socket: ws, isAlive: true, subs: new Map() };
		allConns.add(conn);
		ws.on("close", () => allConns.delete(conn));
		ws.on("error", () => allConns.delete(conn));
		// fan out supervisor events to subscribed connections (watermark = no dupes)
		const off = supervisor.onContractEvent((ev) => {
			const sub = conn.subs.get(ev.sessionId);
			if (!sub || ev.seq <= sub.watermark) return;
			sub.watermark = ev.seq;
			sendJson(ws, { event: ev.event, sessionId: ev.sessionId, seq: ev.seq, data: ev.data, at: ev.at });
		});
		ws.on("close", () => off());
		ws.on("error", () => off());
		ws.on("message", async (data: Buffer, isBinary: boolean) => {
			if (isBinary) {
				sendJson(ws, { id: null, error: { code: "bad_request", message: "binary frames not supported" } });
				return;
			}
			let req: { id?: unknown; method?: unknown; params?: unknown };
			try {
				req = JSON.parse(data.toString("utf8"));
			} catch {
				sendJson(ws, { id: null, error: { code: "bad_request", message: "malformed JSON" } });
				return;
			}
			const id = req.id ?? null;
			if (typeof req.method !== "string") {
				sendJson(ws, { id, error: { code: "bad_request", message: "request must have a string method" } });
				return;
			}
			const params = (req.params ?? {}) as Params;
			try {
				const handler = methods[req.method];
				if (!handler) throw new BridgeError("method_not_found", `unknown method: ${req.method}`);
				const result = await handler(conn, params);
				sendJson(ws, { id, result });
			} catch (err) {
				if (err instanceof BridgeError) {
					sendJson(ws, { id, error: { code: err.code, message: err.message, ...(err.data !== undefined ? { data: err.data } : {}) } });
				} else if (err instanceof Error && typeof (err as { code?: unknown }).code === "string") {
					// Coded errors from non-supervisor layers (WorkspaceError,
					// McpConfigError, ...) carry their contract code on .code.
					const coded = err as Error & { code: string };
					sendJson(ws, { id, error: { code: coded.code, message: coded.message } });
				} else {
					console.error("[bridge] method error:", err);
					sendJson(ws, { id, error: { code: "internal", message: String(err instanceof Error ? err.message : err) } });
				}
			}
		});
	});

	server.listen(config.port, "127.0.0.1", () => {
		console.error(`[bridge] WSS listening on 127.0.0.1:${config.port} (ws only; TLS termination upstream)`);
	});
	return server;
}
