// Per-session and global state directory resolution for prime-harness (M2).
//
// Layout (documented simplification of prime-agent):
//   PA has per-session "artifact dirs"; upstream pi's SessionManager only has a
//   per-storage sessionDir (shared across root sessions in the same host). So
//   M2 defines a per-session state root:
//     <sessionDir>/prime/<sessionId>/
//       harness/harness_state.json   local harness store (PA-identical schema)
//       refine-results.jsonl         /refine command results (observability)
//       prompt-builds.jsonl          debug log of system-prompt assemblies (H3)
//   Global store (PA-identical): <agentDir>/harness/harness_state.json
//
// This module is duplicated (not shared) in prime-comms with the comms-specific
// parts added; the two packages intentionally share no code.

import { join } from "node:path";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";

export const HARNESS_STATE_DIR_NAME = "harness";
export const HARNESS_STATE_FILE_NAME = "harness_state.json";

/** Per-session state root: <sessionDir>/prime/<sessionId>. */
export function sessionStateDir(ctx: ExtensionContext): string {
	return join(ctx.sessionManager.getSessionDir(), "prime", ctx.sessionManager.getSessionId());
}

/** Local harness store dir for this session: <sessionStateDir>/harness. */
export function localHarnessDir(ctx: ExtensionContext): string {
	return join(sessionStateDir(ctx), HARNESS_STATE_DIR_NAME);
}

/** Global harness store dir (PA layout): <agentDir>/harness. */
export function globalHarnessDir(agentDir: string): string {
	return join(agentDir, HARNESS_STATE_DIR_NAME);
}
