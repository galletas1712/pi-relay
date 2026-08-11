// Verification: V1 (row counts + per-session active-branch chain hashes),
// V3 helpers (output tree snapshot/compare for idempotency).
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { canonicalSourceContent } from "./convert.ts";
import type { EntryRow, SessionRow } from "./pgsource.ts";
import { sessionUuid } from "./ids.ts";
import { fileStamp } from "./sessions.ts";
import type { MigratorConfig } from "./config.ts";

/** stable stringify: object keys sorted recursively (chain-hash canonical form) */
export function stable(v: unknown): string {
	if (Array.isArray(v)) return `[${v.map(stable).join(",")}]`;
	if (v !== null && typeof v === "object") {
		const o = v as Record<string, unknown>;
		return `{${Object.keys(o)
			.sort()
			.map((k) => `${JSON.stringify(k)}:${stable(o[k])}`)
			.join(",")}}`;
	}
	return JSON.stringify(v);
}

export function chainHash(contents: unknown[]): string {
	const h = createHash("sha256");
	for (const c of contents) h.update(createHash("sha256").update(stable(c), "utf8").digest());
	return h.digest("hex");
}

/** canonical model-visible content of ONE emitted v3 entry (mirrors canonicalSourceContent) */
export function canonicalEmittedContent(e: Record<string, unknown>): unknown | null {
	const ts = typeof e.timestamp === "string" ? Date.parse(e.timestamp) : 0;
	if (e.type === "message") {
		const m = e.message as Record<string, unknown>;
		const ms = Number(m.timestamp ?? ts);
		if (m.role === "user") {
			const blocks = (m.content as Array<Record<string, unknown>>) ?? [];
			return ["user", ms, blocks.map((b) => (b.type === "text" ? ["text", String(b.text)] : ["image", JSON.stringify(b)]))];
		}
		if (m.role === "assistant") {
			const content = (m.content as Array<Record<string, unknown>>) ?? [];
			return [
				"assistant",
				ms,
				content.map((cb) =>
					cb.type === "toolCall" ? ["toolCall", String(cb.id), String(cb.name), canonArgsEmitted(cb.arguments)] : ["text", String(cb.text ?? "")],
				),
			];
		}
		if (m.role === "toolResult") {
			const content = (m.content as Array<Record<string, unknown>>) ?? [];
			const text = content
				.filter((b) => b.type === "text")
				.map((b) => String(b.text ?? ""))
				.join("");
			const status = (m.details as Record<string, unknown> | undefined)?.pi_relay_status ?? (m.isError === true ? "Error" : "Success");
			return ["toolResult", ms, String(m.toolCallId ?? ""), String(m.toolName ?? ""), text, String(status)];
		}
		return null;
	}
	if (e.type === "custom_message" && e.customType === "pi-relay-daemon-observation") {
		const d = (e.details as Record<string, unknown> | undefined)?.pi_relay as Record<string, unknown> | undefined;
		return ["daemonObservation", ts, String(d?.tool_name ?? ""), String(d?.summary ?? ""), String(d?.status ?? "")];
	}
	if (e.type === "compaction") {
		return ["compaction", ts, String(e.summary ?? ""), Number(e.tokensBefore ?? 0)];
	}
	return null; // marker, custom provenance, model_change, thinking_level_change, ...
}

function canonArgsEmitted(args: unknown): string {
	if (args !== null && typeof args === "object" && !Array.isArray(args)) {
		const keys = Object.keys(args as Record<string, unknown>);
		if (keys.length === 1 && keys[0] === "raw") return JSON.stringify((args as Record<string, unknown>).raw);
		if (keys.length === 1 && keys[0] === "value") return JSON.stringify((args as Record<string, unknown>).value);
	}
	return JSON.stringify(args);
}

