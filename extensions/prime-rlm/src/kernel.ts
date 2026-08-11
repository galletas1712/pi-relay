// M1 port of prime-agent's KernelManager (packages/coding-agent/src/core/kernel/index.ts).
// Stripped: fork-server, namespace snapshots, registerSessionResourceCleanup (pi-ai),
// diff/attachment/agent-message display MIME plumbing. Kept: ZMQ wire protocol,
// connection file bootstrap, serialized execute queue, host-request comm dispatch,
// output truncation, interrupt/abort handling, process-wide cleanup on exit/signals.
import { type ChildProcess, spawn } from "node:child_process";
import { createHmac, randomBytes } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { Dealer, Subscriber } from "zeromq";
import {
	DEFAULT_SNAPSHOT_MAX_BYTES,
	type RestoreResult,
	type SnapshotResult,
	buildListNamesCode,
	buildRestoreCode,
	buildSnapshotCode,
	parseListNamesResult,
	parseRestoreResult,
	parseSnapshotResult,
} from "./state-snapshot.ts";

const DELIM = Buffer.from("<IDS|MSG>");
const PROTOCOL_VERSION = "5.3";
const PORTS_RESOLVE_TIMEOUT_MS = 5000;
const READY_TIMEOUT_MS = 5000;
const DEFAULT_SNAPSHOT_DEBOUNCE_MS = 1500;
const SNAPSHOT_MAX_OUTPUT_CHARS = 1_000_000;
const SNAPSHOT_DISPOSE_TIMEOUT_MS = 5000;
// Loopback PUB/SUB subscription propagation is usually sub-ms, but keep a small guard before first execute.
const IOPUB_SUBSCRIBE_DELAY_MS = 50;
const DEFAULT_MAX_OUTPUT_CHARS = 65536;
const HOST_REQUEST_DISPOSE_TIMEOUT_MS = 5000;
const KERNEL_ABORT_GRACE_MS = 1000;
const KERNEL_BUSY_REUSE_WAIT_MS = 5000;
const KERNEL_BUSY_INTERRUPT_INTERVAL_MS = 500;
const KERNEL_BUSY_AFTER_INTERRUPT_MESSAGE =
	"IPython kernel is still running the previously interrupted cell. Wait and try again, or kill the IPython kernel to start fresh.";

export class KernelBusyAfterInterruptError extends Error {
	constructor() {
		super(KERNEL_BUSY_AFTER_INTERRUPT_MESSAGE);
		this.name = "KernelBusyAfterInterruptError";
	}
}

/** Comm target the kernel-side `prime_rlm_runtime` shim opens for typed host requests. */
export const HOST_COMM_TARGET = "host.request";

/** Handles one typed request from Python code running in the kernel. */
export type HostRequestHandler = (payload: Record<string, unknown>) => Promise<Record<string, unknown>>;

/** Host request handlers keyed by request type (e.g. "rlm.run"). */
export type HostRequestHandlers = Record<string, HostRequestHandler>;

export interface KernelManagerOptions {
	/** Python interpreter that has `ipykernel` available. */
	python: string;
	cwd?: string;
	env?: Record<string, string>;
	sessionId?: string;
	hostHandlers?: HostRequestHandlers;
	/** Persist/revive the user namespace across kernel restarts and session resume. */
	snapshot?: KernelSnapshotConfig;
	/** Default: "prime-rlm". */
	username?: string;
	/** M4 (O2): called for agent-message receipts that arrive AFTER the owning
	 * execution settled (or with no active execution), so the host can append a
	 * session entry. In-execution receipts are reported via ExecuteResult. */
	onLateSentAgentMessage?: (message: KernelSentAgentMessage) => void;
	/** M9: repl-console cell lifecycle/output hook. Called for executions that
	 * carry `ExecuteOptions.cell` meta only — internal cells stay silent. */
	onCellEvent?: (event: KernelCellEvent) => void;
}

/** M9: identity + provenance of a console-visible cell. */
export interface KernelCellMeta {
	/** Unique per session: `u_<client>` for console cells, `m_<toolCallId>` for model cells. */
	cellId: string;
	provenance: "user" | "model";
	toolCallId?: string;
	clientCellId?: string;
}

/** M9: lifecycle/output events for console-visible cells. */
export type KernelCellEvent =
	| { kind: "running"; meta: KernelCellMeta; startedAt: string }
	| { kind: "stream"; meta: KernelCellMeta; name: "stdout" | "stderr"; text: string }
	| { kind: "display"; meta: KernelCellMeta; mimeType: string; data: string }
	| { kind: "result"; meta: KernelCellMeta; text: string }
	| { kind: "error"; meta: KernelCellMeta; ename: string; evalue: string; traceback: string[] }
	| {
			kind: "finished";
			meta: KernelCellMeta;
			status: "ok" | "error" | "aborted";
			durationMs: number;
			finishedAt: string;
			error?: { ename: string; evalue: string };
			stdoutTruncated: boolean;
			stderrTruncated: boolean;
	  };

export interface KernelStartOptions {
	signal?: AbortSignal;
}

export interface ExecuteOptions {
	/** Aborting interrupts the kernel via the control channel. */
	signal?: AbortSignal;
	onStream?: (chunk: string, name: "stdout" | "stderr") => void;
	/** Cap stdout / stderr / result at this many characters. Default 65536. */
	maxOutputChars?: number;
	/** Synthetic host cell; excluded from lastCellCode attribution. */
	internal?: boolean;
	/** M9: console-visible cell identity. When set, the manager emits
	 * KernelCellEvent lifecycle/output events via `onCellEvent`. */
	cell?: KernelCellMeta;
}

/** Where and how to persist the kernel's user namespace so it survives resume. */
export interface KernelSnapshotConfig {
	/** Absolute path for the dill payload. */
	path: string;
	/** Absolute path for the JSON manifest written alongside the payload. */
	manifestPath: string;
	/** Skip variables (and abort the payload) above this many bytes. Default 256 MiB. */
	maxBytes?: number;
	/** Debounce window for the auto-snapshot after a successful execution. Default 1500 ms. */
	debounceMs?: number;
}

export interface ExecuteResult {
	stdout: string;
	stderr: string;
	/** Last `execute_result` payload (text/plain), if the cell produced one. */
	result?: string;
	status: "ok" | "error" | "aborted";
	error?: { ename: string; evalue: string; traceback: string[] };
	durationMs: number;
	/** Diffs emitted via display_data (edit skill), in order. */
	diffs?: KernelDiffDisplay[];
	/** Media attachments emitted via display_data (attach-image skill), in order. */
	attachments?: KernelAttachment[];
	/** Agent-message receipts emitted via display_data, in order. */
	sentAgentMessages?: KernelSentAgentMessage[];
}

// ---- M4: MIME-rich display_data plumbing -----------------------------------
// Ported from prime-agent packages/coding-agent/src/core/kernel/index.ts.

