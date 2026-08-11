// Supervisor: owns one SessionHandle per product session — spawn/respawn the
// pi rpc host, journal every command (PG + per-session JSONL mirror), map host
// events into contract events, spool them (resumable stream), and replay the
// durable queue after respawn.
import { randomUUID } from "node:crypto";
import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import path from "node:path";
import { HostProcess } from "./host.ts";
import { config, journalDir, sessionsDir } from "./config.ts";
import * as db from "./db.ts";
import type { SessionWorkspace, WorkspaceDecl } from "@pi-relay/workspace-lib";
import { destroyForSession, ensureForSession, forkForSession, materializeForSession } from "./workspaces.ts";
import { branchFilePath, findEntryFile, lastLeafId, messageText, readAllBranchRecords, userMessageBoundaries, userMessageText, writeBranchFile } from "./fork.ts";
import { readSessionRecords } from "./transcript.ts";
import { McpManager, type McpSessionSelection } from "./mcp/manager.ts";
import { OAuthManager } from "./mcp/oauth.ts";
import { firstUserText, generateTitle } from "./titles.ts";
import { rebuildBranchTranscript } from "./transcript.ts";
import { buildSessionCatalog, removeSessionMcpArtifacts, writeSessionMcpCatalog } from "./mcp/catalog.ts";

export class BridgeError extends Error {
	code: string;
	data?: unknown;
	constructor(code: string, message: string, data?: unknown) {
		super(message);
		this.code = code;
		this.data = data;
	}
}

export interface ContractEvent {
	event: string;
	sessionId: string;
	seq: number;
	data: Record<string, unknown>;
	at: string;
}

interface SessionHandle {
	id: string;
	cwd: string;
	name: string | null;
	sessionFile: string | null;
	state: string;
	projectId: string | null;
	workspaces: SessionWorkspace[];
	mcpSelection: unknown;
	generation: number;
	host: HostProcess | null;
	eventSeq: number;
	intentionalStop: boolean;
	/** serialize host-bound sends (prompt/steer/followUp/abort) per session */
	sendChain: Promise<unknown>;
	/** serialize spool append vs attach replay per session */
	eventChain: Promise<unknown>;
	/** M8: sidecar title generation attempted for this handle (once per host lifetime). */
	titleAttempted?: boolean;
	/** M9: prime-rlm reported repl capability (prime_rlm_ready session entry). */
	replReady?: boolean;
	/** M11b: an on-demand respawn is in flight (dedupe prompt-triggered respawns). */
	respawnInflight?: boolean;
}

const handles = new Map<string, SessionHandle>();
const listeners = new Set<(ev: ContractEvent) => void>();
let shuttingDown = false;

// M8 phase 2: bridge-hosted MCP manager (control plane) + OAuth state machine.
let mcpManager: McpManager | null = null;
let oauthManager: OAuthManager | null = null;

/** Boot wiring (index.ts): construct the MCP manager, load mcp.toml, wire the
 * OAuth state machine to the shared credential store. Missing mcp.toml = MCP
 * empty; invalid mcp.toml = mcp.* methods fail with mcp_config_invalid. */
export function initMcp(): { ok: boolean; error?: string; revision: string } {
	mcpManager = new McpManager({
		configPath: config.mcpConfigPath,
		credentialsPath: `${config.workspaceStateRoot}/mcp-oauth-credentials.json`,
		registrationPath: `${config.workspaceStateRoot}/mcp-oauth-clients.json`,
		defaultCallbackPort: config.mcpOauthCallbackPort,
	});
	oauthManager = new OAuthManager({
		credentials: mcpManager.credentials,
		defaultCallbackPort: config.mcpOauthCallbackPort,
		registrationPath: `${config.workspaceStateRoot}/mcp-oauth-clients.json`,
	});
	mcpManager.setOAuth(oauthManager);
	return mcpManager.reload();
}

export function getMcpManager(): McpManager {
	if (!mcpManager) throw new BridgeError("mcp_unavailable", "mcp manager not initialized");
	return mcpManager;
}

export function getOAuthManager(): OAuthManager {
	if (!oauthManager) throw new BridgeError("mcp_unavailable", "mcp oauth manager not initialized");
	return oauthManager;
}

/** Validate + normalize a client-provided MCP selection (session.create or
 * mcp.select). Returns the normalized selection pinned to the current
 * inventory revision. */
export async function validateMcpSelection(raw: unknown): Promise<McpSessionSelection> {
	if (typeof raw !== "object" || raw === null) throw new BridgeError("invalid_request", "mcpSelection must be an object");
	const selection = raw as McpSessionSelection;
	const result = await getMcpManager().validateSelection(selection);
	if (!result.ok) throw new BridgeError("mcp_selection_invalid", result.error);
	return selection;
}

function writeMcpArtifacts(handle: SessionHandle): void {
	if (!handle.mcpSelection || !mcpManager) return;
	const catalog = buildSessionCatalog(mcpManager, handle.mcpSelection as McpSessionSelection);
	writeSessionMcpCatalog(config.workspaceStateRoot, handle.id, catalog);
}

/** mcp.select: change a session's MCP selection. Only possible while no host
 * is running — the kernel reads the catalog from its spawn environment, so a
 * live host pins the selection it booted with (pi-relay grows selections via
 * idle-only mcp.add; the kernel-mediated port applies changes at respawn). */
export async function selectMcp(sessionId: string, raw: unknown): Promise<{ inventoryRevision: string }> {
	const handle = handles.get(sessionId);
	if (!handle) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	if (handle.host !== null) throw new BridgeError("mcp_selection_locked", "mcp selection is locked while the host is running; stop the session first");
	const selection = await validateMcpSelection(raw);
	handle.mcpSelection = selection;
	writeMcpArtifacts(handle);
	await db.updateSession(sessionId, { mcpSelection: selection });
	emitContractEvent(handle, "mcp.selectionChanged", { selection });
	return { inventoryRevision: selection.inventory_revision };
}

export function onContractEvent(fn: (ev: ContractEvent) => void): () => void {
	listeners.add(fn);
	return () => listeners.delete(fn);
}

function broadcast(ev: ContractEvent): void {
	for (const fn of listeners) {
		try {
			fn(ev);
		} catch {
			/* listener must not take down the supervisor */
		}
	}
}

function withChain<T>(handle: SessionHandle, which: "sendChain" | "eventChain", fn: () => Promise<T>): Promise<T> {
	const next = handle[which].then(fn, fn);
	handle[which] = next.catch(() => {});
	return next;
}

// ---- journal JSONL mirror (per-session audit artifact; PG is authoritative) --

function journalMirror(sessionId: string, obj: Record<string, unknown>): void {
	try {
		mkdirSync(journalDir(), { recursive: true });
		appendFileSync(path.join(journalDir(), `${sessionId}.jsonl`), JSON.stringify({ at: new Date().toISOString(), ...obj }) + "\n");
	} catch {
		/* mirror is best-effort */
	}
}

// ---- event mapping: pi AgentSessionEvent JSON → contract events --------------

const MAX_FIELD = config.eventFieldCap;

function clip(v: unknown): unknown {
	if (typeof v === "string" && v.length > MAX_FIELD) {
		return v.slice(0, MAX_FIELD) + `… [truncated ${v.length - MAX_FIELD} chars]`;
	}
	return v;
}

