// M9: repl.execute — per-session IPython console (contract v0.1, additive).
// Thin route: validation + idempotency; the forward path lives in
// supervisor.replExecute (sentinel rpc prompt → prime-rlm input interception).
import { randomUUID } from "node:crypto";
import * as db from "../db.ts";
import * as supervisor from "../supervisor.ts";
import { BridgeError } from "../supervisor.ts";
import { checkIdem, requireString, type MethodTable, type Params } from "./common.ts";

const CODE_MAX = 1024 * 1024;
const CLIENT_CELL_ID_MAX = 200;

/** Deterministic cell id for idempotent retries: u_<sanitized client_cell_id>. */
function sanitizeClientCellId(v: string): string {
	return v.replace(/[^a-zA-Z0-9_-]+/g, "_").slice(0, 80);
}

export function replMethods(): MethodTable {
	return {
		async "repl.execute"(_conn, params: Params) {
			const sessionId = requireString(params, "sessionId");
			const code = requireString(params, "code");
			if (code.length > CODE_MAX) throw new BridgeError("bad_request", `params.code exceeds ${CODE_MAX} chars`);
			let clientCellId: string | undefined;
			if (params.client_cell_id !== undefined && params.client_cell_id !== null) {
				clientCellId = requireString(params, "client_cell_id");
				if (clientCellId.length > CLIENT_CELL_ID_MAX) {
					throw new BridgeError("bad_request", `params.client_cell_id exceeds ${CLIENT_CELL_ID_MAX} chars`);
				}
			}
			// Idempotent on client_cell_id: replay a stored response; the
			// deterministic cell_id + host-side dedupe cover crash-mid-flight.
			const idemKey = clientCellId ? `repl:${sessionId}:${clientCellId}` : undefined;
			const replay = await checkIdem(idemKey, "repl.execute", { sessionId, code, client_cell_id: clientCellId });
			if (replay) return { ...(replay.replay as Record<string, unknown>), replay: true };
			const cellId = clientCellId ? `u_${sanitizeClientCellId(clientCellId)}` : `u_${randomUUID()}`;
			const res = await supervisor.replExecute(sessionId, cellId, code, clientCellId);
			const response = { accepted: res.accepted, cell_id: res.cell_id };
			if (idemKey) {
				await db.idemPut({
					key: idemKey,
					method: "repl.execute",
					sessionId,
					paramsHash: db.paramsHash({ sessionId, code, client_cell_id: clientCellId }),
					response,
				});
			}
			return response;
		},
	};
}