/** MIME tag the `edit` skill emits diff payloads under, via `display_data`. */
export const DIFF_DISPLAY_MIME = "application/vnd.prime-agent.diff+json";

/** MIME tag the `attach-image` skill emits media payloads under, via `display_data`. */
export const ATTACHMENT_DISPLAY_MIME = "application/vnd.prime-agent.attachment+json";

/** MIME tag the `agent-message` skill emits after sending a message. */
export const AGENT_MESSAGE_DISPLAY_MIME = "application/vnd.prime-agent.agent-message+json";

/** Hard ceiling on a single attachment's base64 payload (defensive guard). */
const MAX_ATTACHMENT_DATA_CHARS = 10_000_000;

/** Cap on retained late-sent-agent-message handler request ids. */
const MAX_LATE_SENT_AGENT_MESSAGE_HANDLERS = 64;

/** One file edit, captured from a {@link DIFF_DISPLAY_MIME} display payload. */
export interface KernelDiffDisplay {
	path: string;
	oldStr: string;
	newStr: string;
	/** 1-based line where `oldStr` begins in the file, for absolute line numbers. */
	startLine?: number;
}

/** One media attachment, captured from an {@link ATTACHMENT_DISPLAY_MIME} display payload. */
export interface KernelAttachment {
	mimeType: string;
	/** Base64 payload. */
	data: string;
	path?: string;
}

export interface KernelSentAgentMessageTarget {
	activeSessionId: string;
	sessionId: string;
	sessionName?: string;
}

export interface KernelSentAgentMessage {
	id: string;
	message: string;
	deliveryStatus: "delivered" | "queued";
	receiverRole?: "parent" | "sibling" | "child";
	target: KernelSentAgentMessageTarget;
}

function parseDiffDisplay(payload: unknown): KernelDiffDisplay | undefined {
	if (!isRecord(payload)) {
		return undefined;
	}
	const { path, old_str: oldStr, new_str: newStr, start_line: startLine } = payload;
	if (typeof path !== "string" || typeof oldStr !== "string" || typeof newStr !== "string") {
		return undefined;
	}
	return { path, oldStr, newStr, startLine: typeof startLine === "number" ? startLine : undefined };
}

/**
 * Parse an {@link ATTACHMENT_DISPLAY_MIME} payload. Malformed payloads are
 * tolerantly ignored (`undefined`); a well-formed payload exceeding
 * {@link MAX_ATTACHMENT_DATA_CHARS} is reported as `"oversized"` so the caller
 * can fail the cell loudly rather than silently dropping the image.
 */
function parseAttachmentDisplay(payload: unknown): KernelAttachment | "oversized" | undefined {
	if (!isRecord(payload)) {
		return undefined;
	}
	const { mime_type: mimeType, data, path } = payload;
	if (typeof mimeType !== "string" || typeof data !== "string") {
		return undefined;
	}
	if (data.length > MAX_ATTACHMENT_DATA_CHARS) {
		return "oversized";
	}
	return { mimeType, data, path: typeof path === "string" ? path : undefined };
}

function parseSentAgentMessage(payload: unknown): KernelSentAgentMessage | undefined {
	if (!isRecord(payload) || !isRecord(payload.target)) {
		return undefined;
	}
	const { id, message, deliveryStatus, receiverRole, target } = payload;
	const { activeSessionId, sessionId, sessionName } = target;
	if (
		typeof id !== "string" ||
		typeof message !== "string" ||
		(deliveryStatus !== "delivered" && deliveryStatus !== "queued") ||
		typeof activeSessionId !== "string" ||
		typeof sessionId !== "string"
	) {
		return undefined;
	}
	return {
		id,
		message,
		deliveryStatus,
		...(receiverRole === "parent" || receiverRole === "sibling" || receiverRole === "child" ? { receiverRole } : {}),
		target: {
			activeSessionId,
			sessionId,
			...(typeof sessionName === "string" ? { sessionName } : {}),
		},
	};
}

interface JupyterMessage {
	header: {
		msg_id: string;
		session: string;
		username: string;
		date: string;
		msg_type: string;
		version: string;
	};
	parent_header: Record<string, unknown>;
	metadata: Record<string, unknown>;
	content: Record<string, unknown>;
}

interface ActiveExecution {
	requestMsgId: string;
	/** Source of the cell currently executing; surfaced to rlm.run spawns. */
	code: string;
	started: number;
	maxChars: number;
	opts: ExecuteOptions;
	stdout: string;
	stderr: string;
	stdoutTruncated: boolean;
	stderrTruncated: boolean;
	result?: string;
	error?: ExecuteResult["error"];
	status: ExecuteResult["status"];
	settled: boolean;
	diffs: KernelDiffDisplay[];
	attachments: KernelAttachment[];
	sentAgentMessages: KernelSentAgentMessage[];
	/** M9: guards exactly-once finished emission for console cells. */
	cellFinishedEmitted?: boolean;
	resolve: (result: ExecuteResult) => void;
	reject: (error: Error) => void;
}