function mapHostEvent(obj: Record<string, unknown>): Array<{ event: string; data: Record<string, unknown> }> {
	const t = obj.type as string;
	switch (t) {
		case "agent_start":
			return [{ event: "session.state", data: { state: "running", piType: t } }];
		case "agent_settled":
			return [{ event: "session.state", data: { state: "idle", piType: t } }];
		case "thinking_level_changed":
			// M11a: host-side level change (extension cycle, /think) — the event
			// carries only the level; the projection merges it with the model it
			// last saw on session.model.
			return [{ event: "session.model", data: { thinkingLevel: obj.level, piType: t } }];
		case "compaction_start":
			return [{ event: "session.state", data: { state: "compacting", piType: t } }];
		case "compaction_end":
			return [{ event: "session.state", data: { state: "compacted", piType: t } }];
		case "message_start": {
			const m = obj.message as
				| { role?: string; customType?: string; details?: Record<string, unknown>; timestamp?: number }
				| undefined;
			// M11a: custom messages are extension mechanics, not conversation.
			// prime-comms deliveries (customType agent_message) surface as an
			// INBOUND comms.message annotation (the details carry sender +
			// content); every other custom role is suppressed — the transcript
			// must not render generic custom cards.
			if (m?.role === "custom") {
				if (m.customType === "agent_message" && m.details) {
					const d = m.details as {
						from?: { sessionId?: string; name?: string };
						fromRelationship?: string;
						message?: string;
					};
					return [
						{
							event: "comms.message",
							data: {
								direction: "in",
								fromSessionId: d.from?.sessionId ?? null,
								fromName: d.from?.name ?? null,
								role: d.fromRelationship ?? null,
								message: typeof d.message === "string" ? d.message : null,
								deliveryStatus: "delivered",
								ts: typeof m.timestamp === "number" ? new Date(m.timestamp).toISOString() : null,
								piType: t,
							},
						},
					];
				}
				return [];
			}
			return [{ event: "message.delta", data: { kind: "start", role: m?.role, piType: t } }];
		}
		case "message_end": {
			const m = obj.message as { role?: string } | undefined;
			// custom messages: suppressed symmetrically with message_start.
			if (m?.role === "custom") return [];
			return [{ event: "message.delta", data: { kind: "end", role: m?.role, piType: t } }];
		}
		case "message_update": {
			const ame = obj.assistantMessageEvent as Record<string, unknown> | undefined;
			if (!ame) return [];
			const at = ame.type as string;
			if (at === "text_delta" || at === "thinking_delta") {
				return [
					{
						event: "message.delta",
						data: {
							kind: at === "text_delta" ? "text" : "thinking",
							delta: clip(ame.delta),
							contentIndex: ame.contentIndex,
							piType: t,
						},
					},
				];
			}
			if (at === "toolcall_end") {
				return [{ event: "message.delta", data: { kind: "toolcall", toolCall: clip(JSON.stringify(ame.toolCall)), piType: t } }];
			}
			return [];
		}
		case "tool_execution_start":
			return [
				{
					event: "tool.exec",
					data: {
						phase: "start",
						toolName: obj.toolName,
						toolCallId: obj.toolCallId,
						args: clip(JSON.stringify(obj.args ?? null)),
						piType: t,
					},
				},
			];
		case "tool_execution_update":
			return [
				{
					event: "tool.exec",
					data: {
						phase: "update",
						toolName: obj.toolName,
						toolCallId: obj.toolCallId,
						partialResult: clip(JSON.stringify(obj.partialResult ?? null)),
						piType: t,
					},
				},
			];
		case "tool_execution_end":
			return [
				{
					event: "tool.exec",
					data: {
						phase: "end",
						toolName: obj.toolName,
						toolCallId: obj.toolCallId,
						isError: obj.isError ?? false,
						result: clip(JSON.stringify(obj.result ?? null)),
						piType: t,
					},
				},
			];
		case "entry_appended": {
			const entry = obj.entry as { customType?: string; data?: unknown } | undefined;
			if (entry?.customType === "rlm_child_lifecycle") {
				return [{ event: "subagent.lifecycle", data: { ...(entry.data as Record<string, unknown>), piType: t } }];
			}
			// M9: repl console events (prime-rlm appendEntry pipe). Data is
			// pre-capped host-side (code echo 16KiB, text chunks 16KiB, display
			// 3MiB); spread through untouched.
			if (entry?.customType === "repl_cell" && entry.data && typeof entry.data === "object") {
				return [{ event: "repl.cell", data: { ...(entry.data as Record<string, unknown>), piType: t } }];
			}
			if (entry?.customType === "repl_output" && entry.data && typeof entry.data === "object") {
				return [{ event: "repl.output", data: { ...(entry.data as Record<string, unknown>), piType: t } }];
			}
			// M11a: prime-comms delivery notifications (one appendEntry per
			// terminal status in notifyMessageListeners). Data is the
			// CommsMessageEvent — spread through untouched like repl.*.
			if (entry?.customType === "prime_comms_message" && entry.data && typeof entry.data === "object") {
				return [{ event: "comms.message", data: { ...(entry.data as Record<string, unknown>), piType: t } }];
			}
			return [];
		}
		case "extension_error":
			return [
				{
					event: "session.error",
					data: {
						code: "extension_error",
						extensionPath: obj.extensionPath,
						hostEvent: obj.event,
						message: clip(String(obj.error ?? "")),
						piType: t,
					},
				},
			];
		default:
			return [];
	}
}

// ---- contract event emission (spool then broadcast) ---------------------------

async function emitContractEvent(handle: SessionHandle, event: string, data: Record<string, unknown>): Promise<ContractEvent> {
	return withChain(handle, "eventChain", async () => {
		const seq = ++handle.eventSeq;
		const ev: ContractEvent = { event, sessionId: handle.id, seq, data, at: new Date().toISOString() };
		try {
			await db.spoolInsert(handle.id, seq, event, ev.data);
		} catch (err) {
			// PG loss is fatal for stream integrity; surface loudly but keep serving live frames.
			console.error(`[supervisor] spool insert failed for ${handle.id}#${seq}:`, err);
		}
		if (event === "repl.cell") noteReplCellEvent(handle.id, data);
		broadcast(ev);
		return ev;
	});
}

// ---- M9: repl console runtime (event-sourced reducer over repl.cell events) --------

interface ReplRuntime {
	/** cellId -> latest status ("queued" | "running" | "done" | "error"). */
	cells: Map<string, string>;
	active: string | null;
	queued: Set<string>;
}

const replRuntimes = new Map<string, ReplRuntime>();

function replRuntimeFor(sessionId: string): ReplRuntime {
	let rt = replRuntimes.get(sessionId);
	if (!rt) {
		rt = { cells: new Map(), active: null, queued: new Set() };
		replRuntimes.set(sessionId, rt);
	}
	return rt;
}

function noteReplCellEvent(sessionId: string, data: Record<string, unknown>): void {
	const cellId = typeof data.cell_id === "string" ? data.cell_id : "";
	if (!cellId) return;
	const rt = replRuntimeFor(sessionId);
	const status = typeof data.status === "string" ? data.status : "";
	rt.cells.set(cellId, status);
	if (status === "queued") rt.queued.add(cellId);
	else if (status === "running") {
		rt.queued.delete(cellId);
		rt.active = cellId;
	} else if (status === "done" || status === "error") {
		rt.queued.delete(cellId);
		if (rt.active === cellId) rt.active = null;
		// bound memory: drop oldest finished cells beyond 256 tracked
		if (rt.cells.size > 256) {
			for (const [id, s] of rt.cells) {
				if (rt.cells.size <= 256) break;
				if (s === "done" || s === "error") rt.cells.delete(id);
			}
		}
	}
}

/** Emit terminal error events for cells left unfinished by a crash/restart. */
async function terminateUnfinishedReplCells(handle: SessionHandle, ename: string, evalue: string): Promise<void> {
	const rt = replRuntimes.get(handle.id);
	if (!rt) return;
	const unfinished = [...rt.cells.entries()].filter(([, s]) => s === "queued" || s === "running");
	for (const [cellId] of unfinished) {
		await emitContractEvent(handle, "repl.cell", {
			cell_id: cellId,
			status: "error",
			finished_at: new Date().toISOString(),
			error: { ename, evalue },
		});
	}
}

function replRuntimeSnapshot(sessionId: string): Record<string, unknown> {
	const rt = replRuntimes.get(sessionId);
	return {
		active_cell: rt?.active ?? null,
		queue_depth: rt?.queued.size ?? 0,
		queued_cells: rt ? [...rt.queued] : [],
	};
}

// ---- M9: repl.execute — forward to the session host as a sentinel rpc prompt --------

const REPL_SENTINEL = "\u0001prime-repl:";

/** Forward a console cell to the session host. NOT journaled: repl.execute is
 * not an agent command — a host-down cell fails typed instead of queueing for
 * respawn (the kernel-side queue is the ordering point). */
