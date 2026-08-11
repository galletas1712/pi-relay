// RLM child-session host: async rlm() spawn, multi-turn children, delete, and
// result delivery. Ported from prime-agent's agent-session.ts RLM blocks
// (admission-async `_startRlmChildRun`, `deleteRlmSubagent`, and the
// `_notifyParentOfRlmChildTerminal` result-notice flow in messages.ts).
//
// Semantics (M3):
// - `rlm.run` validates the request, creates the child's session dir, registers
//   a "running" entry, kicks a DETACHED task, and returns the handle
//   {rlm_child_id, name, session_dir, model} immediately — never the answer.
// - The child session+kernel are retained after the first settle so parents can
//   steer follow-up turns via agent_message (prime-comms).
// - When the initial run settles, the host delivers the outcome to the parent:
//   if the child already replied via agent_message nothing more is sent;
//   otherwise the final assistant text (or failure) is delivered AS the child
//   through prime-comms (parent wakeup), or — when prime-comms is not loaded —
//   injected directly into the parent session (rlm-only modularity, C3).
// - `rlm.delete_subagent` cancels a running child or removes a finished one:
//   session + kernel disposed, registry entry removed, session dir deleted.
import { randomUUID } from "node:crypto";
import { mkdirSync } from "node:fs";
import { rm } from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";
import { createAgentSession, SessionManager } from "@earendil-works/pi-coding-agent";
import type { AgentSession, AgentSessionEvent, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { KernelProvisioner } from "./ipython-tool.ts";
import type { HostRequestHandlers } from "./kernel.ts";
import type { ChildRegistryEntry, SessionState } from "./registry.ts";
import { disposeSessionState, registerPendingDepth, registerPendingParent, registerPendingRole } from "./registry.ts";
import { resolveRoleForSpawn, type SessionRoleSpec } from "./roles.ts";

/** Public API seam published by prime-comms (v1). Read lazily; when absent the
 * rlm-only fallback injects the result message directly into the parent. */
interface PrimeCommsHostApiLike {
	version: number;
	sendAs(
		fromSessionId: string,
		role: "parent" | "child" | "root" | "sibling",
		message: string,
		receiverName?: string,
	): Promise<unknown>;
	registerMessageListener(
		fn: (event: { fromSessionId: string; role: string; [key: string]: unknown }) => void,
	): void;
	isBound(sessionId: string): boolean;
}

function getCommsApi(): PrimeCommsHostApiLike | null {
	const value = (globalThis as Record<symbol, unknown>)[Symbol.for("prime-comms.host-api")];
	if (!value || typeof value !== "object") return null;
	const candidate = value as PrimeCommsHostApiLike;
	if (candidate.version !== 1) return null;
	return candidate;
}

/** One in-flight (or settled, still addressable) child run. */
interface ChildRun {
	parentSessionId: string;
	entry: ChildRegistryEntry;
	repliedToParent: boolean;
	deleted: boolean;
	resolveDeleted: () => void;
	deletedPromise: Promise<void>;
}

const runsByChildId = new Map<string, ChildRun>();
const runsByChildSessionId = new Map<string, ChildRun>();
let commsListenerRegistered = false;

function newChildRun(parentSessionId: string, entry: ChildRegistryEntry): ChildRun {
	let resolveDeleted!: () => void;
	const deletedPromise = new Promise<void>((r) => {
		resolveDeleted = r;
	});
	const run: ChildRun = {
		parentSessionId,
		entry,
		repliedToParent: false,
		deleted: false,
		resolveDeleted,
		deletedPromise,
	};
	runsByChildId.set(entry.rlm_child_id, run);
	return run;
}

/** Watch child→parent agent_message traffic so the host notice is skipped when
 * the child already delivered its own answer (PA's _parentReplyCount gate). */
function ensureCommsReplyListener(): void {
	if (commsListenerRegistered) return;
	const comms = getCommsApi();
	if (!comms) return; // prime-comms not loaded (or older); retry on next call
	comms.registerMessageListener((event) => {
		// "queued" counts: prime-comms defers busy-parent delivery to the
		// parent's next settle (the outbox machinery guarantees the terminal
		// delivery), so the child HAS replied even though the wakeup lands later.
		if (event.role !== "parent") return;
		if (event.deliveryStatus !== "delivered" && event.deliveryStatus !== "queued") return;
		const run = runsByChildSessionId.get(event.fromSessionId);
		if (run) run.repliedToParent = true;
	});
	commsListenerRegistered = true;
}

/** Parent session is going away: mark its runs deleted so the detached tasks
 * stop without delivering; registry.disposeSessionState handles sessions/kernels. */
export function cleanupRunsForParent(parentSessionId: string): void {
	for (const run of [...runsByChildId.values()]) {
		if (run.parentSessionId !== parentSessionId) continue;
		run.deleted = true;
		run.resolveDeleted();
		if (run.entry.session_id) runsByChildSessionId.delete(run.entry.session_id);
		runsByChildId.delete(run.entry.rlm_child_id);
	}
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

// ---- M4 (O1): subagent lifecycle observability ------------------------------
// rpc-visible session entries (never LLM context). PA emits
// "rlm_child_lifecycle" session events; pi extensions can't add AgentSessionEvent
// types, so the equivalent is appendEntry + the entry_appended rpc event.
const RLM_CHILD_LIFECYCLE_CUSTOM_TYPE = "rlm_child_lifecycle";

function emitChildLifecycle(
	state: SessionState,
	phase: "admitted" | "completed" | "error" | "deleted",
	entry: ChildRegistryEntry,
	extra?: Record<string, unknown>,
): void {
	try {
		state.pi?.appendEntry(RLM_CHILD_LIFECYCLE_CUSTOM_TYPE, {
			phase,
			rlm_child_id: entry.rlm_child_id,
			session_name: entry.session_name,
			session_id: entry.session_id,
			status: entry.status,
			model: entry.model ?? null,
			created_at: entry.created_at,
			...(extra ?? {}),
		});
	} catch (error) {
		console.warn(`[prime-rlm] failed to record child lifecycle entry: ${errorMessage(error)}`);
	}
}

// ---- M4 (R4): rlm.find_models ------------------------------------------------
// Ported from prime-agent packages/coding-agent/src/core/rlm-runtime.ts.
const DEFAULT_RLM_MODEL_SEARCH_LIMIT = 8;
const MAX_RLM_MODEL_SEARCH_LIMIT = 20;

function normalizeModelSearchText(value: string): string {
	return value.toLowerCase().replace(/[^a-z0-9]+/g, "");
}

interface RlmModelLike {
	provider: string;
	id: string;
	name?: string;
}

function findRlmModelMatches(query: string, models: RlmModelLike[], limit: number): Record<string, string>[] {
	const normalizedQuery = normalizeModelSearchText(query.trim());
	return models
		.map((model) => {
			const selector = `${model.provider}/${model.id}`;
			const fields = [selector, model.id, model.name || model.id];
			const normalizedFields = fields.map(normalizeModelSearchText);
			let score = normalizedQuery ? Number.POSITIVE_INFINITY : 0;
			if (normalizedQuery) {
				const exactIndex = normalizedFields.indexOf(normalizedQuery);
				const prefixIndex = normalizedFields.findIndex((field) => field.startsWith(normalizedQuery));
				const partialIndex = normalizedFields.findIndex((field) => field.includes(normalizedQuery));
				if (exactIndex >= 0) score = exactIndex;
				else if (prefixIndex >= 0) score = 3 + prefixIndex;
				else if (partialIndex >= 0) score = 6 + partialIndex;
			}
			return { model, selector, score };
		})
		.filter((candidate) => Number.isFinite(candidate.score))
		.sort((a, b) => a.score - b.score || a.selector.localeCompare(b.selector))
		.slice(0, limit)
		.map(({ model, selector }) => ({
			provider: model.provider,
			id: model.id,
			name: model.name || model.id,
			selector,
		}));
}

/** Models whose provider currently has credentials (PA's authenticated-only
 * filter, adapted: pi extensions see getApiKeyForProvider, not auth status). */
async function authenticatedModels(ctx: ExtensionContext): Promise<RlmModelLike[]> {
	const all = ctx.modelRegistry.getAll();
	const providers = [...new Set(all.map((m) => m.provider))];
	const authed = new Set<string>();
	await Promise.all(
		providers.map(async (provider) => {
			try {
				if (await ctx.modelRegistry.getApiKeyForProvider(provider)) authed.add(provider);
			} catch {
				// unauthenticated providers are simply excluded
			}
		}),
	);
	return all.filter((m) => authed.has(m.provider));
}

const RESULT_TEXT_LIMIT = 8000;

function clipResultText(text: string | null): string {
	if (!text) return "(the child produced no final assistant text)";
	return text.length > RESULT_TEXT_LIMIT ? `${text.slice(0, RESULT_TEXT_LIMIT)}… [truncated]` : text;
}

async function createChildSession(
	state: SessionState,
	childDir: string,
	modelOverride?: { provider: string; id: string },
	rolePolicy?: { thinkingLevel?: string; maxTokens?: number },
): Promise<AgentSession> {
	const ctx = state.ctx!;
	let model = ctx.model;
	if (modelOverride) {
		const found = ctx.modelRegistry.find(modelOverride.provider, modelOverride.id);
		if (!found) {
			throw new Error(`Unknown model for rlm() spawn: ${modelOverride.provider}/${modelOverride.id}`);
		}
		model = found;
	}
	// M7: role max_tokens overrides the resolved model's output cap for the
	// child session only (pi-relay ProviderConfig.max_tokens port; Model is a
	// plain record, so a shallow clone is safe).
	if (rolePolicy?.maxTokens !== undefined && model) {
		model = { ...model, maxTokens: rolePolicy.maxTokens };
	}
	// No settingsManager/modelRuntime passing: the child builds its own from the
	// same agentDir (same settings.json -> same default model + extensions).
	const { session } = await createAgentSession({
		cwd: ctx.cwd,
		agentDir: process.env.PI_CODING_AGENT_DIR,
		sessionManager: SessionManager.create(ctx.cwd, childDir),
		model,
		// M7: role reasoning_effort (pi-relay port; pi clamps to model capabilities).
		...(rolePolicy?.thinkingLevel ? { thinkingLevel: rolePolicy.thinkingLevel as never } : {}),
	});
	return session;
}

interface AssistantMessageLike {
	role?: string;
	stopReason?: string;
	content?: Array<{ type: string; text?: string }>;
}

function lastAssistant(events: AgentSessionEvent[]): AssistantMessageLike | null {
	for (let i = events.length - 1; i >= 0; i--) {
		const e = events[i];
		if (e.type === "message_end" && (e.message as AssistantMessageLike).role === "assistant") {
			return e.message as AssistantMessageLike;
		}
	}
	return null;
}

function assistantText(message: AssistantMessageLike | null): string | null {
	if (!message) return null;
	return (message.content ?? [])
		.filter((c) => c.type === "text")
		.map((c) => c.text ?? "")
		.join("\n");
}

/** Deliver a terminal child outcome to the parent. Prefers prime-comms (message
 * recorded in the child's outbox, formatted `[from child:...]`, wakes the
 * parent); falls back to a direct custom-message injection so rlm-only setups
 * (C3) still get the wakeup + text. Never throws. */
async function deliverChildOutcome(state: SessionState, entry: ChildRegistryEntry, body: string): Promise<void> {
	const comms = getCommsApi();
	if (comms && entry.session_id) {
		try {
			await comms.sendAs(entry.session_id, "parent", body);
			return;
		} catch (error) {
			console.warn(
				`[prime-rlm] comms delivery of child outcome failed, using direct injection: ${errorMessage(error)}`,
			);
		}
	}
	const pi = state.pi;
	if (!pi) {
		console.warn(`[prime-rlm] cannot deliver child outcome for ${entry.rlm_child_id}: no comms seam and no session API`);
		return;
	}
	pi.sendMessage(
		{
			customType: "rlm_child_result",
			content: `[from child:${entry.session_name}]\n${body}`,
			display: true,
		},
		{ triggerTurn: true },
	);
}

function completionNotice(entry: ChildRegistryEntry, finalText: string | null): string {
	return (
		`RLM child ${entry.session_name} (${entry.rlm_child_id}) completed without an explicit agent_message reply. ` +
		`Final assistant text:\n${clipResultText(finalText)}`
	);
}

function failureNotice(entry: ChildRegistryEntry, error: unknown): string {
	return `RLM child ${entry.session_name} (${entry.rlm_child_id}) failed: ${errorMessage(error)}`;
}

/** Detached child run: create, bind, prompt, await the first settle (or
 * deletion), then retain the session for follow-up turns and deliver the
 * outcome to the parent. */
async function runRlmChildDetached(
	state: SessionState,
	entry: ChildRegistryEntry,
	run: ChildRun,
	prompt: string,
	modelOverride?: { provider: string; id: string },
	roleSpawn?: { role: SessionRoleSpec; thinkingLevel?: string; maxTokens?: number },
): Promise<void> {
	let child: AgentSession | undefined;
	const events: AgentSessionEvent[] = [];
	let unsubscribe: () => void = () => {};
	try {
		const created = await createChildSession(state, entry.session_dir, modelOverride, roleSpawn);
		if (run.deleted) {
			// Deleted while the session was being created; the delete path owns cleanup.
			try {
				created.dispose();
			} catch {}
			return;
		}
		child = created;
		entry.session_id = child.sessionId;
		runsByChildSessionId.set(child.sessionId, run);
		// Depth + parent linkage must be registered BEFORE bindExtensions emits
		// session_start for the child (its handlers consume the pending maps).
		registerPendingDepth(child.sessionId, state.depth + 1);
		registerPendingParent(child.sessionId, {
			parentSessionId: state.sessionId,
			childId: entry.rlm_child_id,
			name: entry.session_name,
		});
		// M7: role snapshot consumed by the child's session_start (prompt +
		// kernel pre-imports). Same pending-map pattern as depth/parent above.
		if (roleSpawn) registerPendingRole(child.sessionId, roleSpawn.role);
		await child.bindExtensions({ mode: "rpc" });
		if (run.deleted) return;
		// Track from creation (not settle) so parents can steer a RUNNING child
		// via agent_message and disposeSessionState covers it on shutdown.
		state.liveChildren.set(entry.rlm_child_id, child);

		let resolveSettled!: (event: AgentSessionEvent) => void;
		const settledPromise = new Promise<AgentSessionEvent>((r) => {
			resolveSettled = r;
		});
		unsubscribe = child.subscribe((event) => {
			events.push(event);
			if (event.type === "agent_settled") resolveSettled(event);
		});
		await child.prompt(`[task from parent]\n\n${prompt}`, { source: "extension" });
		const final = await Promise.race([settledPromise, run.deletedPromise.then(() => null)]);
		if (final === null || run.deleted) return; // delete path owns cleanup

		const last = lastAssistant(events);
		const assistant = assistantText(last);
		const errored = last?.stopReason === "error";
		entry.status = errored ? "error" : "completed";
		entry.result_preview = assistant?.slice(0, 2000);
		if (errored) entry.error = "child agent ended with stopReason=error";
		emitChildLifecycle(state, errored ? "error" : "completed", entry, {
			result_preview: entry.result_preview ?? null,
			error: entry.error ?? null,
		});

		if (!run.repliedToParent) {
			await deliverChildOutcome(
				state,
				entry,
				errored ? failureNotice(entry, entry.error) : completionNotice(entry, assistant),
			);
		}
	} catch (error) {
		if (run.deleted) return;
		entry.status = "error";
		entry.error = errorMessage(error);
		emitChildLifecycle(state, "error", entry, { error: entry.error });
		state.liveChildren.delete(entry.rlm_child_id);
		try {
			child?.dispose();
		} catch {}
		if (entry.session_id) await disposeSessionState(entry.session_id);
		if (!run.repliedToParent) {
			await deliverChildOutcome(state, entry, failureNotice(entry, error));
		}
	} finally {
		unsubscribe();
	}
}

function subagentPayload(entry: ChildRegistryEntry): Record<string, unknown> {
	return {
		rlm_child_id: entry.rlm_child_id,
		active_session_id: null, // PA exposes the daemon-hosted session id; no daemon here
		session_id: entry.session_id,
		session_name: entry.session_name,
		session_dir: entry.session_dir,
		status: entry.status,
		model: entry.model ?? null,
		role: entry.role ?? null,
		created_at: entry.created_at,
		result_preview: entry.result_preview ?? null,
		error: entry.error ?? null,
	};
}

/** Resolve a delete target (rlm_child_id | session_id | exact session_name). */
function resolveDeleteTarget(state: SessionState, target: string): ChildRegistryEntry {
	const matches = [...state.children.values()].filter(
		(c) => c.rlm_child_id === target || c.session_id === target || c.session_name === target,
	);
	if (matches.length === 0) {
		throw new Error(`No RLM child matches "${target}" (match by rlm_child_id, session_id, or exact session_name)`);
	}
	if (matches.length > 1) {
		throw new Error(`"${target}" matches ${matches.length} RLM children; use the unique rlm_child_id instead`);
	}
	return matches[0];
}

export function rlmMaxDepth(): number {
	return Number(process.env.RLM_MAX_DEPTH ?? "1");
}

export function createRlmHostHandlers(deps: {
	getState: () => SessionState;
	provisionerFor: () => KernelProvisioner | undefined;
}): HostRequestHandlers {
	const { getState, provisionerFor } = deps;
	return {
		// Admission-async spawn: returns the handle immediately; the child runs in
		// a detached task and its result arrives later as a message (wakeup).
		"rlm.run": async (payload) => {
			ensureCommsReplyListener();
			const state = getState();
			const prompt = typeof payload.prompt === "string" ? payload.prompt : null;
			if (!prompt) throw new Error("rlm.run requires a prompt string");
			const kwargs = (payload.kwargs ?? {}) as Record<string, unknown>;
			const allowedKwargs = new Set(["name", "model", "role"]);
			const unexpected = Object.keys(kwargs).filter((key) => !allowedKwargs.has(key));
			if (unexpected.length > 0) {
				throw new Error(`rlm() got unexpected keyword argument(s): ${unexpected.join(", ")}`);
			}
			const name = typeof kwargs.name === "string" && kwargs.name.trim() ? kwargs.name.trim() : undefined;
			let modelOverride: { provider: string; id: string } | undefined;
			if (kwargs.model !== undefined) {
				if (typeof kwargs.model !== "string" || !kwargs.model.includes("/")) {
					throw new Error('rlm() model must be a "provider/model" string');
				}
				const [provider, ...rest] = kwargs.model.split("/");
				modelOverride = { provider, id: rest.join("/") };
			}
			if (state.depth >= rlmMaxDepth()) {
				throw new Error(`rlm() recursion depth limit exceeded (max ${rlmMaxDepth()})`);
			}
			if (!state.ctx) {
				throw new Error("rlm() session metadata not available (session still initializing)");
			}
			const ctx: ExtensionContext = state.ctx;

			// M7: role-configured spawn (pi-relay parity). Resolution + validation
			// happen at ADMISSION so an unknown/invalid role fails the rlm.run
			// call itself (pi-relay role_not_found), never the detached task.
			// Model precedence (select_subagent_provider port): explicit caller
			// model -> role frontmatter model -> parent default.
			let roleSpawn: { role: SessionRoleSpec; thinkingLevel?: string; maxTokens?: number } | undefined;
			let roleModelFallback: string | undefined;
			if (kwargs.role !== undefined) {
				if (typeof kwargs.role !== "string" || !kwargs.role.trim()) {
					throw new Error("rlm() role must be a non-empty string");
				}
				const agentDir = process.env.PI_CODING_AGENT_DIR ?? path.join(homedir(), ".pi", "agent");
				const resolved = resolveRoleForSpawn(
					kwargs.role.trim(),
					{
						sessionId: state.sessionId,
						sessionDir: ctx.sessionManager.getSessionDir(),
						cwd: ctx.cwd,
						agentDir,
					},
					(provider, id) => !!ctx.modelRegistry.find(provider, id),
				);
				roleSpawn = {
					role: resolved.role,
					thinkingLevel: resolved.thinkingLevel,
					maxTokens: resolved.maxTokens,
				};
				roleModelFallback = resolved.modelFallbackReason;
				if (!modelOverride && resolved.modelOverride) modelOverride = resolved.modelOverride;
			}

			let childId = "";
			let childDir = "";
			for (let attempt = 0; attempt < 8; attempt++) {
				const candidate = randomUUID().slice(0, 8);
				const dir = path.join(ctx.sessionManager.getSessionDir(), `sub-${candidate}`);
				try {
					mkdirSync(dir, { recursive: false });
					childId = candidate;
					childDir = dir;
					break;
				} catch (error) {
					if ((error as NodeJS.ErrnoException).code === "EEXIST") continue;
					throw error;
				}
			}
			if (!childId) throw new Error("Could not allocate a child session id");

			const entry: ChildRegistryEntry = {
				rlm_child_id: childId,
				session_name: name ?? childId,
				session_dir: childDir,
				session_id: null,
				status: "running",
				created_at: new Date().toISOString(),
				model: modelOverride ? `${modelOverride.provider}/${modelOverride.id}` : undefined,
				role: roleSpawn?.role.name,
			};
			state.children.set(childId, entry);
			emitChildLifecycle(state, "admitted", entry, {
				prompt_chars: prompt.length,
				role: entry.role ?? null,
				role_origin: roleSpawn?.role.origin ?? null,
				role_model_fallback: roleModelFallback ?? null,
			});
			const run = newChildRun(state.sessionId, entry);
			void runRlmChildDetached(state, entry, run, prompt, modelOverride, roleSpawn).catch((error) => {
				console.warn(`[prime-rlm] detached child task escaped error handling: ${errorMessage(error)}`);
			});
			return {
				rlm_child_id: childId,
				name: entry.session_name,
				session_dir: childDir,
				model: entry.model ?? null,
				role: entry.role ?? null,
			};
		},

		"rlm.list_subagents": async () => {
			const state = getState();
			return { subagents: [...state.children.values()].map(subagentPayload) };
		},

		// M4 (R4): bounded authenticated model catalog search (PA parity).
		"rlm.find_models": async (payload) => {
			if (typeof payload.query !== "string") {
				throw new Error("rlm.find_models query must be a string");
			}
			const limit = payload.limit === undefined ? DEFAULT_RLM_MODEL_SEARCH_LIMIT : payload.limit;
			if (!Number.isInteger(limit) || (limit as number) < 1 || (limit as number) > MAX_RLM_MODEL_SEARCH_LIMIT) {
				throw new Error(`rlm.find_models limit must be an integer from 1 to ${MAX_RLM_MODEL_SEARCH_LIMIT}`);
			}
			const state = getState();
			if (!state.ctx) throw new Error("rlm.find_models session metadata not available");
			const models = await authenticatedModels(state.ctx);
			return { models: findRlmModelMatches(payload.query, models, limit as number) };
		},

		// M4: current model identity for kernel skills (attach-image input check).
		"model.info": async () => {
			const state = getState();
			const model = state.ctx?.model;
			return {
				id: model?.id ?? null,
				provider: model?.provider ?? null,
				input: model?.input ?? [],
			};
		},

		// Cancel a running child or remove a finished one; dispose session+kernel,
		// drop the registry entry, and delete the child's session directory.
		"rlm.delete_subagent": async (payload) => {
			const state = getState();
			const target = typeof payload.target === "string" ? payload.target.trim() : "";
			if (!target) {
				throw new Error("rlm.delete_subagent requires a target (rlm_child_id, session_id, or session_name)");
			}
			const entry = resolveDeleteTarget(state, target);
			const run = runsByChildId.get(entry.rlm_child_id);

			// Tombstone first so the detached run task never delivers an outcome
			// for a deleted child.
			if (run) {
				run.deleted = true;
				run.resolveDeleted();
				if (entry.session_id) runsByChildSessionId.delete(entry.session_id);
				runsByChildId.delete(entry.rlm_child_id);
			}

			const live = state.liveChildren.get(entry.rlm_child_id);
			if (live) {
				try {
					await live.abort();
				} catch {}
				try {
					live.dispose();
				} catch {}
				state.liveChildren.delete(entry.rlm_child_id);
			}
			if (entry.session_id) {
				await disposeSessionState(entry.session_id);
			}
			state.children.delete(entry.rlm_child_id);
			emitChildLifecycle(state, "deleted", entry);
			await rm(entry.session_dir, { recursive: true, force: true });
			return { subagent: subagentPayload(entry), outcome: "deleted" };
		},

		// Deadlock-free scheduled semantics: these run as internal kernel cells
		// right AFTER the calling cell finishes, so the reply returns immediately.
		"rlm.snapshot_save": async () => {
			const manager = provisionerFor()?.manager;
			const cfg = manager?.snapshotConfig;
			if (!manager || !manager.isRunning || !cfg) {
				return { scheduled: false, path: cfg?.path ?? null, reason: "kernel not running or snapshots not configured" };
			}
			void manager.snapshotState();
			return { scheduled: true, path: cfg.path };
		},
		"rlm.snapshot_restore": async () => {
			const manager = provisionerFor()?.manager;
			const cfg = manager?.snapshotConfig;
			if (!manager || !manager.isRunning || !cfg) {
				return { scheduled: false, path: cfg?.path ?? null, reason: "kernel not running or snapshots not configured" };
			}
			void manager.restoreState();
			return { scheduled: true, path: cfg.path };
		},
	};
}