interface Deferred<T> {
	promise: Promise<T>;
	resolve: (value: T) => void;
	reject: (error: Error) => void;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function createDeferred<T>(): Deferred<T> {
	let resolve!: (value: T) => void;
	let reject!: (error: Error) => void;
	const promise = new Promise<T>((promiseResolve, promiseReject) => {
		resolve = promiseResolve;
		reject = promiseReject;
	});
	return { promise, resolve, reject };
}

function uuid(): string {
	return randomBytes(16).toString("hex");
}

// ---- wire format ---------------------------------------------------------

function buildMessage(
	msgType: string,
	content: Record<string, unknown>,
	session: string,
	username: string,
): JupyterMessage {
	return {
		header: {
			msg_id: uuid(),
			session,
			username,
			date: new Date().toISOString(),
			msg_type: msgType,
			version: PROTOCOL_VERSION,
		},
		parent_header: {},
		metadata: {},
		content,
	};
}

function sign(parts: Buffer[], key: string): Buffer {
	const hmac = createHmac("sha256", key);
	for (const p of parts) hmac.update(p);
	return Buffer.from(hmac.digest("hex"));
}

function encode(msg: JupyterMessage, key: string): Buffer[] {
	const parts = [
		Buffer.from(JSON.stringify(msg.header)),
		Buffer.from(JSON.stringify(msg.parent_header)),
		Buffer.from(JSON.stringify(msg.metadata)),
		Buffer.from(JSON.stringify(msg.content)),
	];
	return [DELIM, sign(parts, key), ...parts];
}

function decode(frames: Buffer[]): JupyterMessage | null {
	let i = 0;
	while (i < frames.length && !frames[i].equals(DELIM)) i++;
	if (i + 5 >= frames.length) return null;
	try {
		return {
			header: JSON.parse(frames[i + 2].toString()),
			parent_header: JSON.parse(frames[i + 3].toString()),
			metadata: JSON.parse(frames[i + 4].toString()),
			content: JSON.parse(frames[i + 5].toString()),
		};
	} catch {
		return null;
	}
}

// ---- connection setup ----------------------------------------------------

const CONNECTION_PORT_KEYS = ["shell_port", "iopub_port", "stdin_port", "control_port", "hb_port"] as const;

interface ConnectionInfo {
	ip: string;
	transport: "tcp";
	shell_port: number;
	iopub_port: number;
	stdin_port: number;
	control_port: number;
	hb_port: number;
	signature_scheme: "hmac-sha256";
	key: string;
	kernel_name: string;
}

function hasResolvedPorts(info: ConnectionInfo): boolean {
	return CONNECTION_PORT_KEYS.every((key) => Number.isInteger(info[key]) && info[key] > 0);
}

function parseConnectionInfo(value: unknown): ConnectionInfo | null {
	if (!isRecord(value)) return null;
	if (value.ip !== "127.0.0.1") return null;
	if (value.transport !== "tcp") return null;
	if (value.signature_scheme !== "hmac-sha256") return null;
	if (typeof value.key !== "string") return null;
	const shellPort = value.shell_port;
	const iopubPort = value.iopub_port;
	const stdinPort = value.stdin_port;
	const controlPort = value.control_port;
	const hbPort = value.hb_port;
	if (typeof shellPort !== "number" || !Number.isInteger(shellPort)) return null;
	if (typeof iopubPort !== "number" || !Number.isInteger(iopubPort)) return null;
	if (typeof stdinPort !== "number" || !Number.isInteger(stdinPort)) return null;
	if (typeof controlPort !== "number" || !Number.isInteger(controlPort)) return null;
	if (typeof hbPort !== "number" || !Number.isInteger(hbPort)) return null;
	const kernelName = typeof value.kernel_name === "string" ? value.kernel_name : "python3";
	return {
		ip: value.ip,
		transport: value.transport,
		shell_port: shellPort,
		iopub_port: iopubPort,
		stdin_port: stdinPort,
		control_port: controlPort,
		hb_port: hbPort,
		signature_scheme: value.signature_scheme,
		key: value.key,
		kernel_name: kernelName,
	};
}

function readConnectionInfo(path: string): ConnectionInfo | null {
	try {
		return parseConnectionInfo(JSON.parse(readFileSync(path, "utf8")));
	} catch {
		return null;
	}
}

function makeConnection(): { info: ConnectionInfo; path: string; tempDir: string } {
	const info: ConnectionInfo = {
		ip: "127.0.0.1",
		transport: "tcp",
		shell_port: 0,
		iopub_port: 0,
		stdin_port: 0,
		control_port: 0,
		hb_port: 0,
		signature_scheme: "hmac-sha256",
		key: randomBytes(16).toString("hex"),
		kernel_name: "python3",
	};
	const tempDir = mkdtempSync(join(tmpdir(), "prime-rlm-kernel-"));
	const path = join(tempDir, "connection.json");
	writeFileSync(path, JSON.stringify(info, null, 2), { mode: 0o600 });
	return { info, path, tempDir };
}

// ---- process-wide cleanup -----------------------------------------------

const liveKernels = new Set<KernelManager>();
let signalHandlersInstalled = false;

function installSignalHandlersOnce(): void {
	if (signalHandlersInstalled) return;
	signalHandlersInstalled = true;

	const asyncShutdown = async (): Promise<void> => {
		// These paths can await, so flush the namespace snapshot before tearing down.
		await Promise.allSettled([...liveKernels].map((k) => k.shutdown({ snapshot: true })));
	};

	// `beforeExit` and signal handlers can await async cleanup. `exit`
	// can only do sync work (Node won't run pending microtasks past it),
	// so it falls back to `disposeSync()` which kills the child synchronously.
	process.on("beforeExit", () => {
		void asyncShutdown();
	});
	process.on("SIGINT", () => {
		void asyncShutdown().finally(() => process.exit(130));
	});
	process.on("SIGTERM", () => {
		void asyncShutdown().finally(() => process.exit(143));
	});
	process.on("exit", () => {
		for (const k of liveKernels) k.disposeSync();
	});
}

function createKernelStartupAbortError(): Error {
	return new Error("Kernel startup aborted");
}

function raceStartupWithAbort<T>(promise: Promise<T>, signal: AbortSignal | undefined): Promise<T> {
	if (!signal) {
		return promise;
	}
	if (signal.aborted) {
		return Promise.reject(createKernelStartupAbortError());
	}
	return new Promise<T>((resolve, reject) => {
		let settled = false;
		const cleanup = () => signal.removeEventListener("abort", abort);
		const abort = () => {
			if (settled) {
				return;
			}
			settled = true;
			cleanup();
			reject(createKernelStartupAbortError());
		};
		signal.addEventListener("abort", abort, { once: true });
		promise.then(
			(value) => {
				if (settled) {
					return;
				}
				settled = true;
				cleanup();
				resolve(value);
			},
			(error: unknown) => {
				if (settled) {
					return;
				}
				settled = true;
				cleanup();
				reject(error);
			},
		);
	});
}

// ---- kernel manager ------------------------------------------------------

export class KernelManager {
	private readonly options: Pick<
		KernelManagerOptions,
		"python" | "cwd" | "env" | "sessionId" | "hostHandlers" | "snapshot" | "onLateSentAgentMessage" | "onCellEvent"
	> &
		Required<Pick<KernelManagerOptions, "username">>;
	/** M4 (O2): request ids whose tool result already shipped; late receipts route to onLateSentAgentMessage. */
	private readonly lateSentAgentMessageRequestIds = new Set<string>();
	private readonly session = uuid();
	private readonly commTargets = new Map<string, string>();
	private readonly handledHostRequestCommIds = new Set<string>();
	private kernel?: ChildProcess;
	private shell?: Dealer;
	private iopub?: Subscriber;
	private control?: Dealer;
	private iopubPumpPromise?: Promise<void>;
	private connection?: ConnectionInfo;
	private tempDir?: string;
	private kernelStderr = "";
	/** Serializes execute() calls — Jupyter shell channel is request/reply. */
	private executionQueue: Promise<unknown> = Promise.resolve();
	private activeExecution?: ActiveExecution;
	/** M9: console cells linked into the queue but not yet dispatched (cellIds, FIFO). */
	private pendingCellIds: string[] = [];
	/** M9: console cell currently dispatched to the kernel (cellId). */
	private activeCellId?: string;
	private readonly activeExecutionIdleWaiters = new Set<() => void>();
	// Source of the most recently started cell, retained after it finishes so
	// rlm.run spawns from detached asyncio tasks can still attribute their cell.
	private lastCellCode?: string;
	private readonly inFlightHostRequests = new Set<Promise<void>>();
	/** Pending debounced auto-snapshot, if one has been scheduled. */
	private snapshotTimer?: ReturnType<typeof globalThis.setTimeout>;
	private state: "idle" | "starting" | "running" | "shutdown" = "idle";
	/** Memoized so concurrent callers all await the same in-flight startup. */
	private startPromise?: Promise<void>;