export async function replExecute(
	sessionId: string,
	cellId: string,
	code: string,
	clientCellId?: string,
): Promise<{ accepted: true; cell_id: string }> {
	const handle = handles.get(sessionId);
	if (!handle) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	if (handle.state === "closed") throw new BridgeError("session_not_found", "session is closed");
	if (!handle.host?.alive) {
		throw new BridgeError("host_down", `session host is not running (state: ${handle.state})`, { state: handle.state });
	}
	if (!handle.replReady) {
		throw new BridgeError("repl_unavailable", "session host has not reported repl capability (prime-rlm not ready)");
	}
	const payload = JSON.stringify({ v: 1, op: "execute", cell_id: cellId, code, ...(clientCellId ? { client_cell_id: clientCellId } : {}) });
	let resp: Record<string, unknown>;
	try {
		resp = await handle.host.send({ type: "prompt", message: REPL_SENTINEL + payload }, "repl_exec");
	} catch (err) {
		throw new BridgeError("host_down", `repl delivery failed: ${String(err instanceof Error ? err.message : err)}`, {
			state: handle.state,
		});
	}
	if (!resp.success) {
		const msg = String(resp.error ?? "host rejected repl.execute");
		if (msg.includes("Cannot submit a prompt")) {
			throw new BridgeError("session_busy", msg, { reason: "compacting" });
		}
		throw new BridgeError("host_error", msg);
	}
	return { accepted: true, cell_id: cellId };
}

// ---- session lifecycle ---------------------------------------------------------

export async function createSession(opts: {
	cwd?: string;
	name?: string;
	projectId?: string;
	workspaces?: WorkspaceDecl[];
	mcpSelection?: unknown;
	/** M11b: optional client-chosen session id (legacy startSession pre-chooses
	 * the id it navigates to). Must be a fresh UUID; an existing id is a
	 * conflict (the route maps same-idempotency-key repeats to replay before
	 * this runs). */
	sessionId?: string;
	/** M11a: "provider/modelId" — applied via rpc set_model right after spawn,
	 * before the session row exists; a failure cleans up and throws typed
	 * (model_not_found / model_unavailable), leaving NO orphan session. */
	model?: string;
}): Promise<{ sessionId: string; state: string; model?: { provider: string; modelId: string } }> {
	// M8: workspaces come from the explicit decls or the project's defaults.
	let projectId: string | null = null;
	let decls: WorkspaceDecl[] | undefined = opts.workspaces;
	if (opts.projectId) {
		const project = await db.getProject(opts.projectId);
		if (!project) throw new BridgeError("project_not_found", `unknown project ${opts.projectId}`);
		projectId = project.id;
		if (!decls || decls.length === 0) decls = (project.workspaces as WorkspaceDecl[]) ?? [];
	}
	// The bridge generates the session id up front and pins it via pi's
	// --session-id flag: bridge id === pi sessionId from birth, which also keys
	// the prime-rlm kernel snapshot dir (<sessionDir>/prime/<sessionId>/kernel).
	let sessionId: string;
	if (opts.sessionId !== undefined) {
		// Filename-safe (the id embeds in the pi session file name) + bounded.
		if (!/^[A-Za-z0-9_.-]{1,128}$/.test(opts.sessionId)) throw new BridgeError("bad_request", "sessionId must be filename-safe ([A-Za-z0-9_.-], <=128 chars)");
		if (handles.has(opts.sessionId) || (await db.getSession(opts.sessionId))) {
			throw new BridgeError("idempotency_conflict", `session id ${opts.sessionId} already exists`);
		}
		sessionId = opts.sessionId;
	} else {
		sessionId = randomUUID();
	}
	// M8 phase 2: validate the MCP selection BEFORE any host spawn; artifacts
	// (catalog + skills dir) are written below once the handle exists.
	if (opts.mcpSelection) {
		await validateMcpSelection(opts.mcpSelection);
	}
	// M8: managed workspaces → materialize the session subvolume; the host cwd
	// is the session cwd. Unmanaged (legacy) sessions keep the M5 cwd behavior.
	let cwd = opts.cwd ?? config.defaultCwd;
	let workspaces: SessionWorkspace[] = [];
	if (decls && decls.length > 0) {
		const mat = await materializeForSession(sessionId, projectId ?? "default", decls);
		cwd = mat.cwd;
		workspaces = mat.workspaces;
	} else {
		mkdirSync(cwd, { recursive: true });
	}
	const handle: SessionHandle = {
		id: sessionId,
		cwd,
		name: opts.name ?? null,
		sessionFile: null,
		state: "starting",
		projectId,
		workspaces,
		mcpSelection: opts.mcpSelection ?? null,
		generation: 0,
		host: null,
		eventSeq: 0,
		intentionalStop: false,
		sendChain: Promise.resolve(),
		eventChain: Promise.resolve(),
	};
	handles.set(sessionId, handle);
	if (handle.mcpSelection) {
		try {
			writeMcpArtifacts(handle);
		} catch (err) {
			handles.delete(sessionId);
			if (workspaces.length > 0) await destroyForSession(sessionId).catch(() => {});
			throw new BridgeError("mcp_selection_invalid", `failed to write mcp artifacts: ${String(err)}`);
		}
	}
	const cleanup = async () => {
		handles.delete(sessionId);
		try {
			await handle.host?.stop(1500);
		} catch {
			/* best effort */
		}
		if (workspaces.length > 0) {
			await destroyForSession(sessionId).catch(() => {});
		}
		if (handle.mcpSelection) removeSessionMcpArtifacts(config.workspaceStateRoot, sessionId);
	};
	let stateResp: Record<string, unknown>;
	try {
		await spawnHost(handle, undefined);
		// confirm identity + capture the planned session file path
		stateResp = await handle.host!.send({ type: "get_state" }, "get_state", config.spawnTimeoutMs);
	} catch (err) {
		await cleanup();
		throw err instanceof BridgeError ? err : new BridgeError("host_error", `host spawn failed: ${String(err)}`);
	}
	if (!stateResp.success) {
		await cleanup();
		throw new BridgeError("host_error", `get_state failed: ${String(stateResp.error)}`);
	}
	const st = stateResp.data as { sessionId: string; sessionFile?: string };
	if (st.sessionId !== sessionId) {
		await cleanup();
		throw new BridgeError("host_error", `session id mismatch: expected ${sessionId}, host reports ${st.sessionId}`);
	}
	// M11a: apply the requested model while the session is still pre-insert:
	// a set_model failure cleans up completely (no orphan session row/host).
	if (opts.model) {
		const slash = opts.model.indexOf("/");
		if (slash <= 0 || slash === opts.model.length - 1) {
			await cleanup();
			throw new BridgeError("bad_request", `params.model must be "provider/modelId", got ${JSON.stringify(opts.model)}`);
		}
		const provider = opts.model.slice(0, slash);
		const modelId = opts.model.slice(slash + 1);
		const setResp = await handle.host!.send({ type: "set_model", provider, modelId }, "set_model");
		if (!setResp.success) {
			const msg = String(setResp.error ?? "set_model failed");
			await cleanup();
			throw mapModelRpcError(msg);
		}
	}
	handle.sessionFile = st.sessionFile ?? null;
	await db.insertSession({
		id: handle.id,
		cwd,
		sessionFile: handle.sessionFile,
		name: handle.name,
		hostPid: handle.host!.pid,
		hostGeneration: handle.generation,
		projectId: handle.projectId,
		workspaces: handle.workspaces,
		mcpSelection: handle.mcpSelection,
	});
	handle.state = "idle";
	await db.updateSession(handle.id, { state: "idle" });
	await emitContractEvent(handle, "session.state", { state: "idle", initial: true });
	// M11a: surface the applied model on the wire + in the result.
	if (opts.model) {
		const ms = (await liveModelState(handle)) ?? {};
		await emitSessionModel(handle, ms);
		const slash = opts.model.indexOf("/");
		return {
			sessionId: handle.id,
			state: handle.state,
			model: { provider: ms.provider ?? opts.model.slice(0, slash), modelId: ms.modelId ?? opts.model.slice(slash + 1) },
		};
	}
	return { sessionId: handle.id, state: handle.state };
}

