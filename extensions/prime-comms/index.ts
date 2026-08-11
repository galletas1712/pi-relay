// prime-comms — M2 milestone extension for unpatched upstream pi.
//
// Inter-agent messaging + observation for prime-rlm session trees, ported from
// prime-agent's core/agent-messages.ts + core/agent-observe.ts + the python
// agent_message/agent_observe skills.
//
//   (a) in-kernel `agent_message.send/list_agents` and
//       `agent_observe.get_agent/recent_messages/list_agents`, routed through
//       the M1 kernel host bridge via prime-rlm's public host-handler seam
//   (b) durable JSONL outbox per session: every send writes "queued" before
//       delivery and a terminal record after; on session_start, still-queued
//       records are re-driven (live delivery → direct session-file append →
//       recovery notice into the sender's own context)
//   (c) delivery: to a settled live child via AgentSession.sendCustomMessage
//       (triggerTurn when idle — wakes it; steer when busy, with a settle-time
//       park-window re-issue guard), to the parent via its captured
//       ExtensionAPI (triggerTurn when idle; DEFERRED to the next settle when
//       busy, so the reply always starts a real follow-up turn)
//
// HARD dependency: prime-rlm must be loaded (kernel bridge + child registry).
// Load order: prime-rlm, then prime-comms (prime-harness anywhere after rlm).