	constructor(options: KernelManagerOptions) {
		this.options = {
			python: options.python,
			cwd: options.cwd,
			env: options.env,
			sessionId: options.sessionId,
			hostHandlers: options.hostHandlers,
			snapshot: options.snapshot,
			onLateSentAgentMessage: options.onLateSentAgentMessage,
			onCellEvent: options.onCellEvent,
			username: options.username ?? "prime-rlm",
		};
	}

	get ownerSessionId(): string | undefined {
		return this.options.sessionId;
	}

	private appendKernelDiagnostic(message: string): void {
		this.kernelStderr += `[kernel] ${message.endsWith("\n") ? message : `${message}\n`}`;
	}

	async start(options: KernelStartOptions = {}): Promise<void> {
		if (options.signal?.aborted) {
			throw createKernelStartupAbortError();
		}
		if (!this.startPromise) {
			this.startPromise = this.doStart().catch((error) => {
				this.startPromise = undefined;
				throw error;
			});
		}
		return raceStartupWithAbort(this.startPromise, options.signal);
	}

	private async doStart(): Promise<void> {
		if (this.state !== "idle") return;
		this.state = "starting";
		installSignalHandlersOnce();
		liveKernels.add(this);

		const connection = makeConnection();
		this.tempDir = connection.tempDir;

		const kernel = spawn(this.options.python, ["-m", "ipykernel_launcher", "-f", connection.path], {
			cwd: this.options.cwd,
			env: this.options.env ? { ...process.env, ...this.options.env } : process.env,
			stdio: ["ignore", "pipe", "pipe"],
		});
		this.kernel = kernel;

		kernel.stderr?.on("data", (buf: Buffer) => {
			this.kernelStderr += buf.toString();
		});

		kernel.on("error", (err) => {
			if (this.kernel !== kernel) return;
			this.appendKernelDiagnostic(`spawn error: ${err.message}`);
			this.state = "shutdown";
			liveKernels.delete(this);
			this.cleanupResources();
		});

		kernel.on("exit", (code, signal) => {
			if (this.kernel !== kernel) return;
			if (this.state !== "shutdown") {
				this.appendKernelDiagnostic(`unexpected exit code=${code} signal=${signal}`);
			}
			this.state = "shutdown";
			liveKernels.delete(this);
			this.cleanupResources();
		});

		let conn: ConnectionInfo;
		try {
			conn = await this.waitForResolvedConnection(connection.path);
			this.connection = conn;
		} catch (e) {
			const canRetryStartup = (this.state as string) !== "shutdown";
			await this.shutdown();
			if (canRetryStartup) this.state = "idle";
			throw e;
		}

		this.shell = new Dealer();
		this.iopub = new Subscriber();
		this.control = new Dealer();
		this.shell.connect(`${conn.transport}://${conn.ip}:${conn.shell_port}`);
		this.iopub.connect(`${conn.transport}://${conn.ip}:${conn.iopub_port}`);
		this.control.connect(`${conn.transport}://${conn.ip}:${conn.control_port}`);
		this.iopub.subscribe("");

		// ZMQ PUB/SUB slow-joiner: give the subscription a brief chance to reach the kernel before first execute.
		await sleep(IOPUB_SUBSCRIBE_DELAY_MS);
		this.startIopubPump();

		try {
			await this.probeReady();
		} catch (e) {
			const canRetryStartup = (this.state as string) !== "shutdown";
			await this.shutdown();
			if (canRetryStartup) this.state = "idle";
			throw e;
		}

		this.state = "running";
	}

	private async waitForResolvedConnection(connectionPath: string): Promise<ConnectionInfo> {
		const startedAt = Date.now();
		while (Date.now() - startedAt < PORTS_RESOLVE_TIMEOUT_MS) {
			if ((this.state as string) === "shutdown") {
				const tail = this.kernelStderr.slice(-1024);
				throw new Error(`Kernel exited before resolving ports. stderr:\n${tail || "(empty)"}`);
			}

			const info = readConnectionInfo(connectionPath);
			if (info && hasResolvedPorts(info)) {
				return info;
			}

			await sleep(25);
		}

		const tail = this.kernelStderr.slice(-1024);
		throw new Error(
			`Kernel did not resolve connection ports within ${PORTS_RESOLVE_TIMEOUT_MS}ms. stderr tail:\n${tail || "(empty)"}`,
		);
	}

	private async probeReady(): Promise<void> {
		const conn = this.connection!;
		const shell = this.shell!;

		const msg = buildMessage("kernel_info_request", {}, this.session, this.options.username);
		const requestMsgId = msg.header.msg_id;
		await shell.send(encode(msg, conn.key));

		const startedAt = Date.now();
		while (Date.now() - startedAt < READY_TIMEOUT_MS) {
			if ((this.state as string) === "shutdown") {
				const tail = this.kernelStderr.slice(-1024);
				throw new Error(`Kernel exited during startup. stderr:\n${tail || "(empty)"}`);
			}

			const remaining = READY_TIMEOUT_MS - (Date.now() - startedAt);
			const winner = await Promise.race([
				shell.receive().then((frames) => ({ kind: "frames" as const, frames })),
				sleep(remaining).then(() => ({ kind: "timeout" as const })),
			]);
			if (winner.kind === "timeout") break;

			const incoming = decode(winner.frames);
			if (
				incoming?.header.msg_type === "kernel_info_reply" &&
				(incoming.parent_header as { msg_id?: string }).msg_id === requestMsgId
			) {
				return;
			}
		}
		const tail = this.kernelStderr.slice(-1024);
		throw new Error(
			`Kernel did not respond to kernel_info_request within ${READY_TIMEOUT_MS}ms. stderr tail:\n${tail || "(empty)"}`,
		);
	}

		async execute(code: string, opts: ExecuteOptions = {}): Promise<ExecuteResult> {
		const result = await this.enqueueExecute(code, opts);
		// Refresh the on-disk snapshot after real work so a later resume (or a
		// crash before graceful shutdown) revives the most recent namespace.
		if (!opts.internal && result.status === "ok") {
			this.scheduleSnapshot();
		}
		return result;
	}

	/** Queue and run a cell, serializing against all other executions. */
	private async enqueueExecute(code: string, opts: ExecuteOptions): Promise<ExecuteResult> {
		if (opts.signal?.aborted) {
			if (opts.cell) this.emitCellFinished(opts.cell, "aborted", Date.now());
			return { stdout: "", stderr: "", status: "aborted", durationMs: 0 };
		}
		await this.start({ signal: opts.signal });
		if ((this.state as string) === "shutdown") {
			throw new Error("Kernel has been shut down");
		}

		const prev = this.executionQueue;
		let resolveNext: () => void = () => {};
		this.executionQueue = new Promise<void>((r) => {
			resolveNext = r;
		});
		if (opts.cell) this.pendingCellIds.push(opts.cell.cellId);
		await prev;

		const started = Date.now();
		try {
			await this.waitForActiveExecutionToClearForReuse(opts.signal);
			if (opts.signal?.aborted) {
				this.removePendingCell(opts.cell);
				if (opts.cell) this.emitCellFinished(opts.cell, "aborted", started);
				return { stdout: "", stderr: "", status: "aborted", durationMs: Date.now() - started };
			}
			if ((this.state as string) === "shutdown") {
				this.removePendingCell(opts.cell);
				throw new Error("Kernel has been shut down");
			}
			return await this.executeInner(code, opts, started);
		} finally {
			resolveNext();
		}
	}