async function spawnHost(handle: SessionHandle, sessionFile: string | null | undefined): Promise<void> {
	// M8: managed sessions must still own their workspace tree (port of
	// ensure_session) before the host is pointed at it.
	if (handle.workspaces.length > 0) {
		await ensureForSession(handle.id, handle.workspaces);
	}
	handle.generation += 1;
	const host = new HostProcess(handle.id, handle.generation);
	handle.host = host;
	wireHost(handle, host);
	const resumeFile = sessionFile && existsSync(sessionFile) ? sessionFile : null;
	await host.start({ cwd: handle.cwd, sessionFile: resumeFile, extraEnv: sessionEnv(handle) });
}

/** M8: per-session environment for the host (and, through it, the kernel):
 * workspace dir names for multi-dir AGENTS.md, MCP catalog/credentials paths
 * and the generated per-session skills dir (written by mcp.ts on selection).
 * M10b: PI_CACHE_RETENTION when BRIDGE_PI_CACHE_RETENTION is configured.
 * Exported for unit tests. */
export function sessionEnv(handle: SessionHandle): Record<string, string> {
	const env: Record<string, string> = {};
	if (config.piCacheRetention) {
		env.PI_CACHE_RETENTION = config.piCacheRetention;
	}
	if (handle.workspaces.length > 0) {
		env.PI_RELAY_WORKSPACE_DIRS = handle.workspaces.map((w) => w.workspaceDir).join(",");
	}
	if (handle.mcpSelection) {
		const mcpDir = `${config.workspaceStateRoot}/sessions/${handle.id}/mcp`;
		env.PRIME_HARNESS_EXTRA_SKILLS_DIRS = `${mcpDir}/skills`;
		env.PI_RELAY_MCP_CATALOG = `${mcpDir}/catalog.json`;
		env.PI_RELAY_MCP_CREDENTIALS = `${config.workspaceStateRoot}/mcp-oauth-credentials.json`;
	}
	return env;
}

function wireHost(handle: SessionHandle, host: HostProcess): void {
	handle.replReady = false;
	host.onEvent = (obj) => {
		// M9: extension capability marker — control frame, not a contract event.
		if (obj.type === "entry_appended") {
			const entry = obj.entry as { customType?: string; data?: unknown } | undefined;
			if (entry?.customType === "prime_rlm_ready") {
				const data = (entry.data ?? {}) as { features?: unknown };
				if (Array.isArray(data.features) && data.features.includes("repl")) {
					handle.replReady = true;
				}
				return;
			}
		}
		const mapped = mapHostEvent(obj);
		for (const m of mapped) {
			// keep handle/PG state roughly in step with the stream
			if (m.event === "session.state") {
				const s = m.data.state as string;
				if (s === "running" || s === "idle" || s === "compacting") {
					handle.state = s;
					void db.updateSession(handle.id, { state: s }).catch(() => {});
					if (s === "idle") void maybeAutoTitle(handle);
				}
			}
			void emitContractEvent(handle, m.event, m.data).catch(() => {});
		}
	};
	host.onExit = (exit) => {
		if (handle.host !== host) return; // stale generation
		handle.host = null;
		if (handle.intentionalStop || shuttingDown) return;
		void handleHostCrash(handle, exit).catch((err) => console.error("[supervisor] crash handling failed:", err));
	};
}

async function handleHostCrash(handle: SessionHandle, exit: { code: number | null; signal: string | null }): Promise<void> {
	handle.state = "host_down";
	await db.updateSession(handle.id, { state: "host_down", hostPid: null });
	const maybeLost = await db.journalMarkMaybeLost(handle.id);
	await emitContractEvent(handle, "session.state", { state: "host_exited", code: exit.code, signal: exit.signal });
	await emitContractEvent(handle, "session.error", {
		code: "host_crashed",
		message: `session host exited unexpectedly (code=${exit.code} signal=${exit.signal})`,
		maybeLostQueued: maybeLost,
	});
	// M9: the kernel dies with the host — close out unfinished console cells.
	await terminateUnfinishedReplCells(handle, "HostCrashed", "session host exited while the cell was unfinished");
	if (config.respawnDelayMs > 0) {
		await new Promise((r) => setTimeout(r, config.respawnDelayMs));
	}
	// a graceful bridge shutdown may have started while we delayed
	if (shuttingDown || handle.intentionalStop) return;
	await respawnHost(handle, "crash");
}

export async function respawnHost(handle: SessionHandle, reason: string): Promise<void> {
	handle.state = "respawning";
	await db.updateSession(handle.id, { state: "respawning" });
	await emitContractEvent(handle, "session.state", { state: "respawning", reason });
	await spawnHost(handle, handle.sessionFile);
	const stateResp = await handle.host!.send({ type: "get_state" }, "get_state", config.spawnTimeoutMs);
	if (stateResp.success) {
		const st = stateResp.data as { sessionId: string; sessionFile?: string };
		if (st.sessionId !== handle.id) {
			// Resume must preserve identity; a mismatch is fatal for this attempt.
			await emitContractEvent(handle, "session.error", {
				code: "host_error",
				message: `respawned host session id mismatch: expected ${handle.id}, got ${st.sessionId}`,
			});
			try {
				await handle.host?.stop(1500);
			} catch {
				/* ignore */
			}
			handle.host = null;
			handle.state = "host_down";
			await db.updateSession(handle.id, { state: "host_down" });
			return;
		}
		if (st.sessionFile) handle.sessionFile = st.sessionFile;
	}
	handle.state = "idle";
	await db.updateSession(handle.id, {
		state: "idle",
		hostPid: handle.host!.pid,
		sessionFile: handle.sessionFile ?? undefined,
		hostGeneration: handle.generation,
	});
	await emitContractEvent(handle, "session.state", {
		state: "host_respawned",
		generation: handle.generation,
		reason,
	});
	// durable queue: deliver everything still pending, oldest first. Runs on the
	// per-session send chain so user commands arriving mid-drain serialize AFTER
	// it; loops because a send can journal itself pending while we drain.
	await withChain(handle, "sendChain", async () => {
		for (;;) {
			if (!handle.host?.alive || shuttingDown) return;
			const pending = await db.journalPending(handle.id);
			if (pending.length === 0) return;
			for (const entry of pending) {
				if (!handle.host?.alive || shuttingDown) return;
				// Pacing: entries that START A RUN (prompt, or follow_up/steer
				// converted to prompt while idle) must wait for the previous
				// drained run to settle — rpc prompt acks at preflight, so
				// back-to-back prompts would fail session_busy and be lost.
				if (entry.kind === "prompt" || effectiveRpcKind(handle, entry.kind) === "prompt") {
					await waitForIdleBeforeRun(handle, DRAIN_IDLE_TIMEOUT_MS);
				}
				await deliverJournalEntry(handle, Number(entry.seq), entry.kind, entry.payload).catch((err) => {
					console.error(`[supervisor] replay of journal ${entry.seq} failed:`, err);
				});
			}
		}
	});
}

// ---- command send path ---------------------------------------------------------

type CommandKind = "prompt" | "steer" | "follow_up" | "abort";

const KIND_TO_RPC: Record<CommandKind, string> = {
	prompt: "prompt",
	steer: "steer",
	follow_up: "follow_up",
	abort: "abort",
};

/** How long the journal drain waits for a session to settle before delivering
 * the next run-starting entry (converted prompts serialize into real turns). */
const DRAIN_IDLE_TIMEOUT_MS = 5 * 60_000;

/** rpc `follow_up`/`steer` on an IDLE host only enqueue — nothing drains the
 * queue until an unrelated prompt arrives (rpc-park). Convert them to `prompt`
 * at delivery time when the host is not running so the command actually starts
 * a turn; while busy they keep their native enqueue semantics. */
function effectiveRpcKind(handle: SessionHandle, kind: string): string {
	if ((kind === "follow_up" || kind === "steer") && handle.state !== "running" && handle.state !== "compacting") {
		return "prompt";
	}
	return KIND_TO_RPC[kind as CommandKind] ?? kind;
}

/** Wait until the session reports idle (agent_settled) so a drained prompt is
 * not rejected with session_busy. Bounded; on timeout the caller delivers
 * anyway and the normal failure path applies. */
