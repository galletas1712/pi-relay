// M11a: subagent.transcript — drill-down into an rlm child's own session
// file (contract v0.2, additive). Reuses the M8 rebuildTranscript JSONL v3
// tree-walk for message/tool blocks and folds repl_cell/repl_output custom
// entries for the console view. Load-on-open; the UI offers refresh.
import * as agentfiles from "../agentfiles.ts";
import { rebuildTranscript } from "../transcript.ts";
import * as supervisor from "../supervisor.ts";
import { BridgeError } from "../supervisor.ts";
import { requireString, type MethodTable, type Params } from "./common.ts";

export function subagentMethods(): MethodTable {
	return {
		async "subagent.transcript"(_conn, params: Params) {
			const sessionId = requireString(params, "sessionId");
			const childId = requireString(params, "childId");
			// Child identity comes from the parent's lifecycle view (session file
			// + spool merge, same as subagent.tree) — unknown childId → 404-ish.
			const tree = (await supervisor.subagentTree(sessionId)) as { children: Array<{ rlmChildId: string; childSessionId: string | null }> };
			const child = tree.children.find((c) => c.rlmChildId === childId);
			if (!child) throw new BridgeError("subagent_not_found", `session ${sessionId} has no child ${childId}`);
			const { sessionFile } = await agentfiles.sessionFileInfo(sessionId);
			const childFile = agentfiles.resolveChildSessionFile(sessionFile, childId, child.childSessionId ?? null);
			if (!childFile) {
				throw new BridgeError("subagent_not_found", `child ${childId} has no session file yet (admitted but not started?)`);
			}
			return {
				sessionId,
				childId,
				childSessionId: child.childSessionId ?? null,
				sessionFile: childFile,
				blocks: rebuildTranscript(childFile),
				replCells: agentfiles.replCellsFromFile(childFile),
			};
		},
	};
}