	private removePendingCell(cell: KernelCellMeta | undefined): void {
		if (!cell) return;
		const idx = this.pendingCellIds.indexOf(cell.cellId);
		if (idx >= 0) this.pendingCellIds.splice(idx, 1);
	}

	/** M9: exactly-once finished event for a console cell. */
	private emitCellFinished(
		cell: KernelCellMeta,
		status: "ok" | "error" | "aborted",
		started: number,
		extra?: { error?: { ename: string; evalue: string }; stdoutTruncated?: boolean; stderrTruncated?: boolean },
		execution?: ActiveExecution,
	): void {
		if (execution) {
			if (execution.cellFinishedEmitted) return;
			execution.cellFinishedEmitted = true;
		}
		if (this.activeCellId === cell.cellId) this.activeCellId = undefined;
		try {
			this.options.onCellEvent?.({
				kind: "finished",
				meta: cell,
				status,
				durationMs: Date.now() - started,
				finishedAt: new Date().toISOString(),
				error: extra?.error,
				stdoutTruncated: extra?.stdoutTruncated ?? false,
				stderrTruncated: extra?.stderrTruncated ?? false,
			});
		} catch {
			/* console reporting must never break execution */
		}
	}

	private emitCellEvent(event: KernelCellEvent): void {
		try {
			this.options.onCellEvent?.(event);
		} catch {
			/* console reporting must never break execution */
		}
	}

	/** M9: console-cell queue occupancy (for queue-position reporting). */
	get cellQueueSnapshot(): { running: string | null; queued: string[] } {
		return { running: this.activeCellId ?? null, queued: [...this.pendingCellIds] };
	}

	private async executeInner(code: string, opts: ExecuteOptions, started: number): Promise<ExecuteResult> {
		const conn = this.connection!;
		const shell = this.shell!;
		const maxChars = opts.maxOutputChars ?? DEFAULT_MAX_OUTPUT_CHARS;

		const msg = buildMessage(
			"execute_request",
			{
				code,
				silent: false,
				store_history: true,
				user_expressions: {},
				allow_stdin: false,
				stop_on_error: true,
			},
			this.session,
			this.options.username,
		);
		const requestMsgId = msg.header.msg_id;

		this.removePendingCell(opts.cell);
		if (opts.signal?.aborted) {
			if (opts.cell) this.emitCellFinished(opts.cell, "aborted", started);
			return { stdout: "", stderr: "", status: "aborted", durationMs: Date.now() - started };
		}
		if (this.activeExecution) {
			if (opts.cell) {
				this.emitCellFinished(opts.cell, "error", started, { error: { ename: "KernelBusy", evalue: "kernel already has an active execution" } });
			}
			throw new Error("Kernel already has an active execution");
		}

		const result = createDeferred<ExecuteResult>();
		const execution: ActiveExecution = {
			requestMsgId,
			code,
			started,
			maxChars,
			opts,
			stdout: "",
			stderr: "",
			stdoutTruncated: false,
			stderrTruncated: false,
			status: "ok",
			settled: false,
			diffs: [],
			attachments: [],
			sentAgentMessages: [],
			resolve: result.resolve,
			reject: result.reject,
		};
		let abortTimer: ReturnType<typeof globalThis.setTimeout> | undefined;
		const clearAbortTimer = () => {
			if (abortTimer) {
				globalThis.clearTimeout(abortTimer);
				abortTimer = undefined;
			}
		};
		const forceAbort = () => {
			if (this.activeExecution !== execution) {
				return;
			}
			execution.status = "aborted";
			this.resolveExecution(execution, { clearActive: false });
		};
		const onAbort = () => {
			void this.interrupt().catch(() => undefined);
			clearAbortTimer();
			abortTimer = globalThis.setTimeout(forceAbort, KERNEL_ABORT_GRACE_MS);
			if (abortTimer && typeof abortTimer === "object" && "unref" in abortTimer) {
				abortTimer.unref();
			}
		};

		try {
			this.activeExecution = execution;
			opts.signal?.addEventListener("abort", onAbort, { once: true });
			if (opts.signal?.aborted) {
				onAbort();
			}
			if (!opts.internal) {
				this.lastCellCode = code;
			}
			try {
				const sendPromise = shell.send(encode(msg, conn.key));
				sendPromise.catch(() => undefined);
				await Promise.race([sendPromise, result.promise.then(() => undefined)]);
				if (this.activeExecution === execution && execution.status !== "aborted") {
					await sendPromise;
				}
				if (opts.cell && !execution.settled) {
					this.activeCellId = opts.cell.cellId;
					this.emitCellEvent({ kind: "running", meta: opts.cell, startedAt: new Date(started).toISOString() });
				}
			} catch (error) {
				if (this.activeExecution === execution) {
					this.activeExecution = undefined;
				}
				if (opts.cell) {
					this.emitCellFinished(opts.cell, "error", started, {
						error: { ename: "KernelChannelError", evalue: errorMessage(error) },
					}, execution);
				}
				throw error instanceof Error ? error : new Error(String(error));
			}
			return await result.promise;
		} finally {
			clearAbortTimer();
			opts.signal?.removeEventListener("abort", onAbort);
		}
	}

	private startIopubPump(): void {
		if (this.iopubPumpPromise) {
			return;
		}
		this.iopubPumpPromise = this.runIopubPump();
	}

	private async runIopubPump(): Promise<void> {
		const iopub = this.iopub;
		if (!iopub) {
			return;
		}

		try {
			for await (const frames of iopub) {
				const incoming = decode(frames);
				if (!incoming) continue;
				const t = incoming.header.msg_type;
				if (t === "comm_open" || t === "comm_msg" || t === "comm_close") {
					this.handleCommMessage(incoming);
					continue;
				}
				this.handleExecutionMessage(incoming);
			}
		} catch (error) {
			if ((this.state as string) !== "shutdown") {
				this.appendKernelDiagnostic(`iopub pump failed: ${errorMessage(error)}`);
				this.rejectActiveExecution(new Error(`Kernel IOPub channel failed: ${errorMessage(error)}`));
			}
		} finally {
			if (this.iopub === iopub) {
				this.iopubPumpPromise = undefined;
			}
		}
	}