async function waitForIdleBeforeRun(handle: SessionHandle, timeoutMs: number): Promise<void> {
	const start = Date.now();
	while (handle.state !== "idle" && Date.now() - start < timeoutMs) {
		if (!handle.host?.alive || shuttingDown || handle.state === "closed") return;
		await new Promise((r) => setTimeout(r, 250));
	}
	if (handle.state !== "idle") {
		console.warn(`[supervisor] drain pacing timed out waiting for idle on ${handle.id}; delivering anyway`);
	}
}

async function deliverJournalEntry(
	handle: SessionHandle,
	journalSeq: number,
	kind: string,
	payload: { text?: string },
): Promise<Record<string, unknown>> {
	const host = handle.host;
	if (!host || !host.alive) throw new BridgeError("host_unavailable", "session host is not running");
	const rpcType = effectiveRpcKind(handle, kind);
	journalMirror(handle.id, { journalSeq, kind, rpcType, sent: true, generation: handle.generation });
	try {
		const cmd = rpcType === "abort" ? { type: "abort" } : { type: rpcType, message: payload.text ?? "" };
		const resp = await host.send(cmd, rpcType === kind ? kind : `${kind}->prompt`);
		if (resp.success) {
			if (rpcType === "prompt") {
				// A prompt starts a run at preflight ack. Mark running NOW so a
				// follow-on journal entry sees busy semantics instead of also
				// converting to a (rejected) prompt in the agent_start gap.
				handle.state = "running";
				void db.updateSession(handle.id, { state: "running" }).catch(() => {});
			}
			await db.journalSetStatus(journalSeq, "acked");
			journalMirror(handle.id, { journalSeq, acked: true });
			return resp;
		}
		await db.journalSetStatus(journalSeq, "failed", String(resp.error ?? "unknown"));
		journalMirror(handle.id, { journalSeq, acked: false, error: String(resp.error) });
		const msg = String(resp.error ?? "host rejected command");
		if (msg.includes("already processing")) {
			throw new BridgeError("session_busy", msg);
		}
		throw new BridgeError("host_error", msg);
	} catch (err) {
		if (err instanceof BridgeError) throw err;
		// transport failure: leave pending for replay after respawn
		journalMirror(handle.id, { journalSeq, transportError: String(err) });
		throw new BridgeError("host_unavailable", `command delivery uncertain: ${String(err)}`);
	}
}

export async function sendUserCommand(
	sessionId: string,
	kind: CommandKind,
	text: string | undefined,
): Promise<{ seq: number; queued: boolean }> {
	const handle = handles.get(sessionId);
	if (!handle) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	if (handle.state === "closed") throw new BridgeError("session_not_found", "session is closed");
	const journalSeq = await db.journalInsert({
		sessionId,
		hostGeneration: handle.generation,
		kind,
		payload: { text },
	});
	journalMirror(sessionId, { journalSeq, kind, journaled: true, hostAlive: !!handle.host?.alive });
	if (!handle.host?.alive) {
		// host down: pending entry awaits respawn delivery (durable queue)
		maybeRespawnOnDemand(handle); // M11b: fork children + crash orphans reopen on prompt
		return { seq: journalSeq, queued: true };
	}
	return withChain(handle, "sendChain", async () => {
		// host may have died between journalInsert and acquiring the chain
		if (!handle.host?.alive) return { seq: journalSeq, queued: true };
		await deliverJournalEntry(handle, journalSeq, kind, { text });
		return { seq: journalSeq, queued: false };
	});
}

// ---- state / listing -------------------------------------------------------------

export async function getSessionState(sessionId: string): Promise<Record<string, unknown>> {
	const handle = handles.get(sessionId);
	const row = await db.getSession(sessionId);
	if (!row) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	const base: Record<string, unknown> = {
		sessionId,
		state: row.state,
		cwd: row.cwd,
		sessionFile: row.session_file,
		hostGeneration: row.host_generation,
		hostAlive: !!handle?.host?.alive,
		hostPid: handle?.host?.pid ?? null,
		headSeq: Number(row.last_event_seq),
		name: row.name,
		projectId: row.project_id,
		parentSessionId: row.parent_session_id ?? null,
		workspaces: Array.isArray(row.workspaces) ? row.workspaces : [],
		mcpSelection: row.mcp_selection ?? null,
		createdAt: row.created_at,
		updatedAt: row.updated_at,
	};
	if (handle?.host?.alive) {
		try {
			const resp = await handle.host.send({ type: "get_state" }, "get_state");
			if (resp.success) {
				const d = resp.data as Record<string, unknown>;
				base.live = {
					isStreaming: d.isStreaming,
					isCompacting: d.isCompacting,
					messageCount: d.messageCount,
					pendingMessageCount: d.pendingMessageCount,
					sessionName: d.sessionName,
					model: d.model ? `${(d.model as { provider?: string }).provider}/${(d.model as { id?: string }).id}` : null,
					thinkingLevel: typeof d.thinkingLevel === "string" ? d.thinkingLevel : null,
				};
			}
		} catch {
			base.live = null;
		}
	}
	// M11a: when the host is down, surface the last persisted model/thinking
	// level from the session file (pi appends model_change /
	// thinking_level_change on every set) so pickers are not blind.
	if (!base.live) {
		const persisted = modelFromSessionFile(row.session_file);
		if (Object.keys(persisted).length > 0) {
			base.model = {
				provider: (persisted as { provider?: string }).provider ?? null,
				modelId: (persisted as { modelId?: string }).modelId ?? null,
				thinkingLevel: (persisted as { thinkingLevel?: string }).thinkingLevel ?? null,
			};
		}
	}
	// M8 (G2): current-branch transcript so clients past a spool trim can
	// rebuild the projection without the live stream.
	const branch = rebuildBranchTranscript(row.session_file);
	base.transcript = { blocks: branch.blocks };
	// M11b: the real branch-tip entry id (legacy active-leaf semantics — the
	// adapter reports it as active_leaf_id when the branch has no renderable
	// blocks, e.g. after switching to the first user-message boundary).
	base.branchTipId = branch.tipId;
	// M11b: snapshot-consistency invariant — headSeq read AFTER the transcript
	// build (from the in-memory emitter counter when a handle exists). Every
	// event whose effects the file read could include has seq <= headSeq, so a
	// client attaching with fromSeq=headSeq never re-receives a completed,
	// already-transcribed message, and never misses one appended later.
	base.headSeq = handle?.eventSeq ?? Number(row.last_event_seq);
	// M9: repl console runtime (event-sourced from repl.cell spool events).
	base.repl = replRuntimeSnapshot(sessionId);
	return base;
}

// ---- M11a: model surface (set_model / set_thinking_level passthrough) ----------------

/** Map upstream set_model/set_thinking_level rpc error strings to typed
 * bridge errors. Upstream messages (rpc-mode.js): "Model not found: p/m"
 * (not in the available snapshot) and "No API key for p/m" (session.setModel
 * checkAuth). Anything else is a host error. */
function mapModelRpcError(message: string): BridgeError {
	if (/^model not found:/i.test(message)) return new BridgeError("model_not_found", message);
	if (/^no api key for/i.test(message)) return new BridgeError("model_unavailable", message);
	return new BridgeError("host_error", message);
}

interface LiveModelState {
	provider?: string;
	modelId?: string;
	name?: string;
	thinkingLevel?: string;
}

/** get_state reconcile: the source of truth for the current model + EFFECTIVE
 * thinking level (set_thinking_level clamps silently upstream). */
async function liveModelState(handle: SessionHandle): Promise<LiveModelState | null> {
	if (!handle.host?.alive) return null;
	try {
		const resp = await handle.host.send({ type: "get_state" }, "get_state");
		if (!resp.success) return null;
		const d = resp.data as Record<string, unknown>;
		const m = d.model as { provider?: string; id?: string; name?: string } | null | undefined;
		return {
			provider: m?.provider,
			modelId: m?.id,
			name: m?.name,
			thinkingLevel: typeof d.thinkingLevel === "string" ? d.thinkingLevel : undefined,
		};
	} catch {
		return null;
	}
}

/** Emit the session.model contract event (spooled + broadcast). Pi emits no
 * wire event for set_model (model_select is extension-runner-only), so the
 * bridge emits after a get_state reconcile; UIs re-read or merge. */
