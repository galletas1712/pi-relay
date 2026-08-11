// prime-rlm public host API (seam for sibling extensions, e.g. prime-harness,
// prime-comms). Published on globalThis because pi's extension loader uses jiti
// with moduleCache:false and one jiti instance per extension: importing this
// module from a sibling extension would produce a SECOND module instance with
// separate state. globalThis is the only process-wide, identity-stable channel.
//
// Consumers must resolve the API LAZILY (at call time, not at module scope) so
// load order does not matter, and must check `api.version === 1`.
//
// This file is the ONLY supported cross-extension interface to prime-rlm.
// Everything else in this package is private and may change without notice.

import type { AgentSession, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { HostRequestHandlers } from "./kernel.ts";
import type { ChildRegistryEntry, ParentRef, SessionState } from "./registry.ts";

export const PRIME_RLM_API_VERSION = 1;
export const PRIME_RLM_API_GLOBAL_KEY = Symbol.for("prime-rlm.host-api");

/** Per-session facts handed to contributors/providers at kernel start. */
export interface PrimeRlmSessionInfo {
	sessionId: string;
	/** pi's session storage dir for this session (shared for root sessions). */
	sessionDir: string;
	cwd: string;
	agentDir: string;
	depth: number;
}

/** A sibling extension's contribution to kernel startup. */
export interface KernelBootstrapContribution {
	/** Python executed as its own cell AFTER prime-rlm's core bootstrap. Failures
	 * are collected and surfaced as warnings; they do not kill the kernel. */
	python?: string;
	/** Extra environment for the kernel process (merged over process.env). */
	env?: Record<string, string>;
}

export type KernelBootstrapContributor = (
	info: PrimeRlmSessionInfo,
) => KernelBootstrapContribution | undefined;

export type HostHandlerProvider = (info: PrimeRlmSessionInfo) => HostRequestHandlers | undefined;

/** A retained (post-settle) in-process child session. */
export interface LiveChild {
	entry: ChildRegistryEntry;
	session: AgentSession;
}

export interface PrimeRlmHostApi {
	version: typeof PRIME_RLM_API_VERSION;

	// --- extension points (call at extension load time; order preserved) ---
	registerKernelBootstrapContributor(fn: KernelBootstrapContributor): void;
	registerHostHandlerProvider(fn: HostHandlerProvider): void;

	// --- registry read access (prime-comms routing, observability) ---
	listSessions(): PrimeRlmSessionInfo[];
	sessionInfo(sessionId: string): PrimeRlmSessionInfo | undefined;
	listChildren(parentSessionId: string): ChildRegistryEntry[];
	getLiveChild(parentSessionId: string, childId: string): LiveChild | undefined;
	findChildByName(parentSessionId: string, name: string): ChildRegistryEntry | undefined;
	getParentRef(sessionId: string): ParentRef | undefined;
}

// ---- module-scope implementation -------------------------------------------

const bootstrapContributors: KernelBootstrapContributor[] = [];
const hostHandlerProviders: HostHandlerProvider[] = [];

/** Wired by index.ts at load: resolves the live SessionState map. */
let resolveState: (sessionId: string) => SessionState | undefined = () => undefined;
let allStates: () => SessionState[] = () => [];

/** Internal: called once from index.ts (module init) to bind the registry. */
export function _bindRegistryAccessors(
	getState: (sessionId: string) => SessionState | undefined,
	getAll: () => SessionState[],
): void {
	resolveState = getState;
	allStates = getAll;
}

function sessionInfoOf(state: SessionState): PrimeRlmSessionInfo | undefined {
	const ctx = state.ctx;
	if (!ctx) return undefined;
	return {
		sessionId: state.sessionId,
		sessionDir: ctx.sessionManager.getSessionDir(),
		cwd: ctx.cwd,
		agentDir: process.env.PI_CODING_AGENT_DIR ?? "",
		depth: state.depth,
	};
}

const api: PrimeRlmHostApi = {
	version: PRIME_RLM_API_VERSION,

	registerKernelBootstrapContributor(fn) {
		bootstrapContributors.push(fn);
	},
	registerHostHandlerProvider(fn) {
		hostHandlerProviders.push(fn);
	},

	listSessions() {
		return allStates()
			.map(sessionInfoOf)
			.filter((info): info is PrimeRlmSessionInfo => info !== undefined);
	},
	sessionInfo(sessionId) {
		const state = resolveState(sessionId);
		return state ? sessionInfoOf(state) : undefined;
	},
	listChildren(parentSessionId) {
		const state = resolveState(parentSessionId);
		return state ? [...state.children.values()] : [];
	},
	getLiveChild(parentSessionId, childId) {
		const state = resolveState(parentSessionId);
		const entry = state?.children.get(childId);
		const session = state?.liveChildren.get(childId);
		if (!state || !entry || !session) return undefined;
		return { entry, session };
	},
	findChildByName(parentSessionId, name) {
		return this.listChildren(parentSessionId).find((c) => c.session_name === name);
	},
	getParentRef(sessionId) {
		return resolveState(sessionId)?.parentRef;
	},
};

/** Internal: collect bootstrap contributions for a session (used by ipython-tool). */
export function collectBootstrapContributions(info: PrimeRlmSessionInfo): KernelBootstrapContribution[] {
	const out: KernelBootstrapContribution[] = [];
	for (const fn of bootstrapContributors) {
		try {
			const contribution = fn(info);
			if (contribution) out.push(contribution);
		} catch (error) {
			out.push({
				python: `_PRIME_BOOTSTRAP_CONTRIBUTOR_ERROR = ${JSON.stringify(
					`bootstrap contributor threw: ${error instanceof Error ? error.message : String(error)}`,
				)}`,
			});
		}
	}
	return out;
}

/** Internal: collect host handlers from providers for a session. */
export function collectHostHandlers(info: PrimeRlmSessionInfo): HostRequestHandlers {
	let merged: HostRequestHandlers = {};
	for (const fn of hostHandlerProviders) {
		try {
			const handlers = fn(info);
			if (handlers) merged = { ...merged, ...handlers };
		} catch {
			// a throwing provider loses its handlers; core rlm.* handlers stay intact
		}
	}
	return merged;
}

// Self-publish at module evaluation so siblings can resolve the API even before
// any session factory runs. Re-publication is idempotent.
(globalThis as Record<symbol, unknown>)[PRIME_RLM_API_GLOBAL_KEY] = api;

/** Accessor for sibling extensions (also usable from within prime-rlm). */
export function getHostApi(): PrimeRlmHostApi | undefined {
	const candidate = (globalThis as Record<symbol, unknown>)[PRIME_RLM_API_GLOBAL_KEY];
	return candidate as PrimeRlmHostApi | undefined;
}
