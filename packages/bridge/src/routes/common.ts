// Shared types + param helpers for contract method route modules (M8).
// server.ts composes tables from routes/*.ts via mergeMethodTables; a new
// surface (e.g. M9 repl.*) is one more module + one more spread.
import type { WebSocket } from "ws";
import * as db from "../db.ts";
import { BridgeError } from "../supervisor.ts";

export interface Subscription {
	watermark: number;
}

export interface Conn {
	id: number;
	socket: WebSocket;
	isAlive: boolean;
	subs: Map<string, Subscription>; // sessionId -> subscription
}

export type Params = Record<string, unknown>;
export type MethodHandler = (conn: Conn, params: Params) => Promise<unknown>;
export type MethodTable = Record<string, MethodHandler>;

export function requireString(p: Params, name: string): string {
	const v = p[name];
	if (typeof v !== "string" || v === "") throw new BridgeError("bad_request", `params.${name} must be a non-empty string`);
	return v;
}

/** Compose route tables; a duplicate method name is a boot-time bug, so fail loud. */
export function mergeMethodTables(...tables: MethodTable[]): MethodTable {
	const out: MethodTable = {};
	for (const table of tables) {
		for (const [name, handler] of Object.entries(table)) {
			if (name in out) throw new Error(`duplicate contract method: ${name}`);
			out[name] = handler;
		}
	}
	return out;
}

/** Broadcast fn injected by server.ts (it owns the connection set). */
export type McpAuthBroadcast = (server: string, status: string, scopes?: string[], detail?: string) => void;

export async function checkIdem(key: string | undefined, method: string, params: Params): Promise<{ replay: unknown } | null> {
	if (!key) return null;
	const hit = await db.idemGet(key);
	if (!hit) return null;
	if (hit.method !== method || hit.params_hash !== db.paramsHash(params)) {
		throw new BridgeError("idempotency_conflict", `idempotency key ${key} was used with different params`, {
			originalMethod: hit.method,
		});
	}
	return { replay: hit.response };
}