async function emitSessionModel(handle: SessionHandle, ms: LiveModelState): Promise<void> {
	await emitContractEvent(handle, "session.model", {
		provider: ms.provider ?? null,
		modelId: ms.modelId ?? null,
		name: ms.name ?? null,
		thinkingLevel: ms.thinkingLevel ?? null,
	});
}

/** Re-emit session.state so clients watching only the state family refresh. */
async function reemitSessionState(handle: SessionHandle): Promise<void> {
	await emitContractEvent(handle, "session.state", { state: handle.state });
}

export async function setSessionModel(
	sessionId: string,
	provider: string,
	modelId: string,
): Promise<{ provider: string; modelId: string; name: string | null; thinkingLevel: string | null }> {
	const handle = handles.get(sessionId);
	const row = await db.getSession(sessionId);
	if (!row) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	if (!handle?.host?.alive) {
		throw new BridgeError("host_unavailable", `session ${sessionId} host is not running; model is set per live host (pi persists the choice for the next respawn once applied)`);
	}
	const resp = await handle.host.send({ type: "set_model", provider, modelId }, "set_model");
	if (!resp.success) throw mapModelRpcError(String(resp.error ?? "set_model failed"));
	// Reconcile: pi persists (session model_change entry + settings.json
	// default) and setThinkingLevel may have been clamped to the new model.
	const ms = (await liveModelState(handle)) ?? {};
	await emitSessionModel(handle, ms);
	await reemitSessionState(handle);
	return {
		provider: ms.provider ?? provider,
		modelId: ms.modelId ?? modelId,
		name: ms.name ?? null,
		thinkingLevel: ms.thinkingLevel ?? null,
	};
}

export async function setSessionThinkingLevel(
	sessionId: string,
	level: string,
): Promise<{ thinkingLevel: string | null; availableLevels: string[] }> {
	const handle = handles.get(sessionId);
	const row = await db.getSession(sessionId);
	if (!row) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	if (!handle?.host?.alive) {
		throw new BridgeError("host_unavailable", `session ${sessionId} host is not running`);
	}
	const resp = await handle.host.send({ type: "set_thinking_level", level }, "set_thinking_level");
	if (!resp.success) throw mapModelRpcError(String(resp.error ?? "set_thinking_level failed"));
	// Upstream clamps unsupported levels silently; get_state reports the
	// EFFECTIVE level and get_available_thinking_levels the model's menu.
	const ms = (await liveModelState(handle)) ?? {};
	let availableLevels: string[] = [];
	try {
		const lv = await handle.host.send({ type: "get_available_thinking_levels" }, "get_available_thinking_levels");
		if (lv.success) availableLevels = ((lv.data as { levels?: string[] })?.levels ?? []).map(String);
	} catch {
		/* best effort */
	}
	await emitSessionModel(handle, ms);
	return { thinkingLevel: ms.thinkingLevel ?? null, availableLevels };
}

/** Session-file fallback for the current model/thinking level when the host
 * is down: last model_change / thinking_level_change entries (pi persists
 * them on every set). */
export function modelFromSessionFile(sessionFile: string | null): { provider: string; modelId: string } | { thinkingLevel: string } | Record<string, never> {
	if (!sessionFile) return {};
	let raw: string;
	try {
		raw = readFileSync(sessionFile, "utf8");
	} catch {
		return {};
	}
	let provider: string | undefined;
	let modelId: string | undefined;
	let thinkingLevel: string | undefined;
	for (const line of raw.split("\n")) {
		if (!line.includes('"type"')) continue;
		let o: { type?: string; provider?: string; modelId?: string; thinkingLevel?: string };
		try {
			o = JSON.parse(line);
		} catch {
			continue;
		}
		if (o.type === "model_change" && o.provider && o.modelId) {
			provider = o.provider;
			modelId = o.modelId;
		} else if (o.type === "thinking_level_change" && o.thinkingLevel) {
			thinkingLevel = o.thinkingLevel;
		}
	}
	const out: Record<string, unknown> = {};
	if (provider && modelId) {
		out.provider = provider;
		out.modelId = modelId;
	}
	if (thinkingLevel) out.thinkingLevel = thinkingLevel;
	return out as { provider: string; modelId: string } | { thinkingLevel: string } | Record<string, never>;
}

export async function listSessionStates(): Promise<Array<Record<string, unknown>>> {
	const rows = await db.listSessions();
	return rows.map((r) => ({
		sessionId: r.id,
		state: r.state,
		cwd: r.cwd,
		name: r.name,
		projectId: r.project_id,
		parentSessionId: r.parent_session_id ?? null,
		workspaces: Array.isArray(r.workspaces) ? r.workspaces : [],
		hostGeneration: r.host_generation,
		hostAlive: !!handles.get(r.id)?.host?.alive,
		headSeq: Number(r.last_event_seq),
		createdAt: r.created_at,
		updatedAt: r.updated_at,
	}));
}


// ---- M11b: fork / switch / fork points / commands / compact ----------------------

/** On-demand respawn for hostless sessions (fork parents/children, crash
 * orphans). Journaled commands drain after respawn via the normal path. */
function maybeRespawnOnDemand(handle: SessionHandle): void {
	if (handle.respawnInflight || shuttingDown || handle.intentionalStop) return;
	if (handle.host?.alive) return;
	if (handle.state === "closed" || handle.state === "respawning" || handle.state === "starting") return;
	if (!handle.sessionFile) return; // nothing to resume from (pre-first-flush)
	handle.respawnInflight = true;
	void respawnHost(handle, "on_demand")
		.catch((err) => {
			console.error(`[supervisor] on-demand respawn of ${handle.id} failed:`, err);
		})
		.finally(() => {
			handle.respawnInflight = false;
		});
}

function requireForkableHandle(sessionId: string): SessionHandle {
	const handle = handles.get(sessionId);
	if (!handle) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	if (handle.state === "closed") throw new BridgeError("session_not_found", "session is closed");
	if (handle.state === "running" || handle.state === "compacting" || handle.state === "starting" || handle.state === "respawning") {
		throw new BridgeError("session_busy", `session ${sessionId} is ${handle.state}; fork/switch requires an idle session file`);
	}
	if (!handle.sessionFile || !existsSync(handle.sessionFile)) {
		throw new BridgeError("bad_request", "session has no persisted transcript to branch from yet");
	}
	return handle;
}

export interface ForkSessionResult {
	newSessionId: string;
	name: string | null;
	workspaceSnapshot: { managed: boolean; btrfs: boolean | null; cwd: string } | null;
}

/** pi-relay /fork semantics: duplicate the session at its current state (or at
 * entryId when given). Branch-file surgery (header id = new bridge session id)
 * + workspace-lib btrfs snapshot for managed sessions. The parent keeps its
 * host; the child starts hostless and respawns on demand (first prompt). */