import { existsSync, readdirSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import {
	SessionManager,
	sessionEntryToContextMessages,
	type AgentSession,
	type ExtensionAPI,
	type ExtensionContext,
} from "@earendil-works/pi-coding-agent";
import { COMMS_KERNEL_PYTHON } from "./src/kernel-shim.ts";
import { appendOutboxRecord, pendingOutboxRecords } from "./src/outbox.ts";
import {
	AGENT_MESSAGE_CUSTOM_TYPE,
	AGENT_MESSAGE_SOURCE,
	createAgentMessage,
	createAgentMessageReceipt,
	newAgentMessageId,
	OUTBOX_RECOVERY_CUSTOM_TYPE,
	type AgentMessagePayload,
	type AgentMessageReceipt,
	type AgentMessageSender,
	type AgentRef,
	type FamilyRelationship,
	type OutboxRecord,
	type ReceiverRole,
	type ResolvedTarget,
} from "./src/protocol.ts";

// ---- prime-rlm seam (structural type; resolved lazily via globalThis) -------

interface PrimeRlmSessionInfo {
	sessionId: string;
	sessionDir: string;
	cwd: string;
	agentDir: string;
	depth: number;
}
interface ChildRegistryEntry {
	rlm_child_id: string;
	session_name: string;
	session_dir: string;
	session_id: string | null;
	status: string;
	/** M7: subagent role name when spawned via rlm.run(role=...). */
	role?: string;
}
interface ParentRef {
	parentSessionId: string;
	childId: string;
	name: string;
}
interface LiveChild {
	entry: ChildRegistryEntry;
	session: AgentSession;
}
type HostRequestHandler = (payload: Record<string, unknown>) => Promise<Record<string, unknown>>;
interface PrimeRlmHostApi {
	version: number;
	registerKernelBootstrapContributor(
		fn: (info: PrimeRlmSessionInfo) => { python?: string; env?: Record<string, string> } | undefined,
	): void;
	registerHostHandlerProvider(fn: (info: PrimeRlmSessionInfo) => Record<string, HostRequestHandler> | undefined): void;
	listSessions(): PrimeRlmSessionInfo[];
	sessionInfo(sessionId: string): PrimeRlmSessionInfo | undefined;
	listChildren(parentSessionId: string): ChildRegistryEntry[];
	getLiveChild(parentSessionId: string, childId: string): LiveChild | undefined;
	findChildByName(parentSessionId: string, name: string): ChildRegistryEntry | undefined;
	getParentRef(sessionId: string): ParentRef | undefined;
}

function getPrimeRlmApi(): PrimeRlmHostApi | undefined {
	const candidate = (globalThis as Record<symbol, unknown>)[Symbol.for("prime-rlm.host-api")];
	if (candidate && (candidate as { version?: unknown }).version === 1) {
		return candidate as PrimeRlmHostApi;
	}
	return undefined;
}

function requirePrimeRlmApi(): PrimeRlmHostApi {
	const api = getPrimeRlmApi();
	if (!api) {
		throw new Error("prime-comms requires the prime-rlm extension (host api prime-rlm.host-api v1 not found)");
	}
	return api;
}

// ---- per-session bindings ----------------------------------------------------
// Extension factories re-execute per session with a session-bound ExtensionAPI,
// so every live session in this process (root + rlm children) registers itself.
// Delivery to a parent goes through the parent's captured pi; observation reads
// the target's captured ctx.sessionManager.

interface Binding {
	pi: ExtensionAPI;
	ctx: ExtensionContext;
}
const bindings = new Map<string, Binding>();

// ---- deferred delivery + steer park guard ------------------------------------
// pi rpc-mode hosts expose one sharp edge for comms wakeup: a steered custom
// message is only drained by an IN-FLIGHT agent run. Delivered while the target
// is busy it is consumed inside the current run (no new turn ever exists); in
// the post-drain window just before settle it parks in agent-core's queue with
// nothing left to drain it. For child→parent traffic the product contract is
// stronger: the reply STARTS A NEW TURN on the parent (COMMS_PROMPT_SECTION).
// So:
//  - busy-parent deliveries are DEFERRED: queued here and delivered with
//    triggerTurn at the parent's next agent_settled, guaranteeing a real
//    follow-up turn (the outbox "queued" record stays open until the drain
//    writes the terminal record, so a host restart re-drives it);
//  - busy-child deliveries keep immediate steer semantics (m3-r2 mid-run
//    steering) but are tracked: message_start for the same id proves
//    consumption; an unconsumed steer at settle is re-issued once with
//    triggerTurn (park-window repair).

type AgentMessageObject = ReturnType<typeof createAgentMessage>;

interface DeferredDelivery {
	id: string;
	message: AgentMessageObject;
	finalize: (status: "delivered" | "failed", error?: string) => void;
}
/** Deferred deliveries keyed by TARGET sessionId. */
const deferredByTarget = new Map<string, DeferredDelivery[]>();

interface SteerDelivery {
	id: string;
	message: AgentMessageObject;
	reissued: boolean;
}
/** Busy-target steer deliveries awaiting consumption proof, keyed by TARGET sessionId. */
const steerPendingByTarget = new Map<string, SteerDelivery[]>();

function trackSteerDelivery(targetSessionId: string, message: AgentMessageObject): void {
	const list = steerPendingByTarget.get(targetSessionId) ?? [];
	list.push({ id: message.details.id, message, reissued: false });
	steerPendingByTarget.set(targetSessionId, list);
}

/** A message_start for a delivered agent_message proves the loop consumed it. */
function markDeliveryConsumed(sessionId: string, messageId: string): void {
	const list = steerPendingByTarget.get(sessionId);
	if (!list) return;
	const next = list.filter((d) => d.id !== messageId);
	if (next.length > 0) steerPendingByTarget.set(sessionId, next);
	else steerPendingByTarget.delete(sessionId);
}

function deferParentDelivery(
	targetSessionId: string,
	message: AgentMessageObject,
	finalize: (status: "delivered" | "failed", error?: string) => void,
): void {
	const list = deferredByTarget.get(targetSessionId) ?? [];
	list.push({ id: message.details.id, message, finalize });
	deferredByTarget.set(targetSessionId, list);
	// Settle-race guard: the target may have settled between the isIdle() check
	// and this deferral; drain now (no-ops while the target is busy).
	setImmediate(() => drainTarget(targetSessionId));
}

/** Drain deferred deliveries + repair parked steers for one target session.
 * Called from the target's own agent_settled handler (via setImmediate, so the
 * settle event fully propagates before a follow-up turn starts). */
function drainTarget(targetSessionId: string): void {
	const deferred = deferredByTarget.get(targetSessionId) ?? [];
	const parked = steerPendingByTarget.get(targetSessionId) ?? [];
	if (deferred.length === 0 && parked.length === 0) return;
	const binding = bindings.get(targetSessionId);
	if (!binding) {
		// Target session is gone: resolve deferred sends so sender outboxes
		// terminate (and rlm-host outcome fallbacks can fire).
		if (deferred.length > 0) {
			deferredByTarget.delete(targetSessionId);
			for (const d of deferred) d.finalize("failed", "target session is not bound in this process");
		}
		return;
	}
	if (!binding.ctx.isIdle()) return; // wait for the next settle
	if (deferred.length > 0) {
		deferredByTarget.delete(targetSessionId);
		for (const d of deferred) {
			try {
				if (binding.ctx.isIdle()) {
					binding.pi.sendMessage(d.message, { triggerTurn: true });
				} else {
					// The first deferred delivery already started the follow-up turn;
					// steer the rest into it (consumed in-run), tracked like any steer.
					binding.pi.sendMessage(d.message, { triggerTurn: true, deliverAs: "steer" });
					trackSteerDelivery(targetSessionId, d.message);
				}
				d.finalize("delivered");
			} catch (error) {
				d.finalize("failed", errorMessage(error));
			}
		}
	}
	// Park repair: steers never consumed by the run get ONE triggerTurn re-issue;
	// the resulting message_start marks them consumed (markDeliveryConsumed).
	const stillParked = (steerPendingByTarget.get(targetSessionId) ?? []).filter((d) => !d.reissued);
	const justReissued = new Set<string>();
	for (const d of stillParked) {
		d.reissued = true;
		try {
			binding.pi.sendMessage(d.message, { triggerTurn: true });
			justReissued.add(d.id);
		} catch (error) {
			d.reissued = false; // keep for the next settle
			console.warn(`[prime-comms] steer re-issue failed for ${d.id}: ${errorMessage(error)}`);
		}
	}
	// Re-issued at a PREVIOUS settle and still unconsumed after a full turn:
	// drop (avoid re-issue loops). Entries just re-issued above get their turn
	// to prove consumption via message_start before this filter applies.
	const leftover = (steerPendingByTarget.get(targetSessionId) ?? []).filter((d) => d.reissued && !justReissued.has(d.id));
	if (leftover.length > 0) {
		console.warn(`[prime-comms] dropping ${leftover.length} unconsumed steer(s) for ${targetSessionId}`);
		const drop = new Set(leftover.map((d) => d.id));
		const next = (steerPendingByTarget.get(targetSessionId) ?? []).filter((d) => !drop.has(d.id));
		if (next.length > 0) steerPendingByTarget.set(targetSessionId, next);
		else steerPendingByTarget.delete(targetSessionId);
	}
}

/** Per-session wiring (factory): observe consumption + drive drains. Called for
 * every bound session (roots and rlm children). */
function registerDeliveryGuards(pi: ExtensionAPI): void {
	pi.on("message_start", async (event, ctx) => {
		const m = event.message as
			| { role?: string; customType?: string; details?: { id?: unknown } }
			| undefined;
		if (m?.role === "custom" && m.customType === AGENT_MESSAGE_CUSTOM_TYPE && typeof m.details?.id === "string") {
			markDeliveryConsumed(ctx.sessionManager.getSessionId(), m.details.id);
		}
	});
	pi.on("agent_settled", async (_event, ctx) => {
		const sid = ctx.sessionManager.getSessionId();
		setImmediate(() => {
			try {
				drainTarget(sid);
			} catch (error) {
				console.warn(`[prime-comms] delivery drain failed for ${sid}: ${errorMessage(error)}`);
			}
		});
	});
}

function agentDir(): string {
	return process.env.PI_CODING_AGENT_DIR ?? join(homedir(), ".pi", "agent");
}

function commsStateDir(sessionDir: string, sessionId: string): string {
	return join(sessionDir, "prime", sessionId, "comms");
}

function outboxPathFor(sessionDir: string, sessionId: string): string {
	return join(commsStateDir(sessionDir, sessionId), "outbox.jsonl");
}

/** Terminal outbox writes for deferred sends can land AFTER the sender's
 * session dir was deleted (rlm.delete_subagent on a child whose reply was
 * deferred to the parent's next settle). appendOutboxRecord mkdirs the parent
 * dir, so an unguarded late write would resurrect the deleted session dir
 * (m3-r3 leak). Guard terminal writes on the sender session dir still existing. */
function appendTerminalOutboxRecord(
	sessionDir: string,
	outboxPath: string,
	record: Parameters<typeof appendOutboxRecord>[1],
): void {
	if (!existsSync(sessionDir)) return;
	appendOutboxRecord(outboxPath, record);
}

const SETTLE_TIMEOUT_MS = Number(process.env.PRIME_COMMS_SETTLE_TIMEOUT_MS ?? 120_000);
const DELIVERY_DELAY_MS = Number(process.env.PRIME_COMMS_DELIVERY_DELAY_MS ?? 0);

function sleep(ms: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

// ---- family resolution --------------------------------------------------------

interface Family {
	parentRef?: ParentRef;
	children: ChildRegistryEntry[];
	siblings: ChildRegistryEntry[];
}

function familyOf(api: PrimeRlmHostApi, sessionId: string): Family {
	const parentRef = api.getParentRef(sessionId);
	const children = api.listChildren(sessionId);
	const siblings = parentRef
		? api.listChildren(parentRef.parentSessionId).filter((c) => c.session_id !== sessionId)
		: [];
	return { parentRef, children, siblings };
}

function senderRef(api: PrimeRlmHostApi, info: PrimeRlmSessionInfo): AgentMessageSender {
	const parentRef = api.getParentRef(info.sessionId);
	return { sessionId: info.sessionId, name: parentRef?.name, depth: info.depth };
}

function resolveTarget(
	api: PrimeRlmHostApi,
	info: PrimeRlmSessionInfo,
	role: ReceiverRole,
	name: string | undefined,
): ResolvedTarget {
	if (role === "parent") {
		const parentRef = api.getParentRef(info.sessionId);
		if (!parentRef) throw new Error("this session has no parent (not an rlm child)");
		const parentInfo = api.sessionInfo(parentRef.parentSessionId);
		return {
			sessionId: parentRef.parentSessionId,
			sessionDir: parentInfo?.sessionDir ?? "",
			name: parentInfo ? undefined : "parent",
		};
	}
	if (!name) throw new Error(`receiver_name is required for ${role} messages`);
	if (role === "child") {
		const entry = api.findChildByName(info.sessionId, name);
		if (!entry) throw new Error(`no child named "${name}"`);
		if (!entry.session_id) throw new Error(`child "${name}" has no bound session id`);
		return {
			sessionId: entry.session_id,
			sessionDir: entry.session_dir,
			childId: entry.rlm_child_id,
			name,
			parentSessionId: info.sessionId,
		};
	}
	// sibling
	const parentRef = api.getParentRef(info.sessionId);
	if (!parentRef) throw new Error("this session has no parent (no siblings)");
	const entry = api
		.listChildren(parentRef.parentSessionId)
		.find((c) => c.session_name === name && c.session_id !== info.sessionId);
	if (!entry) throw new Error(`no sibling named "${name}"`);
	if (!entry.session_id) throw new Error(`sibling "${name}" has no bound session id`);
	return {
		sessionId: entry.session_id,
		sessionDir: entry.session_dir,
		childId: entry.rlm_child_id,
		name,
		parentSessionId: parentRef.parentSessionId,
	};
}

/** Relationship of the SENDER from the RECEIVER's point of view. */
function relationshipFor(role: ReceiverRole): FamilyRelationship {
	if (role === "parent") return "child";
	if (role === "child") return "parent";
	return "sibling";
}

// ---- delivery ------------------------------------------------------------------

function awaitChildSettled(session: AgentSession, timeoutMs: number): Promise<boolean> {
	if (session.isIdle) return Promise.resolve(true);
	return new Promise((resolve) => {
		const timer = setTimeout(() => {
			unsub();
			resolve(false);
		}, timeoutMs);
		const unsub = session.subscribe((event) => {
			if (event.type === "agent_settled") {
				clearTimeout(timer);
				unsub();
				resolve(true);
			}
		});
	});
}

interface DeliveryOutcome {
	status: "delivered" | "queued" | "failed" | "persisted";
	deliveryMode?: "steer" | "triggerTurn" | "deferred";
	settled?: boolean;
	error?: string;
}

async function deliverToTarget(
	payload: AgentMessagePayload,
	target: ResolvedTarget,
	deferFinalize?: (status: "delivered" | "failed", error?: string) => void,
): Promise<DeliveryOutcome> {
	const api = requirePrimeRlmApi();
	const message = createAgentMessage(payload);
	if (target.childId === undefined) {
		// parent delivery: through the parent's captured extension API
		const binding = bindings.get(target.sessionId);
		if (!binding) {
			return { status: "failed", error: "parent session is not bound in this process" };
		}
		if (binding.ctx.isIdle()) {
			// pi.sendMessage is fire-and-forget; with triggerTurn it runs a fresh
			// turn on an idle host (E1-verified on rpc mode).
			binding.pi.sendMessage(message, { triggerTurn: true });
			return { status: "delivered", deliveryMode: "triggerTurn" };
		}
		// Busy parent: DEFER to the next settle so the reply starts a real
		// follow-up turn instead of being silently absorbed into the in-flight
		// run (or parking in the post-drain window).
		deferParentDelivery(target.sessionId, message, deferFinalize ?? (() => {}));
		return { status: "queued", deliveryMode: "deferred" };
	}
	// child/sibling delivery: through the retained live AgentSession
	const parentSessionId = target.parentSessionId;
	if (!parentSessionId) {
		return { status: "failed", error: "target has no recorded parent session" };
	}
	const live = api.getLiveChild(parentSessionId, target.childId);
	if (!live) {
		return { status: "failed", error: "target session is not live in this process" };
	}
	const session = live.session;
	if (session.isIdle) {
		const settledPromise = awaitChildSettled(session, SETTLE_TIMEOUT_MS);
		await session.sendCustomMessage(message, { triggerTurn: true });
		const settled = await settledPromise;
		return { status: "delivered", deliveryMode: "triggerTurn", settled };
	}
	await session.sendCustomMessage(message, { triggerTurn: true, deliverAs: "steer" });
	// Mid-run steer into a busy child (m3-r2 semantics, unchanged) — tracked so
	// the park window (enqueued after the loop's final drain check) is repaired
	// at settle with a one-shot triggerTurn re-issue.
	trackSteerDelivery(target.sessionId, message);
	return { status: "delivered", deliveryMode: "steer" };
}

function buildPayload(
	sender: AgentMessageSender,
	target: ResolvedTarget,
	role: ReceiverRole,
	messageText: string,
): AgentMessagePayload {
	return {
		id: newAgentMessageId(),
		source: AGENT_MESSAGE_SOURCE,
		message: messageText,
		from: sender,
		fromRelationship: relationshipFor(role),
		target: { sessionId: target.sessionId, name: target.name },
	};
}

async function sendOne(
	api: PrimeRlmHostApi,
	info: PrimeRlmSessionInfo,
	role: ReceiverRole,
	name: string | undefined,
	messageText: string,
): Promise<AgentMessageReceipt> {
	const sender = senderRef(api, info);
	const outboxPath = outboxPathFor(info.sessionDir, info.sessionId);
	let target: ResolvedTarget;
	try {
		target = resolveTarget(api, info, role, name);
	} catch (error) {
		const payload = buildPayload(sender, { sessionId: "unresolved", sessionDir: "", name }, role, messageText);
		appendOutboxRecord(outboxPath, {
			id: payload.id,
			ts: new Date().toISOString(),
			from: sender,
			role,
			receiverName: name,
			message: messageText,
			status: "failed",
			detail: errorMessage(error),
		});
		notifyMessageListeners({
			fromSessionId: info.sessionId,
			fromName: sender.name,
			role,
			receiverName: name,
			targetSessionId: undefined,
			message: messageText,
			deliveryStatus: "failed",
		});
		return createAgentMessageReceipt(payload, "failed", { error: errorMessage(error) });
	}

	const payload = buildPayload(sender, target, role, messageText);
	appendOutboxRecord(outboxPath, {
		id: payload.id,
		ts: new Date().toISOString(),
		from: sender,
		role,
		receiverName: name,
		target,
		message: messageText,
		status: "queued",
	});

	if (DELIVERY_DELAY_MS > 0) {
		// Test knob (C2): window in which a kill -9 leaves a queued record behind.
		await sleep(DELIVERY_DELAY_MS);
	}

	// Deferred sends (busy parent): the terminal outbox record + listener
	// notification are written by the settle drain via this finalize closure;
	// the "queued" record above stays open until then (restart-safe re-drive).
	const deferFinalize = (status: "delivered" | "failed", error?: string) => {
		appendTerminalOutboxRecord(info.sessionDir, outboxPath, {
			id: payload.id,
			ts: new Date().toISOString(),
			from: sender,
			role,
			receiverName: name,
			target,
			message: messageText,
			status,
			detail: error,
		});
		notifyMessageListeners({
			fromSessionId: info.sessionId,
			fromName: sender.name,
			role,
			receiverName: name,
			targetSessionId: target.sessionId,
			message: messageText,
			deliveryStatus: status,
		});
	};

	const outcome = await deliverToTarget(payload, target, deferFinalize);
	if (outcome.deliveryMode === "deferred") {
		// Notify as "queued" now so listeners (rlm-host reply tracking) know the
		// child answered even though delivery lands at the parent's next settle.
		notifyMessageListeners({
			fromSessionId: info.sessionId,
			fromName: sender.name,
			role,
			receiverName: name,
			targetSessionId: target.sessionId,
			message: messageText,
			deliveryStatus: "queued",
		});
		return createAgentMessageReceipt(payload, "queued", { deliveryMode: "deferred" });
	}
	appendOutboxRecord(outboxPath, {
		id: payload.id,
		ts: new Date().toISOString(),
		from: sender,
		role,
		receiverName: name,
		target,
		message: messageText,
		status: outcome.status,
		detail: outcome.error,
	});
	notifyMessageListeners({
		fromSessionId: info.sessionId,
		fromName: sender.name,
		role,
		receiverName: name,
		targetSessionId: target.sessionId,
		message: messageText,
		deliveryStatus: outcome.status,
	});
	return createAgentMessageReceipt(payload, outcome.status, {
		deliveryMode: outcome.deliveryMode,
		settled: outcome.settled,
		error: outcome.error,
	});
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

// ---- public API seam (v1) ------------------------------------------------------
// Published on globalThis (same reasoning as prime-rlm's api.ts: jiti gives each
// extension its own module instance, so globalThis is the only process-wide,
// identity-stable channel). prime-rlm uses this to deliver child run outcomes
// AS the child (outbox record + wakeup + reply tracking) and to skip the host
// fallback notice when a child already replied to its parent.

export interface CommsMessageEvent {
	fromSessionId: string;
	fromName?: string;
	role: ReceiverRole;
	receiverName?: string;
	targetSessionId?: string;
	message: string;
	deliveryStatus: "delivered" | "queued" | "failed" | "persisted";
}

export interface PrimeCommsHostApi {
	version: 1;
	/** Send a message AS the given session through the outbox+delivery machinery. */
	sendAs(
		fromSessionId: string,
		role: ReceiverRole,
		message: string,
		receiverName?: string,
	): Promise<AgentMessageReceipt>;
	/** Listener invoked after each send attempt in this process. */
	registerMessageListener(fn: (event: CommsMessageEvent) => void): void;
	/** Whether the given session is bound (live) in this process. */
	isBound(sessionId: string): boolean;
}

const messageListeners = new Set<(event: CommsMessageEvent) => void>();

function notifyMessageListeners(event: CommsMessageEvent): void {
	for (const fn of messageListeners) {
		try {
			fn(event);
		} catch {
			// listener errors must not break message delivery
		}
	}
	// M11a: ONE pi wire emission per terminal delivery transition, from the
	// SENDER's binding (best-effort — the binding is gone after host crash, and
	// the redrive path intentionally does not notify). appendEntry persists to
	// the sender's session file AND emits entry_appended → bridge comms.message
	// event → transcript annotation. "queued" is excluded: the terminal status
	// is the annotation worth keeping; comms.list reads the outbox for detail.
	if (event.deliveryStatus !== "queued") {
		try {
			bindings.get(event.fromSessionId)?.pi.appendEntry("prime_comms_message", {
				fromSessionId: event.fromSessionId,
				fromName: event.fromName,
				role: event.role,
				receiverName: event.receiverName,
				targetSessionId: event.targetSessionId,
				message: event.message,
				deliveryStatus: event.deliveryStatus,
				ts: new Date().toISOString(),
			});
		} catch {
			// emission must never break delivery
		}
	}
}

const primeCommsHostApi: PrimeCommsHostApi = {
	version: 1,
	async sendAs(fromSessionId, role, message, receiverName) {
		const api = requirePrimeRlmApi();
		const info = api.sessionInfo(fromSessionId);
		if (!info) {
			throw new Error(`prime-comms sendAs: session ${fromSessionId} is not registered (no ctx bound)`);
		}
		return sendOne(api, info, role, receiverName, message);
	},
	registerMessageListener(fn) {
		messageListeners.add(fn);
	},
	isBound(sessionId) {
		return bindings.has(sessionId);
	},
};

(globalThis as Record<symbol, unknown>)[Symbol.for("prime-comms.host-api")] = primeCommsHostApi;

// ---- outbox re-drive (session_start) -------------------------------------------

function findSessionFile(sessionDir: string, sessionId: string): string | undefined {
	try {
		const files = readdirSync(sessionDir).filter((f) => f.endsWith(".jsonl"));
		const exact = files.find((f) => f.includes(sessionId));
		if (exact) return join(sessionDir, exact);
		if (files.length === 1) return join(sessionDir, files[0]);
	} catch {
		// fall through
	}
	return undefined;
}

async function redriveOutbox(pi: ExtensionAPI, ctx: ExtensionContext): Promise<void> {
	const sessionId = ctx.sessionManager.getSessionId();
	const sessionDir = ctx.sessionManager.getSessionDir();
	const outboxPath = outboxPathFor(sessionDir, sessionId);
	const pending = pendingOutboxRecords(outboxPath);
	if (pending.length === 0) return;

	for (const record of pending) {
		const target = record.target;
		const sender: AgentMessageSender = { sessionId: record.from.sessionId, name: record.from.name };

		// Tier 1: target live in this process → deliver normally.
		if (target) {
			const payload: AgentMessagePayload = {
				id: record.id,
				source: AGENT_MESSAGE_SOURCE,
				message: record.message,
				from: sender,
				fromRelationship: relationshipFor(record.role),
				target: { sessionId: target.sessionId, name: target.name },
			};
			const outcome = await deliverToTarget(payload, target, (status, error) => {
				appendTerminalOutboxRecord(sessionDir, outboxPath, { ...record, ts: new Date().toISOString(), status, detail: error });
			});
			if (outcome.deliveryMode === "deferred") {
				// Busy target: queued record stays open until the settle drain
				// finalizes it (or a later re-drive retries after a restart).
				continue;
			}
			if (outcome.status === "delivered") {
				appendOutboxRecord(outboxPath, { ...record, ts: new Date().toISOString(), status: "delivered" });
				continue;
			}
			// Tier 2: target dead → append the message entry directly into its
			// session file so it appears when that session is resumed.
			const file = target.sessionDir ? findSessionFile(target.sessionDir, target.sessionId) : undefined;
			if (file && existsSync(file)) {
				try {
					const sm = SessionManager.open(file);
					sm.appendMessage(createAgentMessage(payload));
					appendOutboxRecord(outboxPath, {
						...record,
						ts: new Date().toISOString(),
						status: "persisted",
						detail: file,
					});
					continue;
				} catch (error) {
					appendOutboxRecord(outboxPath, {
						...record,
						ts: new Date().toISOString(),
						status: "failed",
						detail: `persist failed: ${errorMessage(error)}`,
					});
					continue;
				}
			}
		}

		// Tier 3: unresolvable → recover into the sender's own context.
		const notice = createAgentMessage({
			id: `${record.id}_recovery`,
			source: AGENT_MESSAGE_SOURCE,
			message: [
				"An outbound agent message from this session could not be delivered after a host restart.",
				`Original message id: ${record.id}`,
				`Intended receiver: ${record.role}${record.receiverName ? `:${record.receiverName}` : ""}`,
				"Original message:",
				record.message,
			].join("\n"),
			from: { sessionId },
			target: { sessionId },
		});
		// eslint-disable-next-line @typescript-eslint/no-explicit-any
		(pi.sendMessage as any)(
			{ ...notice, customType: OUTBOX_RECOVERY_CUSTOM_TYPE },
			{ triggerTurn: false },
		);
		appendOutboxRecord(outboxPath, { ...record, ts: new Date().toISOString(), status: "recovered" });
	}
}

// ---- kernel host handlers --------------------------------------------------------

function handleListAgents(info: PrimeRlmSessionInfo): Record<string, unknown> {
	const api = requirePrimeRlmApi();
	const { parentRef, children, siblings } = familyOf(api, info.sessionId);
	const describeChild = (entry: ChildRegistryEntry, role: string) => ({
		role,
		name: entry.session_name,
		sessionId: entry.session_id,
		sessionDir: entry.session_dir,
		status: entry.status,
		// M7: subagent role (registry `role` field); named subagentRole to avoid
		// clashing with the relationship `role` param above.
		subagentRole: entry.role ?? null,
		live: entry.session_id ? bindings.has(entry.session_id) : false,
	});
	return {
		self: {
			sessionId: info.sessionId,
			name: parentRef?.name,
			cwd: info.cwd,
			depth: info.depth,
			sessionDir: info.sessionDir,
		},
		parent: parentRef
			? { role: "parent", sessionId: parentRef.parentSessionId, live: bindings.has(parentRef.parentSessionId) }
			: undefined,
		siblings: siblings.map((entry) => describeChild(entry, "sibling")),
		children: children.map((entry) => describeChild(entry, "child")),
	};
}

async function handleSend(
	info: PrimeRlmSessionInfo,
	payload: Record<string, unknown>,
): Promise<Record<string, unknown>> {
	const api = requirePrimeRlmApi();
	if (payload.target === "all") {
		if (typeof payload.message !== "string") throw new Error("broadcast message must be a string");
		const { parentRef, children, siblings } = familyOf(api, info.sessionId);
		const receipts: AgentMessageReceipt[] = [];
		if (parentRef) receipts.push(await sendOne(api, info, "parent", undefined, payload.message));
		for (const child of children) {
			receipts.push(await sendOne(api, info, "child", child.session_name, payload.message));
		}
		for (const sibling of siblings) {
			receipts.push(await sendOne(api, info, "sibling", sibling.session_name, payload.message));
		}
		return { receipts } as unknown as Record<string, unknown>;
	}
	const message = payload.message;
	const role = payload.receiver_role;
	// The kernel shim sends receiver_name:null for parent messages; treat null
	// like undefined (only real junk is rejected).
	const nameRaw = payload.receiver_name;
	if (nameRaw !== undefined && nameRaw !== null && typeof nameRaw !== "string") {
		throw new Error("agent_message.send receiver_name must be a string");
	}
	const name = typeof nameRaw === "string" ? nameRaw : undefined;
	if (typeof message !== "string") throw new Error("agent_message.send message must be a string");
	if (role !== "parent" && role !== "sibling" && role !== "child") {
		throw new Error('agent_message.send receiver_role must be "parent", "sibling", or "child"');
	}
	const receipt = await sendOne(api, info, role, name, message);
	return receipt as unknown as Record<string, unknown>;
}

// ---- observation -----------------------------------------------------------------

interface ObserveTarget {
	sessionId: string;
	name?: string;
	role: string;
}

function resolveObserveTarget(api: PrimeRlmHostApi, info: PrimeRlmSessionInfo, target: string): ObserveTarget {
	if (target === "self") return { sessionId: info.sessionId, role: "self", name: api.getParentRef(info.sessionId)?.name };
	if (target === "parent") {
		const parentRef = api.getParentRef(info.sessionId);
		if (!parentRef) throw new Error("this session has no parent");
		return { sessionId: parentRef.parentSessionId, role: "parent" };
	}
	const { children, siblings } = familyOf(api, info.sessionId);
	const child = children.find((c) => c.session_name === target || c.session_id === target);
	if (child) {
		if (!child.session_id) throw new Error(`child "${target}" has no bound session id`);
		return { sessionId: child.session_id, name: child.session_name, role: "child" };
	}
	const sibling = siblings.find((c) => c.session_name === target || c.session_id === target);
	if (sibling) {
		if (!sibling.session_id) throw new Error(`sibling "${target}" has no bound session id`);
		return { sessionId: sibling.session_id, name: sibling.session_name, role: "sibling" };
	}
	throw new Error(`no visible session named "${target}" (use "self", "parent", or a child/sibling name)`);
}

function handleObserveGet(info: PrimeRlmSessionInfo, payload: Record<string, unknown>): Record<string, unknown> {
	const api = requirePrimeRlmApi();
	const target = payload.target;
	if (typeof target !== "string") throw new Error("agent_observe.get target must be a string");
	const resolved = resolveObserveTarget(api, info, target);
	const binding = bindings.get(resolved.sessionId);
	const sessionInfo = api.sessionInfo(resolved.sessionId);
	const entries = binding ? binding.ctx.sessionManager.getEntries() : undefined;
	return {
		sessionId: resolved.sessionId,
		name: resolved.name,
		role: resolved.role,
		live: binding !== undefined,
		idle: binding ? binding.ctx.isIdle() : undefined,
		cwd: sessionInfo?.cwd,
		sessionDir: sessionInfo?.sessionDir,
		messageCount: entries?.length,
		model: binding ? binding.ctx.model?.id : undefined,
	};
}

function messagePreviewText(message: unknown, maxChars: number): string {
	const content = (message as { content?: unknown }).content;
	let text: string;
	if (typeof content === "string") {
		text = content;
	} else if (Array.isArray(content)) {
		text = content
			.filter((part) => typeof part === "object" && part !== null && (part as { type?: unknown }).type === "text")
			.map((part) => String((part as { text?: unknown }).text ?? ""))
			.join("\n");
	} else {
		text = JSON.stringify(content);
	}
	text = text.replace(/\s+/g, " ").trim();
	return text.length > maxChars ? `${text.slice(0, maxChars)}…` : text;
}

function handleObserveRecent(info: PrimeRlmSessionInfo, payload: Record<string, unknown>): Record<string, unknown> {
	const api = requirePrimeRlmApi();
	const target = payload.target;
	if (typeof target !== "string") throw new Error("agent_observe.recent target must be a string");
	const limit = typeof payload.limit === "number" ? payload.limit : 8;
	const maxChars = typeof payload.max_chars === "number" ? payload.max_chars : 800;
	if (limit < 1 || limit > 50) throw new Error("limit must be between 1 and 50");
	if (maxChars < 80 || maxChars > 2000) throw new Error("max_chars must be between 80 and 2000");
	const resolved = resolveObserveTarget(api, info, target);
	const binding = bindings.get(resolved.sessionId);
	if (!binding) {
		throw new Error(`session "${target}" is not live in this process (observation requires a live session)`);
	}
	const messages = binding.ctx.sessionManager.buildContextEntries().flatMap((entry) => sessionEntryToContextMessages(entry));
	const recent = messages.slice(-limit);
	return {
		sessionId: resolved.sessionId,
		name: resolved.name,
		role: resolved.role,
		messages: recent.map((message) => ({
			role: (message as { role?: unknown }).role,
			preview: messagePreviewText(message, maxChars),
		})),
		total: messages.length,
	};
}

// ---- extension entry ---------------------------------------------------------

let seamRegistered = false;

function registerSeam(): void {
	if (seamRegistered) return;
	const api = getPrimeRlmApi();
	if (!api) return; // prime-rlm absent: extension inert (documented hard dep)
	seamRegistered = true;
	api.registerKernelBootstrapContributor(() => ({ python: COMMS_KERNEL_PYTHON }));
	api.registerHostHandlerProvider((info) => ({
		"agent_message.send": (payload) => handleSend(info, payload),
		"agent_message.list_agents": async () => handleListAgents(info),
		"agent_observe.list": async () => handleListAgents(info),
		"agent_observe.get": async (payload) => handleObserveGet(info, payload),
		"agent_observe.recent": async (payload) => handleObserveRecent(info, payload),
	}));
}

/** Prompt section: the agent_message/agent_observe contract + reply doctrine.
 * Appended via before_agent_start chaining (prime-rlm's base prompt stays
 * messaging-free; this section only exists when prime-comms is loaded). */
const COMMS_PROMPT_SECTION = [
	"Agent messaging: your kernel provides `agent_message` and `agent_observe` modules.",
	"`await agent_message.send(message, receiver_role=\"parent\"|\"child\"|\"sibling\", receiver_name=...)`",
	"delivers a message to a family member; delivery to your parent, or to an idle child, wakes the",
	"receiver up as a new turn. `agent_message.list_agents()` shows your reachable family;",
	"`agent_observe.get(target)` / `agent_observe.recent(target)` inspect a live session.",
	"",
	"If you are a child agent (task prompts labeled `[task from parent]`): when a task calls for an",
	'answer, reply explicitly with `await agent_message.send(message, receiver_role="parent")` — your',
	"final assistant text is NOT shown to your parent by itself. Not every message or task needs a",
	"reply; continue cleanup after sending and go idle normally.",
	"",
	"If you spawned children: their answers arrive as `[from child:<name>]` messages that start new",
	"turns. Do not busy-poll `rlm.list_subagents()` waiting for them; end your turn and let the",
	"wakeups arrive.",
].join("\n");

export default function primeComms(pi: ExtensionAPI): void {
	registerSeam();
	registerDeliveryGuards(pi);

	pi.on("before_agent_start", async (event) => ({
		systemPrompt: `${event.systemPrompt}\n\n${COMMS_PROMPT_SECTION}`,
	}));

	pi.on("session_start", async (_event, ctx) => {
		const sessionId = ctx.sessionManager.getSessionId();
		bindings.set(sessionId, { pi, ctx });
		try {
			await redriveOutbox(pi, ctx);
		} catch (error) {
			ctx.ui.notify(`prime-comms outbox re-drive failed: ${errorMessage(error)}`, "warning");
		}
	});

	pi.on("session_shutdown", async (_event, ctx) => {
		const sessionId = ctx.sessionManager.getSessionId();
		bindings.delete(sessionId);
		// Resolve deferred sends targeting this session so sender outboxes
		// terminate (rlm-host outcome fallbacks can then fire).
		const deferred = deferredByTarget.get(sessionId) ?? [];
		deferredByTarget.delete(sessionId);
		for (const d of deferred) d.finalize("failed", "target session shut down before delivery");
		steerPendingByTarget.delete(sessionId);
	});
}
