// M11b: fork/switch branch-file surgery.
//
// pi's rpc fork/clone REBINDS the live host to the branched file, which fights
// the bridge's sessionId↔host↔sessionFile invariants (and strands the parent
// hostless). The bridge instead performs the same branch operation as pure
// file surgery — copy the leaf→root entry path into a new session file — and
// keeps full control of session ids (the header id IS the bridge session id,
// so the bridge id === pi session id invariant survives forking). Composition
// with workspace-lib (btrfs snapshot of the parent cwd) lives in the
// supervisor (forkSession/switchSession).
//
// File format verified against pi 0.84.1 SessionManager (dist/core/
// session-manager.js): header {type:"session",version:3,id,timestamp,cwd,
// parentSession?}; tree entries carry id/parentId/timestamp; branches are
// entry paths, never re-chained (we copy the path verbatim, keeping label and
// compaction entries so context rebuild matches the source view).
import { existsSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { readSessionRecords, type SessionRecord } from "./transcript.ts";

function isTreeEntry(rec: SessionRecord): boolean {
	return rec.type !== "session" && typeof rec.id === "string" && rec.id !== "";
}

/** Branch tip: last tree entry in file order (file append order = branch tip). */
export function lastLeafId(records: SessionRecord[]): string | null {
	let leaf: string | null = null;
	for (const rec of records) {
		if (isTreeEntry(rec)) leaf = rec.id!;
	}
	return leaf;
}

/** Walk leaf→root via parentId, reversed into chronological order. Stops on
 * cycles/missing parents rather than looping (defensive; files are append-only). */
export function branchPathTo(records: SessionRecord[], leafId: string | null): SessionRecord[] {
	if (!leafId) return [];
	const byId = new Map<string, SessionRecord>();
	for (const rec of records) {
		if (isTreeEntry(rec)) byId.set(rec.id!, rec);
	}
	const branch: SessionRecord[] = [];
	const seen = new Set<string>();
	for (let cur: string | null = leafId; cur; ) {
		if (seen.has(cur)) break;
		seen.add(cur);
		const entry = byId.get(cur);
		if (!entry) break;
		branch.push(entry);
		cur = typeof entry.parentId === "string" ? entry.parentId : null;
	}
	branch.reverse();
	return branch;
}

export function userMessageText(entry: SessionRecord): string {
	const content = entry.message?.content ?? [];
	return content
		.filter((c) => c?.type === "text")
		.map((c) => String(c.text ?? ""))
		.join("");
}

/** M11b: text of any message record (userMessageText generalized — the
 * session.getEntries read serves arbitrary roles). */
export function messageText(entry: SessionRecord): string {
	return userMessageText(entry);
}

export interface ForkPoint {
	entryId: string;
	parentId: string | null;
	timestamp: string | null;
	preview: string;
	onActiveBranch: boolean;
}

/** User-message boundaries (owner rewind rule) across the file's whole tree,
 * in file order, annotated with current-branch membership. File-order matches
 * pi's getUserMessagesForForking scope (the SessionManager tree). */
export function userMessageBoundaries(records: SessionRecord[], leafId?: string | null): ForkPoint[] {
	const leaf = leafId === undefined ? lastLeafId(records) : leafId;
	const onBranch = new Set(branchPathTo(records, leaf).map((e) => e.id));
	const out: ForkPoint[] = [];
	for (const rec of records) {
		if (!isTreeEntry(rec) || rec.type !== "message" || rec.message?.role !== "user") continue;
		const text = userMessageText(rec);
		out.push({
			entryId: rec.id!,
			parentId: typeof rec.parentId === "string" ? rec.parentId : null,
			timestamp: typeof rec.timestamp === "string" ? rec.timestamp : null,
			preview: text.length > 160 ? text.slice(0, 160) + "…" : text,
			onActiveBranch: onBranch.has(rec.id!),
		});
	}
	return out;
}

export interface WriteBranchFileOpts {
	sourceFile: string;
	targetFile: string;
	/** Branch tip to copy; null = empty branch (fresh session w/ parent pointer). */
	leafId: string | null;
	/** Session id stamped into the new header (= bridge session id). */
	sessionId: string;
	/** Recorded as header.parentSession (lineage pointer, pi convention). */
	parentSessionFile?: string | null;
}

/** Write a branched session file. Returns the entry count (header excluded).
 * Throws if the target exists (forks are create-once; idempotency replays at
 * the route layer, never by overwriting). */
export function writeBranchFile(opts: WriteBranchFileOpts): { entries: number; leafId: string | null } {
	if (existsSync(opts.targetFile)) {
		throw new Error(`branch file already exists: ${opts.targetFile}`);
	}
	const records = readSessionRecords(opts.sourceFile);
	const header = records.find((r) => r.type === "session");
	const branch = branchPathTo(records, opts.leafId);
	const newHeader = {
		type: "session",
		version: typeof header?.version === "number" ? header.version : 3,
		id: opts.sessionId,
		timestamp: new Date().toISOString(),
		// cwd is informational in pi (the host process cwd is authoritative);
		// keep the source cwd so tooling that reads headers sees the truth.
		cwd: header?.cwd ?? null,
		parentSession: opts.parentSessionFile ?? undefined,
	};
	const lines = [JSON.stringify(newHeader), ...branch.map((e) => JSON.stringify(e))];
	writeFileSync(opts.targetFile, lines.join("\n") + "\n", { flag: "wx" });
	return { entries: branch.length, leafId: opts.leafId };
}

/** pi session-file naming convention: <iso-ts with : and . dashed>_<id>.jsonl. */
export function branchFileName(sessionId: string, now = new Date()): string {
	return `${now.toISOString().replace(/[:.]/g, "-")}_${sessionId}.jsonl`;
}

export function branchFilePath(sessionDir: string, sessionId: string, now = new Date()): string {
	return path.join(sessionDir, branchFileName(sessionId, now));
}
/** Every on-disk branch file of a session: all "<ts>_<id>.jsonl" siblings in
 * the sessions dir (fork children have a different id suffix and are excluded;
 * the shared entry ids of their copied prefix never leak in). Current file
 * first, then the rest in name (≈ creation) order. */
export function sessionBranchFiles(sessionFile: string): string[] {
	const dir = path.dirname(sessionFile);
	const base = path.basename(sessionFile);
	const idx = base.indexOf("_");
	if (idx < 0 || !base.endsWith(".jsonl")) return [sessionFile];
	const suffix = base.slice(idx);
	let names: string[] = [];
	try {
		names = readdirSync(dir);
	} catch {
		return [sessionFile];
	}
	const siblings = names
		.filter((n) => n !== base && n.endsWith(suffix))
		.sort()
		.map((n) => path.join(dir, n));
	return [sessionFile, ...siblings];
}

/** Tree entries across ALL branch files, deduped by entry id (the current
 * file wins), plus the id set of the current file's active branch. The /switch
 * picker and session.getEntries must see off-branch entries; the active-branch
 * view (getState transcript) keeps reading only the current file. */
export function readAllBranchRecords(sessionFile: string): {
	records: SessionRecord[];
	activeBranchIds: Set<string>;
	/** file each entry id was first seen in (current file first) */
	fileOf: Map<string, string>;
} {
	const files = sessionBranchFiles(sessionFile);
	const active = readSessionRecords(files[0]!);
	const activeBranchIds = new Set(
		branchPathTo(active, lastLeafId(active)).map((e) => e.id!),
	);
	const seen = new Set<string>();
	const fileOf = new Map<string, string>();
	const records: SessionRecord[] = [];
	for (const file of files) {
		let recs: SessionRecord[];
		try {
			recs = file === files[0] ? active : readSessionRecords(file);
		} catch {
			continue; // a corrupt sibling must not blank the whole picker
		}
		for (const rec of recs) {
			if (!isTreeEntry(rec) || seen.has(rec.id!)) continue;
			seen.add(rec.id!);
			fileOf.set(rec.id!, file);
			records.push(rec);
		}
	}
	return { records, activeBranchIds, fileOf };
}

/** Locate the branch file containing an entry id (null if unknown). */
export function findEntryFile(sessionFile: string, entryId: string): string | null {
	for (const file of sessionBranchFiles(sessionFile)) {
		let recs: SessionRecord[];
		try {
			recs = readSessionRecords(file);
		} catch {
			continue;
		}
		if (recs.some((r) => isTreeEntry(r) && r.id === entryId)) return file;
	}
	return null;
}