export async function forkSession(sessionId: string, entryId?: string): Promise<ForkSessionResult> {
	const parent = requireForkableHandle(sessionId);
	let records = readSessionRecords(parent.sessionFile);
	let sourceFile = parent.sessionFile!;
	let leafId: string | null;
	if (entryId !== undefined) {
		// explicit fork-at-entry may target an off-branch entry in a sibling
		// branch file — resolve across the session's files.
		const found = findEntryFile(parent.sessionFile!, entryId);
		if (!found) throw new BridgeError("bad_request", `unknown entry ${entryId}`);
		sourceFile = found;
		records = readSessionRecords(found);
		leafId = entryId;
	} else {
		leafId = lastLeafId(records);
	}
	const childId = randomUUID();
	// Workspace snapshot FIRST: if it fails nothing else has happened.
	let childCwd = parent.cwd;
	let childWorkspaces: SessionWorkspace[] = [];
	let snapshot: ForkSessionResult["workspaceSnapshot"] = null;
	if (parent.workspaces.length > 0) {
		const mat = await forkForSession(parent.id, parent.workspaces, childId);
		childCwd = mat.cwd;
		childWorkspaces = mat.workspaces;
		snapshot = { managed: true, btrfs: mat.subvolume, cwd: mat.cwd };
	}
	const targetFile = branchFilePath(sessionsDir(), childId);
	try {
		writeBranchFile({
			sourceFile,
			targetFile,
			leafId,
			sessionId: childId,
			parentSessionFile: parent.sessionFile,
		});
	} catch (err) {
		if (snapshot) await destroyForSession(childId).catch(() => {});
		throw new BridgeError("fork_failed", `failed to write branch file: ${String(err)}`);
	}
	const childName = parent.name ? `${parent.name} (fork)` : null;
	await db.insertSession({
		id: childId,
		cwd: childCwd,
		sessionFile: targetFile,
		name: childName,
		hostPid: null,
		projectId: parent.projectId,
		parentSessionId: parent.id,
		workspaces: childWorkspaces,
		mcpSelection: parent.mcpSelection ?? undefined,
	});
	await db.updateSession(childId, { state: "idle" });
	const childHandle: SessionHandle = {
		id: childId,
		cwd: childCwd,
		name: childName,
		sessionFile: targetFile,
		state: "idle",
		projectId: parent.projectId,
		workspaces: childWorkspaces,
		mcpSelection: parent.mcpSelection,
		generation: 0,
		host: null,
		eventSeq: 0,
		intentionalStop: false,
		sendChain: Promise.resolve(),
		eventChain: Promise.resolve(),
	};
	handles.set(childId, childHandle);
	await emitContractEvent(childHandle, "session.state", { state: "idle", initial: true, forkedFrom: parent.id });
	await emitContractEvent(parent, "session.forked", {
		newSessionId: childId,
		entryId: entryId ?? null,
		leafId,
		workspaceSnapshot: snapshot,
	});
	return { newSessionId: childId, name: childName, workspaceSnapshot: snapshot };
}

export interface SwitchSessionResult {
	sessionId: string;
	/** New branch tip (parent of the switched-at user message); null = empty branch. */
	activeLeafId: string | null;
	/** Text of the user message branched before — the legacy edit-prefill idiom. */
	selectedText: string;
}

/** pi-relay /switch semantics: branch navigation at USER-MESSAGE boundaries
 * (owner rewind rule). The session row keeps its identity (same id in the new
 * file's header); the row's session_file flips to the branch and a live host
 * is restarted onto it. The abandoned branch file stays on disk. */
export async function switchSession(sessionId: string, entryId: string): Promise<SwitchSessionResult> {
	const handle = requireForkableHandle(sessionId);
	// The target may live in ANY branch file (e.g. switching forward again
	// after switching back) — resolve across siblings and copy from there.
	const sourceFile = findEntryFile(handle.sessionFile!, entryId);
	if (!sourceFile) throw new BridgeError("bad_request", `unknown entry ${entryId}`);
	const target = readSessionRecords(sourceFile).find((r) => r.id === entryId && r.type !== "session");
	if (!target) throw new BridgeError("bad_request", `unknown entry ${entryId}`);
	if (target.type !== "message" || target.message?.role !== "user") {
		throw new BridgeError("bad_request", "switch targets must be user messages (getForkPoints lists the valid boundaries)");
	}
	const leafId = typeof target.parentId === "string" ? target.parentId : null;
	const selectedText = userMessageText(target);
	const targetFile = branchFilePath(sessionsDir(), sessionId);
	try {
		writeBranchFile({
			sourceFile,
			targetFile,
			leafId,
			sessionId, // same id: the session keeps its identity across branches
			parentSessionFile: handle.sessionFile,
		});
	} catch (err) {
		throw new BridgeError("switch_failed", `failed to write branch file: ${String(err)}`);
	}
	const hadLiveHost = !!handle.host?.alive;
	handle.sessionFile = targetFile;
	await db.updateSession(sessionId, { sessionFile: targetFile });
	if (hadLiveHost) {
		// Restart the host onto the branch file. Suppress crash handling for the
		// deliberate stop; respawnHost's identity check passes (header id == id).
		handle.intentionalStop = true;
		try {
			await handle.host?.stop(2000);
		} catch {
			/* best effort */
		}
		handle.host = null;
		handle.intentionalStop = false;
		await respawnHost(handle, "switch");
	}
	await emitContractEvent(handle, "session.switched", { entryId, activeLeafId: leafId });
	return { sessionId, activeLeafId: leafId, selectedText };
}

/** User-message boundaries for the /switch picker. The session file is the
 * source of truth: only the file parse carries parentId (the switch target
 * leaf) + onActiveBranch, so it is preferred even when the host is live.
 * Falls back to pi's get_fork_messages rpc when the file is unreadable. */
export async function getForkPoints(sessionId: string): Promise<{ sessionId: string; points: unknown[] }> {
	const handle = handles.get(sessionId);
	const row = await db.getSession(sessionId);
	if (!row) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	if (row.session_file) {
		try {
			// Aggregate across the session's branch files: /switch must offer
			// EVERY user-message boundary (off-branch ones flagged), not just
			// the active branch's.
			const { records, activeBranchIds } = readAllBranchRecords(row.session_file);
			const points = userMessageBoundaries(records, null).map((p) => ({
				...p,
				onActiveBranch: activeBranchIds.has(p.entryId),
			}));
			return { sessionId, points };
		} catch {
			/* fall through to the live rpc */
		}
	}
	if (handle?.host?.alive) {
		try {
			const resp = await handle.host.send({ type: "get_fork_messages" }, "get_fork_messages");
			if (resp.success) {
				const msgs = ((resp.data as { messages?: Array<{ entryId?: string; text?: string }> }).messages ?? [])
					.filter((m) => typeof m.entryId === "string")
					.map((m) => ({ entryId: m.entryId, preview: String(m.text ?? ""), live: true }));
				return { sessionId, points: msgs };
			}
		} catch {
			/* fall through */
		}
	}
	return { sessionId, points: [] };
}

/** M11b: full text of persisted message entries by id (any branch). The
 * legacy /switch edit idiom restores the boundary user message into the
 * composer; off-branch entries are not in the client-side projection, so the
 * adapter reads them here. Pure read — works host-down. */
export async function getSessionEntryTexts(sessionId: string, ids: string[]): Promise<{ sessionId: string; entries: unknown[] }> {
	const row = await db.getSession(sessionId);
	if (!row) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	if (ids.length === 0) return { sessionId, entries: [] };
	if (ids.length > 64) throw new BridgeError("bad_request", "ids: max 64 entries per call");
	if (!row.session_file) throw new BridgeError("session_file_missing", `session ${sessionId} has no session file yet`);
	const want = new Set(ids);
	const out: { id: string; role: string; text: string; timestamp: string | null }[] = [];
	// Scan every branch file: /switch restore-text lookups routinely hit
	// off-branch entries, which live in sibling branch files after a switch.
	for (const rec of readAllBranchRecords(row.session_file).records) {
		if (!rec.id || !want.has(rec.id)) continue;
		if (rec.type !== "message") continue;
		const role = typeof rec.message?.role === "string" ? rec.message.role : "";
		const text = role === "user" ? userMessageText(rec) : messageText(rec);
		out.push({ id: rec.id, role, text, timestamp: typeof rec.timestamp === "string" ? rec.timestamp : null });
	}
	return { sessionId, entries: out };
}

/** get_commands passthrough (slash discovery). Host must be alive. */
export async function getCommands(sessionId: string): Promise<{ sessionId: string; commands: unknown[] }> {
	const handle = handles.get(sessionId);
	if (!handle) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	if (!handle.host?.alive) throw new BridgeError("host_unavailable", `session ${sessionId} host is not running`);
	const resp = await handle.host.send({ type: "get_commands" }, "get_commands");
	if (!resp.success) throw new BridgeError("host_error", String(resp.error ?? "get_commands failed"));
	return { sessionId, commands: ((resp.data as { commands?: unknown[] }).commands ?? []) };
}

/** Legacy /compact: rpc compact passthrough. Compaction lifecycle streams via
 * the existing compacting/compacted state events; the summary lands in the
 * transcript on the next getState (compaction block). */