	private handleExecutionMessage(incoming: JupyterMessage): void {
		const execution = this.activeExecution;
		const parentMessageId = (incoming.parent_header as { msg_id?: string }).msg_id;
		if (!execution || parentMessageId !== execution.requestMsgId) {
			// Late receipt for an already-settled cell (async send completing after
			// the tool result shipped) — route to the host's session-entry hook.
			if (incoming.header.msg_type === "display_data" || incoming.header.msg_type === "update_display_data") {
				const content = incoming.content as { data?: Record<string, unknown> };
				this.dispatchLateSentAgentMessage(parentMessageId, content.data?.[AGENT_MESSAGE_DISPLAY_MIME]);
			}
			return;
		}

		const t = incoming.header.msg_type;
		if (execution.settled && (t === "display_data" || t === "update_display_data")) {
			const content = incoming.content as { data?: Record<string, unknown> };
			if (this.dispatchLateSentAgentMessage(parentMessageId, content.data?.[AGENT_MESSAGE_DISPLAY_MIME])) {
				return;
			}
		}
		if (t === "stream") {
			const c = incoming.content as { name: "stdout" | "stderr"; text: string };
			if (c.name === "stdout") {
				if (execution.stdout.length < execution.maxChars) {
					execution.stdout += c.text;
					if (execution.stdout.length > execution.maxChars) {
						execution.stdout = execution.stdout.slice(0, execution.maxChars);
						execution.stdoutTruncated = true;
					}
				}
			} else if (c.name === "stderr") {
				if (execution.stderr.length < execution.maxChars) {
					execution.stderr += c.text;
					if (execution.stderr.length > execution.maxChars) {
						execution.stderr = execution.stderr.slice(0, execution.maxChars);
						execution.stderrTruncated = true;
					}
				}
			}
			execution.opts.onStream?.(c.text, c.name);
			if (execution.opts.cell && (c.name === "stdout" || c.name === "stderr")) {
				this.emitCellEvent({ kind: "stream", meta: execution.opts.cell, name: c.name, text: c.text });
			}
		} else if (t === "execute_result") {
			const c = incoming.content as { data: Record<string, string> };
			if (c.data["text/plain"]) execution.result = c.data["text/plain"];
			if (execution.opts.cell && typeof c.data["text/plain"] === "string") {
				this.emitCellEvent({ kind: "result", meta: execution.opts.cell, text: c.data["text/plain"] });
			}
		} else if (t === "display_data" || t === "update_display_data") {
			const c = incoming.content as { data?: Record<string, unknown> };
			if (execution.opts.cell && c.data) {
				for (const [mimeType, value] of Object.entries(c.data)) {
					if (typeof value === "string" && (mimeType === "text/plain" || mimeType === "image/png" || mimeType === "image/jpeg")) {
						this.emitCellEvent({ kind: "display", meta: execution.opts.cell, mimeType, data: value });
					}
				}
			}
			const diff = parseDiffDisplay(c.data?.[DIFF_DISPLAY_MIME]);
			if (diff) execution.diffs.push(diff);
			const attachment = parseAttachmentDisplay(c.data?.[ATTACHMENT_DISPLAY_MIME]);
			if (attachment === "oversized") {
				execution.stderr += `${execution.stderr ? "\n" : ""}attachment dropped: exceeds ${MAX_ATTACHMENT_DATA_CHARS} base64 chars`;
				execution.status = "error";
			} else if (attachment) {
				execution.attachments.push(attachment);
			}
			const sentAgentMessage = parseSentAgentMessage(c.data?.[AGENT_MESSAGE_DISPLAY_MIME]);
			if (sentAgentMessage) execution.sentAgentMessages.push(sentAgentMessage);
		} else if (t === "error") {
			const c = incoming.content as { ename: string; evalue: string; traceback: string[] };
			execution.error = c;
			execution.status = "error";
			if (execution.opts.cell) {
				this.emitCellEvent({ kind: "error", meta: execution.opts.cell, ename: c.ename, evalue: c.evalue, traceback: c.traceback ?? [] });
			}
		} else if (t === "status") {
			const c = incoming.content as { execution_state: string };
			if (c.execution_state === "idle") {
				this.finishActiveExecution(execution);
			}
		}
	}

	/** M4 (O2): route a receipt that arrived outside its owning execution. */
	private dispatchLateSentAgentMessage(parentMessageId: string | undefined, value: unknown): boolean {
		const sentAgentMessage = parseSentAgentMessage(value);
		if (!sentAgentMessage) {
			return false;
		}
		if (this.lateSentAgentMessageRequestIds.has(sentAgentMessage.id)) {
			return true; // duplicate emit of an already-recorded receipt
		}
		this.lateSentAgentMessageRequestIds.add(sentAgentMessage.id);
		while (this.lateSentAgentMessageRequestIds.size > MAX_LATE_SENT_AGENT_MESSAGE_HANDLERS) {
			const oldest = this.lateSentAgentMessageRequestIds.values().next().value;
			if (oldest === undefined) break;
			this.lateSentAgentMessageRequestIds.delete(oldest);
		}
		this.options.onLateSentAgentMessage?.(sentAgentMessage);
		return true;
	}

	private finishActiveExecution(execution: ActiveExecution): void {
		if (this.activeExecution !== execution) {
			return;
		}
		this.resolveExecution(execution, { clearActive: true });
	}

	private resolveExecution(execution: ActiveExecution, options: { clearActive: boolean }): void {
		const didClearActive = options.clearActive && this.activeExecution === execution;
		if (options.clearActive && this.activeExecution === execution) {
			this.activeExecution = undefined;
		}
		if (!execution.settled) {
			execution.settled = true;

			// Remember in-result receipt ids so a duplicate late emit is dropped.
			for (const m of execution.sentAgentMessages) {
				this.lateSentAgentMessageRequestIds.add(m.id);
			}

			let stdout = execution.stdout;
			let stderr = execution.stderr;
			let result = execution.result;
			let status = execution.status;
			if (execution.stdoutTruncated) stdout += `\n[... output truncated at ${execution.maxChars} chars ...]`;
			if (execution.stderrTruncated) stderr += `\n[... output truncated at ${execution.maxChars} chars ...]`;
			if (result !== undefined && result.length > execution.maxChars) {
				result = `${result.slice(0, execution.maxChars)}\n[... output truncated at ${execution.maxChars} chars ...]`;
			}

			if (execution.opts.signal?.aborted) status = "aborted";

			if (execution.opts.cell) {
				this.emitCellFinished(execution.opts.cell, status, execution.started, {
					error: execution.error ? { ename: execution.error.ename, evalue: execution.error.evalue } : undefined,
					stdoutTruncated: execution.stdoutTruncated,
					stderrTruncated: execution.stderrTruncated,
				}, execution);
			}

			execution.resolve({
				stdout,
				stderr,
				result,
				error: execution.error,
				status,
				durationMs: Date.now() - execution.started,
				diffs: execution.diffs.length > 0 ? execution.diffs : undefined,
				attachments: execution.attachments.length > 0 ? execution.attachments : undefined,
				sentAgentMessages: execution.sentAgentMessages.length > 0 ? execution.sentAgentMessages : undefined,
			});
		}
		if (didClearActive) {
			this.notifyActiveExecutionIdle();
		}
	}

