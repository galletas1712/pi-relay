// Module-level per-session state. The extension factory is re-executed for every
// AgentSession in this process (parent + in-process RLM children), but the module
// itself is cached by pi's extension loader — so all cross-session state lives
// here, keyed by sessionId. Never capture one session's ctx/state in another
// session's handlers.

import type { AgentSession, ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { KernelProvisioner } from "./ipython-tool.ts";
import type { SessionRoleSpec } from "./roles.ts";

export interface ChildRegistryEntry {
	rlm_child_id: string;
	session_name: string;
	session_dir: string;
	session_id: string | null;
	status: "running" | "completed" | "error";
	created_at: string;
	/** Requested "provider/model" override, if the spawn passed one. */
	model?: string;
	/** M7: subagent role name attached at spawn (pi-relay session metadata
	 * role_name). Surfaced in list_subagents payloads + agent_message tree rows. */
	role?: string;
	result_preview?: string;
	error?: string;
}

/** Link from a child session back to the session that spawned it via rlm.run. */
export interface ParentRef {
	parentSessionId: string;
	childId: string;
	name: string;
}

export interface SessionState {
	sessionId: string;
	depth: number;
	provisioner?: KernelProvisioner;
	children: Map<string, ChildRegistryEntry>;
	/** M2: child AgentSessions are RETAINED after they settle so sibling
	 * extensions (prime-comms) can re-prompt them. Disposed on this session's
	 * session_shutdown. */
	liveChildren: Map<string, AgentSession>;
	parentRef?: ParentRef;
	/** Latest ExtensionContext seen for this session (events re-supply it). */
	ctx?: ExtensionContext;
	/** This session's ExtensionAPI (set at session_start) — used by the rlm-only
	 * fallback path to inject child results directly into this session. */
	pi?: ExtensionAPI;
	/** M7: role snapshot when this session was spawned with rlm.run(role=...);
	 * consumed from pendingRole at first getSessionState (session_start). */
	role?: SessionRoleSpec;
}

const sessions = new Map<string, SessionState>();

/** Depth of a child session, registered by the parent's rlm.run handler BEFORE
 * createAgentSession runs the extension factory for that child. */
const pendingDepth = new Map<string, number>();
/** Parent linkage of a child session, registered alongside pendingDepth. */
const pendingParent = new Map<string, ParentRef>();
/** M7: role snapshot of a role-configured child, registered alongside
 * pendingDepth BEFORE the child's bindExtensions runs session_start. */
const pendingRole = new Map<string, SessionRoleSpec>();

export function registerPendingRole(sessionId: string, role: SessionRoleSpec): void {
	pendingRole.set(sessionId, role);
}

export function registerPendingDepth(sessionId: string, depth: number): void {
	pendingDepth.set(sessionId, depth);
}

export function registerPendingParent(sessionId: string, ref: ParentRef): void {
	pendingParent.set(sessionId, ref);
}

export function getSessionState(sessionId: string): SessionState {
	let state = sessions.get(sessionId);
	if (!state) {
		const depth = pendingDepth.get(sessionId) ?? 0;
		pendingDepth.delete(sessionId);
		state = {
			sessionId,
			depth,
			children: new Map(),
			liveChildren: new Map(),
			parentRef: pendingParent.get(sessionId),
			role: pendingRole.get(sessionId),
		};
		pendingParent.delete(sessionId);
		pendingRole.delete(sessionId);
		sessions.set(sessionId, state);
	}
	return state;
}

export function peekSessionState(sessionId: string): SessionState | undefined {
	return sessions.get(sessionId);
}

export function listSessionStates(): SessionState[] {
	return [...sessions.values()];
}

export async function disposeSessionState(sessionId: string): Promise<void> {
	const state = sessions.get(sessionId);
	if (!state) return;
	sessions.delete(sessionId);
	// Dispose retained children first (their kernels are children of this host).
	for (const child of state.liveChildren.values()) {
		try {
			child.dispose();
		} catch {
			// best effort
		}
	}
	state.liveChildren.clear();
	try {
		await state.provisioner?.dispose();
	} catch {
		// best effort
	}
	state.children.clear();
}