export async function compactSession(sessionId: string): Promise<{ accepted: true }> {
	const handle = handles.get(sessionId);
	if (!handle) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	if (!handle.host?.alive) throw new BridgeError("host_unavailable", `session ${sessionId} host is not running`);
	const resp = await handle.host.send({ type: "compact" }, "compact");
	if (!resp.success) {
		const msg = String(resp.error ?? "compact failed");
		if (msg.includes("already processing")) throw new BridgeError("session_busy", msg);
		throw new BridgeError("host_error", msg);
	}
	return { accepted: true };
}

// ---- attach / replay (resumable event stream) ---------------------------------------

export async function replayEvents(
	sessionId: string,
	fromSeq: number,
	deliver: (ev: ContractEvent) => void,
): Promise<{ replayed: number; headSeq: number }> {
	const handle = handles.get(sessionId);
	if (!handle) {
		// session may exist in PG but not yet respawned — still replayable from PG
		const row = await db.getSession(sessionId);
		if (!row) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
		const rows = await db.spoolRead(sessionId, fromSeq);
		for (const r of rows) {
			deliver({ event: r.event, sessionId, seq: Number(r.seq), data: r.payload as Record<string, unknown>, at: r.created_at.toISOString() });
		}
		return { replayed: rows.length, headSeq: Number(row.last_event_seq) };
	}
	// serialize with live emission so replay has no gaps/dupes
	return withChain(handle, "eventChain", async () => {
		const rows = await db.spoolRead(sessionId, fromSeq);
		for (const r of rows) {
			deliver({ event: r.event, sessionId, seq: Number(r.seq), data: r.payload as Record<string, unknown>, at: r.created_at.toISOString() });
		}
		return { replayed: rows.length, headSeq: handle.eventSeq };
	});
}

export async function spoolFloor(sessionId: string): Promise<number> {
	return db.spoolMinSeq(sessionId);
}

export function currentSeq(sessionId: string): number {
	return handles.get(sessionId)?.eventSeq ?? 0;
}

export function hasSession(sessionId: string): boolean {
	return handles.has(sessionId);
}

// ---- session delete / rename (M8) -------------------------------------------------------

/** Full teardown: stop host, destroy the workspace tree (subvolume), drop PG
 * rows (journal + spool cascade). Idempotent. */
export async function deleteSession(sessionId: string): Promise<{ deleted: true }> {
	const handle = handles.get(sessionId);
	if (handle) {
		handle.intentionalStop = true;
		try {
			await handle.host?.stop(2000);
		} catch {
			/* best effort */
		}
		handles.delete(sessionId);
	}
	replRuntimes.delete(sessionId);
	await destroyForSession(sessionId).catch((err) => {
		console.error(`[supervisor] workspace destroy for ${sessionId} failed:`, err);
	});
	removeSessionMcpArtifacts(config.workspaceStateRoot, sessionId);
	await db.deleteSessionRow(sessionId);
	return { deleted: true };
}

/** M8 (G1): sidecar title — first settle with no name → title model → rename. */
async function maybeAutoTitle(handle: SessionHandle): Promise<void> {
	if (handle.titleAttempted || handle.name) return;
	handle.titleAttempted = true;
	const userText = firstUserText(handle.sessionFile);
	if (!userText) { console.error(`[titles] ${handle.id}: no user text (sessionFile=${handle.sessionFile})`); return; }
	// one retry: the shim occasionally hiccups (reasoning tokens, cold model)
	let title = await generateTitle(userText);
	if (!title) {
		await new Promise((r) => setTimeout(r, 2000));
		title = await generateTitle(userText);
	}
	if (!title) { console.error(`[titles] ${handle.id}: generateTitle returned null (after retry)`); return; }
	// rename only if STILL unnamed (a user rename in the meantime wins)
	const row = await db.getSession(handle.id).catch(() => null);
	if (!row || row.name) return;
	await renameSession(handle.id, title);
}

export async function renameSession(sessionId: string, name: string): Promise<{ renamed: true }> {
	const row = await db.getSession(sessionId);
	if (!row) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	await db.updateSession(sessionId, { name });
	const handle = handles.get(sessionId);
	if (handle) {
		handle.name = name;
		// keep the pi host's session name in step (best effort; host may be down)
		void handle.host?.send({ type: "set_session_name", name }, "set_session_name").catch(() => {});
		await emitContractEvent(handle, "session.renamed", { name });
	}
	return { renamed: true };
}

// ---- subagent tree (projection over rlm_child_lifecycle entries) ----------------------

export async function subagentTree(sessionId: string): Promise<Record<string, unknown>> {
	const row = await db.getSession(sessionId);
	if (!row) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	const children = new Map<string, Record<string, unknown>>();
	const absorb = (data: Record<string, unknown>) => {
		const id = String(data.rlm_child_id ?? "");
		if (!id) return;
		const cur =
			children.get(id) ??
			({
				rlmChildId: id,
				name: data.session_name ?? null,
				childSessionId: null,
				depth: 1, // every child recorded in THIS session is one level down
				status: "running",
				createdAt: data.created_at ?? null,
				phases: [] as string[],
			} as Record<string, unknown>);
		if (data.session_id) cur.childSessionId = data.session_id;
		if (data.status) cur.status = data.status;
		const phases = cur.phases as string[];
		const phase = String(data.phase);
		// lifecycle phases never repeat for one child; file+spool double-absorb dedupe
		if (!phases.includes(phase)) phases.push(phase);
		children.set(id, cur);
	};
	// durable source: pi session file (survives spool trim)
	if (row.session_file && existsSync(row.session_file)) {
		const { readFileSync } = await import("node:fs");
		for (const line of readFileSync(row.session_file, "utf8").split("\n")) {
			if (!line.includes("rlm_child_lifecycle")) continue;
			try {
				const obj = JSON.parse(line);
				if (obj.type === "custom" && obj.customType === "rlm_child_lifecycle") absorb(obj.data ?? {});
			} catch {
				/* skip malformed */
			}
		}
	}
	// live source: spool (may carry entries not yet flushed to the file)
	const rows = await db.spoolRead(sessionId, 0);
	for (const r of rows) {
		if (r.event === "subagent.lifecycle") absorb(r.payload as Record<string, unknown>);
	}
	return { sessionId, children: [...children.values()] };
}

// ---- startup reconciliation + shutdown -------------------------------------------------

export async function reconcileOnBoot(): Promise<void> {
	const rows = await db.listSessions();
	for (const row of rows) {
		if (row.state === "closed") continue;
		const handle: SessionHandle = {
			id: row.id,
			cwd: row.cwd,
			name: row.name,
			sessionFile: row.session_file,
			state: row.state,
			projectId: row.project_id,
			workspaces: (Array.isArray(row.workspaces) ? row.workspaces : []) as SessionWorkspace[],
			mcpSelection: row.mcp_selection ?? null,
			generation: row.host_generation,
			host: null,
			eventSeq: Number(row.last_event_seq),
			intentionalStop: false,
			sendChain: Promise.resolve(),
			eventChain: Promise.resolve(),
		};
		handles.set(row.id, handle);
		// M9: rebuild the repl runtime from the durable spool; cells left
		// unfinished across the bridge restart are closed out as errors (the
		// kernel did not survive).
		try {
			const spoolRows = await db.spoolRead(row.id, 0);
			for (const r of spoolRows) {
				if (r.event === "repl.cell") noteReplCellEvent(row.id, r.payload as Record<string, unknown>);
			}
			await terminateUnfinishedReplCells(handle, "BridgeRestarted", "bridge restarted while the cell was unfinished");
		} catch (err) {
			console.error(`[supervisor] repl runtime rebuild for ${row.id} failed:`, err);
		}
	}
	// respawn sequentially to keep boot predictable
	for (const handle of handles.values()) {
		try {
			await respawnHost(handle, "bridge_restart");
		} catch (err) {
			console.error(`[supervisor] respawn of ${handle.id} failed:`, err);
			handle.state = "host_down";
			await db.updateSession(handle.id, { state: "host_down", hostPid: null });
			await emitContractEvent(handle, "session.error", {
				code: "respawn_failed",
				message: String(err instanceof Error ? err.message : err),
			});
		}
	}
}

export async function shutdownSupervisor(): Promise<void> {
	shuttingDown = true;
	for (const handle of handles.values()) {
		handle.intentionalStop = true;
		try {
			await handle.host?.stop(2000);
		} catch {
			/* best effort */
		}
	}
}