/** walk an emitted v3 file from its leaf (last id line) collecting canonical contents */
export function emittedActiveContents(file: string): unknown[] {
	const raw = readFileSync(file, "utf8");
	const byId = new Map<string, Record<string, unknown>>();
	let leaf: string | null = null;
	for (const line of raw.split("\n")) {
		if (!line.includes("\"id\"")) continue;
		let rec: Record<string, unknown>;
		try {
			rec = JSON.parse(line);
		} catch {
			continue;
		}
		if (typeof rec.id === "string" && rec.id && rec.type !== "session") {
			byId.set(rec.id, rec);
			leaf = rec.id;
		}
	}
	const path: Record<string, unknown>[] = [];
	const seen = new Set<string>();
	for (let cur = leaf; cur; ) {
		if (seen.has(cur)) break;
		seen.add(cur);
		const e = byId.get(cur);
		if (!e) break;
		path.push(e);
		cur = typeof e.parentId === "string" ? e.parentId : null;
	}
	path.reverse();
	const out: unknown[] = [];
	for (const e of path) {
		const c = canonicalEmittedContent(e);
		if (c !== null) out.push(c);
	}
	return out;
}

/** source-side canonical contents of the active branch (compaction hops, same-session only) */
export function sourceActiveContents(sessionId: string, rows: EntryRow[], activeLeafId: string | null): unknown[] {
	const byId = new Map(rows.map((r) => [r.id, r]));
	const path: EntryRow[] = [];
	const seen = new Set<string>();
	let cur = activeLeafId ?? (rows.length > 0 ? rows[rows.length - 1].id : null);
	while (cur && !seen.has(cur)) {
		seen.add(cur);
		const r = byId.get(cur);
		if (!r) break;
		path.push(r);
		if ((r.item.type as string) === "compaction_summary") {
			cur = r.item.source_session_id === sessionId ? ((r.item.source_leaf_id as string) ?? null) : null;
		} else {
			cur = r.parent_id;
		}
	}
	path.reverse();
	const out: unknown[] = [];
	for (const r of path) {
		const c = canonicalSourceContent(r.item, Number(r.timestamp_ms));
		if (c !== null) out.push(c);
	}
	return out;
}

export interface V1SessionResult {
	old: string;
	new: string;
	ok: boolean;
	sourceLen: number;
	emittedLen: number;
	sourceHash: string;
	emittedHash: string;
	error?: string;
}

export function verifySession(cfg: MigratorConfig, s: SessionRow, rows: EntryRow[]): V1SessionResult {
	const newId = sessionUuid(s.id);
	const file = join(cfg.outRoot, "sessions", `${fileStamp(s.created_at)}_${newId}.jsonl`);
	const src = sourceActiveContents(s.id, rows, s.active_leaf_id);
	const base = { old: s.id, new: newId, sourceLen: src.length, sourceHash: chainHash(src) };
	if (!existsSync(file)) return { ...base, ok: false, emittedLen: 0, emittedHash: "", error: "missing file" };
	const em = emittedActiveContents(file);
	const emittedHash = chainHash(em);
	const ok = em.length === src.length && emittedHash === base.sourceHash;
	return { ...base, ok, emittedLen: em.length, emittedHash, ...(ok ? {} : { error: "chain mismatch" }) };
}

// ---- V3 tree snapshot -----------------------------------------------------------

export function treeSnapshot(rootDir: string): Record<string, string> {
	const out: Record<string, string> = {};
	const walk = (dir: string): void => {
		for (const name of readdirSync(dir).sort()) {
			const p = join(dir, name);
			if (statSync(p).isDirectory()) walk(p);
			else out[relative(rootDir, p)] = createHash("sha256").update(readFileSync(p)).digest("hex");
		}
	};
	walk(rootDir);
	// migration-report.json carries run_at/durations (a run log, not an artifact)
	delete out["migration-report.json"];
	return out;
}

export function treeCompare(a: Record<string, string>, b: Record<string, string>): { added: string[]; removed: string[]; changed: string[] } {
	const added = Object.keys(b).filter((k) => !(k in a));
	const removed = Object.keys(a).filter((k) => !(k in b));
	const changed = Object.keys(a).filter((k) => k in b && a[k] !== b[k]);
	return { added, removed, changed };
}