	private rejectActiveExecution(error: Error): void {
		const execution = this.activeExecution;
		if (!execution) {
			return;
		}
		this.activeExecution = undefined;
		if (execution.opts.cell) {
			this.emitCellFinished(execution.opts.cell, "error", execution.started, {
				error: { ename: "KernelChannelError", evalue: errorMessage(error) },
			}, execution);
		}
		execution.reject(error);
		this.notifyActiveExecutionIdle();
	}

	private notifyActiveExecutionIdle(): void {
		for (const resolve of this.activeExecutionIdleWaiters) {
			resolve();
		}
		this.activeExecutionIdleWaiters.clear();
	}

	private waitForActiveExecutionToClear(signal: AbortSignal | undefined, timeoutMs: number): Promise<boolean> {
		if (!this.activeExecution) {
			return Promise.resolve(true);
		}
		return new Promise<boolean>((resolve) => {
			let settled = false;
			let timeout: ReturnType<typeof globalThis.setTimeout> | undefined;
			const finish = (cleared: boolean) => {
				if (settled) {
					return;
				}
				settled = true;
				if (timeout) {
					globalThis.clearTimeout(timeout);
				}
				this.activeExecutionIdleWaiters.delete(onIdle);
				signal?.removeEventListener("abort", onAbort);
				resolve(cleared);
			};
			const onIdle = () => finish(true);
			const onAbort = () => finish(false);
			this.activeExecutionIdleWaiters.add(onIdle);
			signal?.addEventListener("abort", onAbort, { once: true });
			timeout = globalThis.setTimeout(() => finish(false), timeoutMs);
			if (timeout && typeof timeout === "object" && "unref" in timeout) {
				timeout.unref();
			}
		});
	}

	private async waitForActiveExecutionToClearForReuse(signal?: AbortSignal): Promise<void> {
		const started = Date.now();
		while (this.activeExecution && Date.now() - started < KERNEL_BUSY_REUSE_WAIT_MS) {
			if ((this.state as string) === "shutdown") {
				throw new Error("Kernel has been shut down");
			}
			void this.interrupt().catch(() => undefined);
			const remaining = KERNEL_BUSY_REUSE_WAIT_MS - (Date.now() - started);
			const cleared = await this.waitForActiveExecutionToClear(
				signal,
				Math.max(1, Math.min(KERNEL_BUSY_INTERRUPT_INTERVAL_MS, remaining)),
			);
			if (cleared || signal?.aborted) {
				return;
			}
		}
		if (this.activeExecution) {
			throw new KernelBusyAfterInterruptError();
		}
	}

	private handleCommMessage(incoming: JupyterMessage): void {
		const msgType = incoming.header.msg_type;
		const content = incoming.content;
		const commId = content.comm_id;
		if (typeof commId !== "string") {
			return;
		}

		if (msgType === "comm_close") {
			this.commTargets.delete(commId);
			this.handledHostRequestCommIds.delete(commId);
			return;
		}

		if (msgType === "comm_open") {
			const targetName = content.target_name;
			if (typeof targetName !== "string") {
				return;
			}
			this.commTargets.set(commId, targetName);
			if (targetName === HOST_COMM_TARGET) {
				this.startHostRequestFromComm(commId, content.data);
			}
			return;
		}

		const targetName = this.commTargets.get(commId);
		if (msgType === "comm_msg" && targetName === HOST_COMM_TARGET) {
			this.startHostRequestFromComm(commId, content.data);
		}
	}

	private startHostRequestFromComm(commId: string, data: unknown): void {
		if (this.handledHostRequestCommIds.has(commId)) {
			return;
		}
		this.handledHostRequestCommIds.add(commId);

		const task = (async () => {
			try {
				const result = await this.handleHostRequest(data);
				try {
					// Nested under "value": handler results may legitimately carry their own
					// "status" key (rlm.run returns status: "completed"), which must not
					// collide with the bridge envelope status.
					await this.sendCommMessage(commId, { status: "ok", value: result ?? {} });
				} catch (replyError) {
					this.appendKernelDiagnostic(
						`failed to send host request ok reply for comm ${commId}: ${errorMessage(replyError)}`,
					);
				}
			} catch (error) {
				this.appendKernelDiagnostic(`host request failed for comm ${commId}: ${errorMessage(error)}`);
				try {
					await this.sendCommMessage(commId, { status: "error", error: errorMessage(error) });
				} catch (replyError) {
					this.appendKernelDiagnostic(
						`failed to send host request error reply for comm ${commId}: ${errorMessage(replyError)}`,
					);
				}
			}
		})();
		this.inFlightHostRequests.add(task);
		void task.finally(() => {
			this.inFlightHostRequests.delete(task);
		});
	}

	private async handleHostRequest(data: unknown): Promise<Record<string, unknown>> {
		if (!isRecord(data)) {
			throw new Error("host request payload must be an object");
		}
		if (typeof data.type !== "string" || data.type.length === 0) {
			throw new Error("host request payload must have a string type");
		}

		const handler = this.options.hostHandlers?.[data.type];
		if (!handler) {
			throw new Error(`host request type "${data.type}" is not available in this session`);
		}
		// Tag the request with the cell that triggered it.
		const cellSourceCode = this.activeExecution?.code ?? this.lastCellCode;
		return handler({ ...data, cellSourceCode });
	}

	private async sendCommMessage(commId: string, data: Record<string, unknown>): Promise<void> {
		const channel = this.control ?? this.shell;
		if (!channel || !this.connection) {
			throw new Error("Kernel channel is not connected");
		}
		const msg = buildMessage("comm_msg", { comm_id: commId, data }, this.session, this.options.username);
		await channel.send(encode(msg, this.connection.key));
	}

	private async interrupt(): Promise<void> {
		if (!this.control || !this.connection) return;
		const msg = buildMessage("interrupt_request", {}, this.session, this.options.username);
		await this.control.send(encode(msg, this.connection.key));
	}

	private cleanupResources(killSignal: NodeJS.Signals = "SIGTERM"): void {
		this.clearSnapshotTimer();
		this.rejectActiveExecution(new Error("Kernel has been shut down"));
		this.shell?.close();
		this.iopub?.close();
		this.control?.close();
		this.shell = undefined;
		this.iopub = undefined;
		this.control = undefined;
		this.iopubPumpPromise = undefined;
		try {
			this.kernel?.kill(killSignal);
		} catch {
			// Kernel already exited.
		}
		this.kernel = undefined;
		this.connection = undefined;
		if (this.tempDir) {
			try {
				rmSync(this.tempDir, { recursive: true, force: true });
			} catch {
				// Leave the temp dir for OS tmp cleanup.
			}
		}
		this.tempDir = undefined;
		this.startPromise = undefined;
	}

	private async waitForHostRequestsToSettle(tasks: Promise<void>[], timeoutMs: number): Promise<void> {
		let timeout: ReturnType<typeof globalThis.setTimeout> | undefined;
		const timeoutPromise = new Promise<"timeout">((resolve) => {
			timeout = globalThis.setTimeout(() => resolve("timeout"), timeoutMs);
			if (timeout && typeof timeout === "object" && "unref" in timeout) {
				timeout.unref();
			}
		});

		const result = await Promise.race([Promise.allSettled(tasks).then(() => "settled" as const), timeoutPromise]);
		if (timeout) {
			globalThis.clearTimeout(timeout);
		}
		if (result === "timeout") {
			this.appendKernelDiagnostic(
				`timed out waiting ${timeoutMs}ms for ${tasks.length} host request task(s) during dispose`,
			);
		}
	}

	/**
	 * Serialize the user namespace to disk (best-effort, per-variable). No-op when
	 * the kernel isn't running or no snapshot target was configured. Never throws.
	 */
	async snapshotState(): Promise<SnapshotResult | null> {
		const cfg = this.options.snapshot;
		if (!cfg || !this.isRunning) return null;
		const code = buildSnapshotCode(cfg.path, cfg.manifestPath, cfg.maxBytes ?? DEFAULT_SNAPSHOT_MAX_BYTES);
		try {
			const r = await this.enqueueExecute(code, { maxOutputChars: SNAPSHOT_MAX_OUTPUT_CHARS, internal: true });
			if (r.status !== "ok") {
				this.appendKernelDiagnostic(`state snapshot failed: ${r.error?.evalue ?? r.stderr}`);
				return null;
			}
			return parseSnapshotResult(r.stdout, cfg.path);
		} catch (error) {
			this.appendKernelDiagnostic(`state snapshot error: ${errorMessage(error)}`);
			return null;
		}
	}

	/**
	 * Revive a previously snapshotted namespace into the kernel. Call right after
	 * start() and before the runtime bootstrap, which then refreshes live handles
	 * (rlm, skills) over anything restored. Never throws.
	 */
	async restoreState(): Promise<RestoreResult | null> {
		const cfg = this.options.snapshot;
		if (!cfg) return null;
		const code = buildRestoreCode(cfg.path);
		try {
			const r = await this.enqueueExecute(code, { maxOutputChars: SNAPSHOT_MAX_OUTPUT_CHARS, internal: true });
			if (r.status !== "ok") {
				this.appendKernelDiagnostic(`state restore failed: ${r.error?.evalue ?? r.stderr}`);
				return null;
			}
			return parseRestoreResult(r.stdout, cfg.path);
		} catch (error) {
			this.appendKernelDiagnostic(`state restore error: ${errorMessage(error)}`);
			return null;
		}
	}

	/** Live user-defined top-level names, or null if the kernel isn't running. Never throws. */
	async listNamespaceNames(signal?: AbortSignal): Promise<string[] | null> {
		if (!this.isRunning) return null;
		try {
			const r = await this.enqueueExecute(buildListNamesCode(), {
				maxOutputChars: SNAPSHOT_MAX_OUTPUT_CHARS,
				internal: true,
				signal,
			});
			if (r.status !== "ok") {
				this.appendKernelDiagnostic(`namespace listing failed: ${r.error?.evalue ?? r.stderr}`);
				return null;
			}
			return parseListNamesResult(r.stdout);
		} catch (error) {
			this.appendKernelDiagnostic(`namespace listing error: ${errorMessage(error)}`);
			return null;
		}
	}

	private scheduleSnapshot(): void {
		const cfg = this.options.snapshot;
		if (!cfg) return;
		if (this.snapshotTimer) clearTimeout(this.snapshotTimer);
		this.snapshotTimer = globalThis.setTimeout(() => {
			this.snapshotTimer = undefined;
			void this.snapshotState();
		}, cfg.debounceMs ?? DEFAULT_SNAPSHOT_DEBOUNCE_MS);
		if (this.snapshotTimer && typeof this.snapshotTimer === "object" && "unref" in this.snapshotTimer) {
			this.snapshotTimer.unref();
		}
	}

	private clearSnapshotTimer(): void {
		if (this.snapshotTimer) {
			clearTimeout(this.snapshotTimer);
			this.snapshotTimer = undefined;
		}
	}

	/** Best-effort final snapshot before a graceful dispose, bounded by a timeout. */
	private async flushSnapshotForDispose(): Promise<void> {
		if (!this.options.snapshot || !this.isRunning) return;
		let timeout: ReturnType<typeof globalThis.setTimeout> | undefined;
		const guard = new Promise<void>((resolve) => {
			timeout = globalThis.setTimeout(resolve, SNAPSHOT_DISPOSE_TIMEOUT_MS);
			if (timeout && typeof timeout === "object" && "unref" in timeout) timeout.unref();
		});
		try {
			await Promise.race([this.snapshotState().then(() => undefined), guard]);
		} finally {
			if (timeout) clearTimeout(timeout);
		}
	}

	async shutdown(opts: { snapshot?: boolean } = {}): Promise<void> {
		if (this.state === "shutdown") {
			liveKernels.delete(this);
			this.cleanupResources();
			return;
		}
		// Best-effort final flush (bounded) before teardown — used by signal handlers
		// so a SIGINT/SIGTERM exit doesn't lose work the debounced snapshot hasn't saved.
		if (opts.snapshot) {
			await this.flushSnapshotForDispose();
		}
		this.state = "shutdown";
		liveKernels.delete(this);

		try {
			if (this.control && this.connection) {
				const msg = buildMessage("shutdown_request", { restart: false }, this.session, this.options.username);
				await this.control.send(encode(msg, this.connection.key));
				await sleep(200);
			}
		} catch (error) {
			this.appendKernelDiagnostic(
				`shutdown_request send failed (killing instead): ${error instanceof Error ? error.message : String(error)}`,
			);
		}

		this.cleanupResources();
	}

	async kill(): Promise<void> {
		this.state = "shutdown";
		liveKernels.delete(this);
		this.cleanupResources("SIGKILL");
	}

	/** Graceful cleanup. Waits briefly for in-flight host request handlers before closing sockets. */
	dispose(): Promise<void> {
		return (async () => {
			// Final namespace flush while the kernel is still live (session end / reload).
			await this.flushSnapshotForDispose();
			this.state = "shutdown";
			liveKernels.delete(this);
			const inFlightHostRequests = [...this.inFlightHostRequests];
			try {
				if (inFlightHostRequests.length > 0) {
					await this.waitForHostRequestsToSettle(inFlightHostRequests, HOST_REQUEST_DISPOSE_TIMEOUT_MS);
				}
			} finally {
				this.cleanupResources();
			}
		})();
	}


	/** Synchronous best-effort cleanup. Safe to call from `process.on('exit')`. */
	disposeSync(): void {
		this.state = "shutdown";
		liveKernels.delete(this);
		this.cleanupResources();
	}

	get isRunning(): boolean {
		return this.state === "running";
	}

	/** Snapshot config, when namespace persistence is enabled for this kernel. */
	get snapshotConfig(): KernelSnapshotConfig | undefined {
		return this.options.snapshot;
	}
}
